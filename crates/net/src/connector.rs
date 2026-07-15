//! The SSRF-enforcing connector (design doc §8): a hand-written
//! `tower_service::Service<Uri>` that resolves DNS itself, filters *every*
//! resolved address through the policy, and connects only to vetted ones.
//!
//! Resolving in-house and filtering the resolved set — rather than trusting
//! `HttpConnector`'s resolver — is what makes IP-literal hosts and DNS names
//! go through the same gate, closing DNS-rebinding and numeric-literal
//! bypasses by construction. TLS is layered on top by [`crate::client`] with
//! `hyper-rustls`: SNI verifies the hostname while the TCP connection went to
//! the vetted IP (the SSRF-correct split).

use std::future::Future;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use http::Uri;
use hyper::rt::{Read, ReadBufCursor, Write};
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::rt::TokioIo;
use oxidepage_base::NetErrorKind;
use tokio::net::TcpStream;
use tower_service::Service;

use crate::error::NetError;
use crate::policy::ResourcePolicy;

/// A connector that vets addresses before connecting.
#[derive(Clone)]
pub struct SsrfConnector {
    policy: Arc<ResourcePolicy>,
}

impl SsrfConnector {
    #[must_use]
    pub fn new(policy: Arc<ResourcePolicy>) -> Self {
        Self { policy }
    }
}

/// A connected, vetted TCP stream. Wraps [`TokioIo`] so it speaks hyper's IO
/// traits and adds the [`Connection`] impl the client pool requires.
pub struct SsrfStream(TokioIo<TcpStream>);

impl Connection for SsrfStream {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

impl Read for SsrfStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl Write for SsrfStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write_vectored(cx, bufs)
    }
}

impl Service<Uri> for SsrfConnector {
    type Response = SsrfStream;
    type Error = NetError;
    type Future = Pin<Box<dyn Future<Output = Result<SsrfStream, NetError>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), NetError>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let policy = Arc::clone(&self.policy);
        Box::pin(async move { connect(policy, uri).await })
    }
}

/// Resolve → filter → connect-first-vetted.
async fn connect(policy: Arc<ResourcePolicy>, uri: Uri) -> Result<SsrfStream, NetError> {
    let host = uri
        .host()
        .ok_or_else(|| NetError::invalid_url(format!("missing host in `{uri}`")))?;
    // http::Uri keeps the brackets on an IPv6 literal; the resolver wants the
    // bare address.
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let port = uri.port_u16().unwrap_or(match uri.scheme_str() {
        Some("https") => 443,
        _ => 80,
    });

    // getaddrinfo blocks; keep it off the async worker.
    let lookup_host = host.clone();
    let addrs: Vec<SocketAddr> = tokio::task::spawn_blocking(move || {
        (lookup_host.as_str(), port)
            .to_socket_addrs()
            .map(|it| it.collect::<Vec<_>>())
    })
    .await
    .map_err(|e| NetError::new(NetErrorKind::Dns, format!("resolver task failed: {e}")))?
    .map_err(|e| NetError::new(NetErrorKind::Dns, format!("{host}: {e}")))?;

    // Filter the *resolved* set — this is what closes IP-literal and
    // DNS-rebinding bypasses.
    let vetted: Vec<SocketAddr> = addrs
        .into_iter()
        .filter(|a| policy.ip_allowed(a.ip()))
        .collect();
    if vetted.is_empty() {
        return Err(NetError::blocked(format!(
            "host `{host}` has no policy-permitted address (SSRF filter)"
        )));
    }

    let connect_timeout = policy.connect_timeout;
    let mut last_err: Option<io::Error> = None;
    for addr in vetted {
        // A bare `TcpStream::connect` can block indefinitely (e.g. a host that
        // accepts SYN but never completes the handshake, or a silently dropped
        // route); bound each attempt so a hung connect can never hang the fetch.
        match tokio::time::timeout(connect_timeout, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => {
                let _ = stream.set_nodelay(true);
                return Ok(SsrfStream(TokioIo::new(stream)));
            }
            Ok(Err(e)) => last_err = Some(e),
            Err(_elapsed) => {
                last_err = Some(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("connect to {addr} timed out after {connect_timeout:?}"),
                ));
            }
        }
    }
    // Surface a timeout distinctly so callers can tell a slow host from a
    // refused one.
    let kind = match &last_err {
        Some(e) if e.kind() == io::ErrorKind::TimedOut => NetErrorKind::Timeout,
        _ => NetErrorKind::Connect,
    };
    Err(NetError::new(
        kind,
        last_err
            .map(|e| format!("{host}: {e}"))
            .unwrap_or_else(|| format!("{host}: no address connected")),
    ))
}
