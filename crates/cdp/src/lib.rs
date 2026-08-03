//! Chrome DevTools Protocol server.
//!
//! Speaks enough CDP for Puppeteer and Playwright to drive the engine over a
//! WebSocket. It sits **above** [`oxidepage_engine`] and nothing below it knows
//! the protocol exists: a command becomes an opaque closure at the
//! [`PageHandle`](oxidepage_engine::PageHandle) boundary, exactly as ADR-0027
//! designed, so `page` never learns about sessions, targets or JSON.
//!
//! ```no_run
//! use oxidepage_cdp::{CdpServer, ServerOptions};
//! use oxidepage_engine::{Browser, BrowserOptions};
//!
//! let browser = Browser::new(BrowserOptions::default())?;
//! let server = CdpServer::start(browser, ServerOptions::default())?;
//! println!("{}", server.browser_ws_url());
//! server.wait();
//! # Ok::<_, Box<dyn std::error::Error>>(())
//! ```
//!
//! # Security
//!
//! The endpoint is **total remote control** of a process that executes
//! attacker-controlled web content. Three properties are structural rather than
//! configurable:
//!
//! * it binds `127.0.0.1` only — there is no bind-address option, because there
//!   is no other correct value;
//! * every request must carry a loopback `Host`, which is what stops a hostile
//!   page from resolving its own domain to `127.0.0.1` and driving the endpoint
//!   through the user's browser (DNS rebinding);
//! * the WebSocket path carries a 128-bit CSPRNG token, so a blind scan of the
//!   port space does not reach the protocol.
//!
//! The token is **not** a secret from anything that can issue an HTTP request
//! here and read the answer: `/json/version` publishes it, as Chrome's does,
//! because that is how `puppeteer.connect({ browserURL })` finds the socket.
//!
//! The SSRF filter in `ResourcePolicy` protects the *content* the engine loads;
//! it does not protect the *operator*. Exposing this port to a network is
//! equivalent to handing over the machine's network position.
//!
//! # Scope
//!
//! The implemented method list is an explicit allow-list (see [`domains`]).
//! Everything else answers `MethodNotFound` — P6, "absent beats fake": a driver
//! that is told a method does not exist can report a clean failure, where a stub
//! returning `{}` would leave a test asserting against something that never
//! happened.

pub mod base64;
pub mod domains;
pub mod error;
pub mod http;
pub mod message;
pub mod pump;
pub mod session;
pub mod target;
pub mod token;
pub mod transport;

use std::net::SocketAddr;
use std::sync::Arc;

use oxidepage_engine::Browser;

pub use error::{CommandResult, ProtocolError, ServeError};
pub use target::{TargetInfo, TargetRegistry};
pub use transport::Shutdown;

/// How the endpoint is brought up.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// TCP port on loopback. `0` picks a free one, which is what tests want and
    /// what makes two servers on one machine safe.
    pub port: u16,
    /// Threads for the protocol runtime. This runtime carries socket I/O only —
    /// every page runs on its own OS thread and every command on a lane thread —
    /// so it needs very few.
    pub worker_threads: usize,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            port: 0,
            // Two: one to accept and read, one to write. Commands and page work
            // never touch this runtime, so more would only add idle threads.
            worker_threads: 2,
        }
    }
}

/// A running endpoint.
///
/// Dropping it stops accepting new connections and winds the runtime down; the
/// browser it was given is *not* closed, because the caller may still own pages
/// through it.
pub struct CdpServer {
    addr: SocketAddr,
    browser_ws_url: String,
    shutdown: Arc<Shutdown>,
    runtime: Option<tokio::runtime::Runtime>,
    joined: Option<std::thread::JoinHandle<()>>,
}

impl CdpServer {
    /// Binds and starts serving on a background thread.
    ///
    /// Returns once the socket is bound, so [`CdpServer::browser_ws_url`] is
    /// immediately connectable — a caller that had to poll for readiness would
    /// be racing every test it wrote.
    pub fn start(browser: Browser, options: ServerOptions) -> Result<Self, ServeError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(options.worker_threads.max(1))
            .enable_all()
            .thread_name("cdp")
            .build()
            .map_err(ServeError::Runtime)?;

        let listener = runtime.block_on(transport::Listener::bind(browser, options.port))?;
        let addr = listener.addr();
        let browser_ws_url = listener.browser_ws_url();
        let shutdown = listener.shutdown_signal();

        let handle = runtime.handle().clone();
        let joined = std::thread::Builder::new()
            .name(String::from("cdp-accept"))
            .spawn(move || handle.block_on(listener.run()))
            .map_err(ServeError::Runtime)?;

        Ok(Self {
            addr,
            browser_ws_url,
            shutdown,
            runtime: Some(runtime),
            joined: Some(joined),
        })
    }

    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    #[must_use]
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The URL to hand a driver — this is what `puppeteer.connect` takes as
    /// `browserWSEndpoint`.
    #[must_use]
    pub fn browser_ws_url(&self) -> &str {
        &self.browser_ws_url
    }

    /// The HTTP base a driver can discover the endpoint through, i.e. what
    /// `puppeteer.connect({ browserURL })` takes.
    #[must_use]
    pub fn http_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Asks the accept loop to stop. Idempotent.
    pub fn shutdown(&self) {
        self.shutdown.trigger();
    }

    /// Blocks until the server stops — after `Browser.close`, or after
    /// [`CdpServer::shutdown`]. This is what `oxidepage serve` sits in.
    pub fn wait(mut self) {
        if let Some(joined) = self.joined.take() {
            let _ = joined.join();
        }
    }
}

impl Drop for CdpServer {
    fn drop(&mut self) {
        self.shutdown();
        if let Some(joined) = self.joined.take() {
            let _ = joined.join();
        }
        // The runtime is shut down without waiting on in-flight tasks: a
        // connection blocked on a page that is mid-load would otherwise hold the
        // drop for as long as the load takes. Detached tasks lose their sockets
        // to the OS, which is what a closing server should do anyway.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}
