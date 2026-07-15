//! The HTTP(S) client: the SSRF connector wrapped in `hyper-rustls` TLS and
//! driven by `hyper_util`'s pooling legacy client.
//!
//! TLS uses the `ring` provider with bundled `webpki-roots` (design ADR-0004):
//! a deterministic, C-toolchain-free build across the 3-OS CI matrix.

use std::sync::Arc;

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use oxidepage_base::NetErrorKind;

use crate::connector::SsrfConnector;
use crate::error::{NetError, NetResult};
use crate::policy::ResourcePolicy;

/// The outgoing request body type. Requests are buffered (`Full`); response
/// bodies stream as [`Incoming`].
pub type RequestBody = Full<Bytes>;

type LegacyClient = Client<hyper_rustls::HttpsConnector<SsrfConnector>, RequestBody>;

/// A pooled HTTP(S) client bound to one [`ResourcePolicy`]. Cheap to clone
/// (shares the connection pool).
#[derive(Clone)]
pub struct HttpClient {
    inner: LegacyClient,
}

impl HttpClient {
    /// Builds a client whose every connection is vetted by `policy`.
    pub fn new(policy: Arc<ResourcePolicy>) -> NetResult<Self> {
        let ssrf = SsrfConnector::new(policy);
        let tls = rustls_config()?;
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_or_http()
            .enable_all_versions()
            .wrap_connector(ssrf);
        let inner = Client::builder(TokioExecutor::new())
            .pool_max_idle_per_host(6)
            .build(https);
        Ok(Self { inner })
    }

    /// Sends one request (no redirect handling — that is the fetch pipeline's
    /// job, so each hop re-enters SSRF validation).
    pub async fn send_once(&self, req: Request<RequestBody>) -> NetResult<Response<Incoming>> {
        self.inner.request(req).await.map_err(|e| classify(&e))
    }
}

/// Builds the rustls client config with the ring provider and bundled roots.
fn rustls_config() -> NetResult<rustls::ClientConfig> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| NetError::new(NetErrorKind::Tls, e.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(config)
}

/// Maps a legacy-client error to a [`NetError`], preserving a policy/SSRF
/// rejection raised by the connector (found by walking the source chain).
fn classify(err: &hyper_util::client::legacy::Error) -> NetError {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = source {
        if let Some(net) = e.downcast_ref::<NetError>() {
            return net.clone();
        }
        source = e.source();
    }
    let kind = if err.is_connect() {
        NetErrorKind::Connect
    } else {
        NetErrorKind::Io
    };
    NetError::new(kind, err.to_string())
}
