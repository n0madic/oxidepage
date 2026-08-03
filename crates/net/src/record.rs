//! Request/response retention: what a driver's `Network` domain reports, and
//! the bounded store `Network.getResponseBody` answers from (ADR-0030).
//!
//! Nothing here knows what CDP is. This is a network-level log with a network
//! -level vocabulary; the protocol crate renames it.
//!
//! # Why it exists at all
//!
//! The stack retained *nothing* about a request before this: `dispatch_net_event`
//! consumed each event inline and `finish` dropped the bookkeeping. So a
//! `getResponseBody` a second after the load had nothing to answer from — the
//! HTTP cache is URL-keyed, holds only RFC-9111-cacheable responses, and is
//! shared browser-wide, so it cannot stand in.
//!
//! # Why it is bounded twice
//!
//! Bodies are the memory risk, and a page can issue five hundred requests. The
//! store caps both the **number** of retained bodies and their **total bytes**,
//! evicting oldest-first, so a page that streams a gigabyte of images cannot
//! make the driver's convenience into an out-of-memory kill.

use std::collections::{HashMap, VecDeque};

use bytes::Bytes;
use oxidepage_base::RequestId;

use crate::fetch::ResourceType;
use crate::intercept::AuthChallenge;

/// Most response bodies retained per page.
pub const MAX_RETAINED_BODIES: usize = 256;

/// Most bytes of response body retained per page, across all of them.
///
/// 32 MiB is generous for the text resources a driver actually reads back
/// (documents, JSON, scripts) and small next to the 256 MiB per-page transfer
/// budget the resource policy already enforces.
pub const MAX_RETAINED_BODY_BYTES: usize = 32 * 1024 * 1024;

/// A single body larger than this is not retained at all.
///
/// Retaining one 30 MiB video would evict every useful text response behind it,
/// and no driver reads a video back through the protocol.
pub const MAX_SINGLE_BODY_BYTES: usize = 8 * 1024 * 1024;

/// What a network observer is told. Timestamps are Unix-epoch milliseconds.
#[derive(Clone, Debug)]
pub enum NetworkEvent {
    /// A request is about to go out.
    Requested {
        id: RequestId,
        url: String,
        method: String,
        headers: Vec<(String, String)>,
        /// What the request is for. Rides *this* event, not the pause below,
        /// because a driver reads `resourceType` off `requestWillBeSent`
        /// (ADR-0032 D6).
        resource_type: ResourceType,
        timestamp: f64,
    },
    /// Response headers arrived.
    Responded {
        id: RequestId,
        status: u16,
        status_text: String,
        headers: Vec<(String, String)>,
        /// The URL after redirects.
        final_url: String,
        mime_type: String,
        /// Repeated from the request, because CDP's `responseReceived` carries
        /// it too and a driver that reads it there must not be told `Other`.
        resource_type: ResourceType,
        timestamp: f64,
    },
    /// The body is complete.
    Finished {
        id: RequestId,
        encoded_len: u64,
        timestamp: f64,
    },
    /// The request failed.
    Failed {
        id: RequestId,
        error: String,
        resource_type: ResourceType,
        timestamp: f64,
    },
    /// The request is held at the pause point and needs a decision (ADR-0032).
    ///
    /// Reported **as announced** — same url, method and headers as the
    /// `Requested` that preceded it. A driver pairs the two by id and drops the
    /// pairing if they disagree, which loses the request entirely.
    Paused {
        id: RequestId,
        url: String,
        method: String,
        headers: Vec<(String, String)>,
        resource_type: ResourceType,
        timestamp: f64,
    },
    /// The server asked for credentials and the driver said it would answer
    /// (ADR-0032 D8). The response itself is held back until it does.
    AuthRequired {
        id: RequestId,
        url: String,
        challenge: AuthChallenge,
        resource_type: ResourceType,
        timestamp: f64,
    },
}

impl NetworkEvent {
    /// Whether losing this event would wedge the page rather than merely lose a
    /// milestone.
    ///
    /// A dropped `Paused` leaves a request held for the whole intercept timeout
    /// with nobody able to release it, because the announcement is the only
    /// thing that tells a driver the request exists. The event bus must not
    /// `try_send` these (ADR-0032 D2).
    ///
    /// `Requested` is here for the same reason, one step removed: a driver
    /// **pairs** `Fetch.requestPaused` with the `Network.requestWillBeSent` it
    /// already stored, and Puppeteer parks a pause whose partner never arrived
    /// in `#networkRequestIdToRequestPausedEvent` to wait for it — forever, on a
    /// request it will therefore never continue. Making only half the pair
    /// survive a full bus is the same wedge with an extra step.
    #[must_use]
    pub fn is_load_bearing(&self) -> bool {
        matches!(
            self,
            NetworkEvent::Requested { .. }
                | NetworkEvent::Paused { .. }
                | NetworkEvent::AuthRequired { .. }
        )
    }

    #[must_use]
    pub fn request_id(&self) -> RequestId {
        match self {
            NetworkEvent::Requested { id, .. }
            | NetworkEvent::Responded { id, .. }
            | NetworkEvent::Finished { id, .. }
            | NetworkEvent::Failed { id, .. }
            | NetworkEvent::Paused { id, .. }
            | NetworkEvent::AuthRequired { id, .. } => *id,
        }
    }
}

/// One retained body.
struct Body {
    bytes: Bytes,
    /// Whether the bytes are text. Decided from the `Content-Type` at retention
    /// time, because that is when the header is at hand.
    text: bool,
}

/// The per-page request log and body store.
#[derive(Default)]
pub struct RequestLog {
    bodies: HashMap<RequestId, Body>,
    /// Insertion order, for oldest-first eviction.
    order: VecDeque<RequestId>,
    retained_bytes: usize,
}

impl RequestLog {
    /// Retains `bytes` as the body of `id`, evicting as needed.
    ///
    /// A body over [`MAX_SINGLE_BODY_BYTES`] is dropped rather than retained:
    /// keeping it would evict every smaller response behind it, and nothing
    /// reads a body that size back over the protocol.
    pub fn retain(&mut self, id: RequestId, bytes: Bytes, content_type: &str) {
        if bytes.len() > MAX_SINGLE_BODY_BYTES {
            return;
        }
        self.forget(id);

        while self.order.len() >= MAX_RETAINED_BODIES
            || self.retained_bytes + bytes.len() > MAX_RETAINED_BODY_BYTES
        {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(body) = self.bodies.remove(&oldest) {
                self.retained_bytes -= body.bytes.len();
            }
        }

        self.retained_bytes += bytes.len();
        self.order.push_back(id);
        self.bodies.insert(
            id,
            Body {
                bytes,
                text: is_text(content_type),
            },
        );
    }

    /// The retained body, and whether it is text.
    ///
    /// A caller that gets `false` must base64-encode: the bytes are not valid
    /// text and handing them to a JSON string would either fail or corrupt.
    #[must_use]
    pub fn body(&self, id: RequestId) -> Option<(&[u8], bool)> {
        self.bodies
            .get(&id)
            .map(|body| (body.bytes.as_ref(), body.text))
    }

    /// Drops one retained body.
    pub fn forget(&mut self, id: RequestId) {
        if let Some(body) = self.bodies.remove(&id) {
            self.retained_bytes -= body.bytes.len();
            self.order.retain(|retained| *retained != id);
        }
    }

    pub fn clear(&mut self) {
        self.bodies.clear();
        self.order.clear();
        self.retained_bytes = 0;
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.bodies.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bodies.is_empty()
    }

    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }
}

/// Whether a MIME type names something a JSON string can carry verbatim.
///
/// Conservative on purpose: anything not recognized as text is base64-encoded,
/// which is always correct, where mis-classifying a PNG as text is not.
#[must_use]
pub fn is_text(content_type: &str) -> bool {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    mime.starts_with("text/")
        || matches!(
            mime.as_str(),
            "application/json"
                | "application/javascript"
                | "application/x-javascript"
                | "application/ecmascript"
                | "application/xml"
                | "application/xhtml+xml"
                | "image/svg+xml"
                | "application/ld+json"
        )
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(index: u32) -> RequestId {
        RequestId::from_parts(index, std::num::NonZeroU32::new(1).unwrap())
    }

    #[test]
    fn a_retained_body_reads_back() {
        let mut log = RequestLog::default();
        log.retain(id(1), Bytes::from_static(b"hello"), "text/plain");
        assert_eq!(log.body(id(1)), Some((b"hello".as_ref(), true)));
        assert_eq!(log.retained_bytes(), 5);
        assert!(log.body(id(2)).is_none());
    }

    #[test]
    fn binary_bodies_are_flagged_for_base64() {
        let mut log = RequestLog::default();
        log.retain(id(1), Bytes::from_static(b"\x89PNG"), "image/png");
        assert!(!log.body(id(1)).unwrap().1);
        // SVG is XML, so it *is* text — mis-encoding it would be a needless
        // base64 round trip for the format a driver most wants to read.
        log.retain(id(2), Bytes::from_static(b"<svg/>"), "image/svg+xml");
        assert!(log.body(id(2)).unwrap().1);
    }

    #[test]
    fn the_count_is_bounded_oldest_first() {
        let mut log = RequestLog::default();
        for index in 0..(MAX_RETAINED_BODIES as u32 + 10) {
            log.retain(id(index), Bytes::from_static(b"x"), "text/plain");
        }
        assert!(log.len() <= MAX_RETAINED_BODIES);
        // The oldest went first, the newest survived.
        assert!(log.body(id(0)).is_none());
        assert!(log.body(id(MAX_RETAINED_BODIES as u32 + 9)).is_some());
    }

    #[test]
    fn the_total_byte_budget_is_bounded() {
        let mut log = RequestLog::default();
        let chunk = Bytes::from(vec![0u8; 1024 * 1024]);
        for index in 0..64 {
            log.retain(id(index), chunk.clone(), "text/plain");
        }
        assert!(
            log.retained_bytes() <= MAX_RETAINED_BODY_BYTES,
            "retained {} bytes",
            log.retained_bytes()
        );
    }

    #[test]
    fn an_oversized_body_is_not_retained_at_all() {
        // Retaining one would evict every smaller response behind it.
        let mut log = RequestLog::default();
        log.retain(
            id(1),
            Bytes::from(vec![0u8; MAX_SINGLE_BODY_BYTES + 1]),
            "video/mp4",
        );
        assert!(log.is_empty());
        assert_eq!(log.retained_bytes(), 0);
    }

    #[test]
    fn re_retaining_the_same_id_does_not_double_count() {
        let mut log = RequestLog::default();
        log.retain(id(1), Bytes::from_static(b"aaaa"), "text/plain");
        log.retain(id(1), Bytes::from_static(b"bb"), "text/plain");
        assert_eq!(log.len(), 1);
        assert_eq!(log.retained_bytes(), 2);
        assert_eq!(log.body(id(1)).unwrap().0, b"bb");
    }

    #[test]
    fn text_detection_covers_the_types_a_driver_reads() {
        for mime in [
            "text/html",
            "text/html; charset=utf-8",
            "application/json",
            "application/vnd.api+json",
            "application/javascript",
            "image/svg+xml",
        ] {
            assert!(is_text(mime), "{mime} should be text");
        }
        for mime in ["image/png", "font/woff2", "application/octet-stream", ""] {
            assert!(!is_text(mime), "{mime} should not be text");
        }
    }
}
