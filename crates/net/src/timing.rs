//! Resource Timing: one completed fetch, as `performance.getEntriesByType
//! ('resource')` reports it (ADR-0044).
//!
//! Built from the same [`NetworkEvent`] stream a driver's `Network` domain
//! reads, so the two cannot disagree about which requests happened. The
//! recorder only pairs each request's milestones; who may see the entry, and
//! with how much detail, is decided above, where the reading document is known.

use std::collections::HashMap;

use oxidepage_base::RequestId;

use crate::fetch::ResourceType;
use crate::record::NetworkEvent;

/// A finished fetch. Times are Unix-epoch milliseconds.
#[derive(Clone, Debug, PartialEq)]
pub struct ResourceTiming {
    /// The request URL as it was asked for (Resource Timing's `name`).
    pub url: String,
    /// The URL after redirects: what the timing-allow check compares.
    pub final_url: String,
    pub resource_type: ResourceType,
    /// When the request was started.
    pub start: f64,
    /// When the response headers arrived.
    pub response_start: f64,
    /// When the body completed.
    pub end: f64,
    pub status: u16,
    pub mime_type: String,
    /// Body bytes as they arrived.
    pub encoded_len: u64,
    /// Body bytes plus the response's header block.
    pub transfer_len: u64,
    /// The response's `Timing-Allow-Origin`, if it sent one.
    pub timing_allow_origin: Option<String>,
}

impl ResourceTiming {
    /// Resource Timing's `initiatorType`, from what the request was for.
    ///
    /// `Stylesheet` is `"link"` for an `@import` too, and `Font` is `"css"`:
    /// the request carries its type, not the element or rule behind it.
    #[must_use]
    pub fn initiator_type(&self) -> &'static str {
        match self.resource_type {
            ResourceType::Stylesheet => "link",
            ResourceType::Image => "img",
            ResourceType::Media => "video",
            ResourceType::Font => "css",
            ResourceType::Script => "script",
            ResourceType::Xhr => "xmlhttprequest",
            ResourceType::Fetch => "fetch",
            ResourceType::Document | ResourceType::Other => "other",
        }
    }
}

/// A request between its `Requested` and its terminal event.
#[derive(Debug)]
struct Open {
    url: String,
    resource_type: ResourceType,
    start: f64,
    response: Option<Response>,
}

#[derive(Debug)]
struct Response {
    at: f64,
    status: u16,
    final_url: String,
    mime_type: String,
    header_len: u64,
    timing_allow_origin: Option<String>,
}

/// Pairs each request's milestones into a [`ResourceTiming`].
///
/// Document requests are not tracked: a navigation is the navigation entry's
/// business, not a resource of the document it replaces. Neither are `data:`
/// URLs, and a failed request yields nothing.
#[derive(Debug, Default)]
pub struct TimingRecorder {
    open: HashMap<RequestId, Open>,
}

impl TimingRecorder {
    /// Feeds one event; returns the entry it completed, if any.
    pub fn observe(&mut self, event: &NetworkEvent) -> Option<ResourceTiming> {
        match event {
            NetworkEvent::Requested {
                id,
                url,
                resource_type,
                timestamp,
                ..
            } => {
                // `data:` is decoded in place, not fetched, and browsers
                // report no entry for it.
                let is_data = url
                    .get(..5)
                    .is_some_and(|scheme| scheme.eq_ignore_ascii_case("data:"));
                if *resource_type != ResourceType::Document && !is_data {
                    self.open.insert(
                        *id,
                        Open {
                            url: url.clone(),
                            resource_type: *resource_type,
                            start: *timestamp,
                            response: None,
                        },
                    );
                }
                None
            }
            NetworkEvent::Responded {
                id,
                status,
                headers,
                final_url,
                mime_type,
                timing_allow_origin,
                timestamp,
                ..
            } => {
                if let Some(open) = self.open.get_mut(id) {
                    open.response = Some(Response {
                        at: *timestamp,
                        status: *status,
                        final_url: final_url.clone(),
                        mime_type: mime_type.clone(),
                        // `name: value\r\n` per field, plus the status line's
                        // share is unknown and left out.
                        header_len: headers
                            .iter()
                            .map(|(n, v)| (n.len() + v.len() + 4) as u64)
                            .sum(),
                        timing_allow_origin: timing_allow_origin.clone(),
                    });
                }
                None
            }
            NetworkEvent::Finished {
                id,
                encoded_len,
                timestamp,
            } => {
                let open = self.open.remove(id)?;
                let response = open.response?;
                Some(ResourceTiming {
                    url: open.url,
                    final_url: response.final_url,
                    resource_type: open.resource_type,
                    start: open.start,
                    response_start: response.at,
                    end: *timestamp,
                    status: response.status,
                    mime_type: response.mime_type,
                    encoded_len: *encoded_len,
                    transfer_len: *encoded_len + response.header_len,
                    timing_allow_origin: response.timing_allow_origin,
                })
            }
            NetworkEvent::Failed { id, .. } => {
                self.open.remove(id);
                None
            }
            NetworkEvent::Paused { .. } | NetworkEvent::AuthRequired { .. } => None,
        }
    }
}

/// Whether `timing_allow_origin` lets a document of `origin` (serialized, as
/// `https://a.test`) see a cross-origin entry's detailed timings: a `*` or an
/// exact origin in the comma-separated list (Resource Timing's
/// "timing allow check").
#[must_use]
pub fn timing_allowed(timing_allow_origin: Option<&str>, origin: &str) -> bool {
    timing_allow_origin.is_some_and(|list| {
        list.split(',')
            .map(str::trim)
            .any(|item| item == "*" || item == origin)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u64) -> RequestId {
        RequestId::from_parts(
            u32::try_from(n).unwrap(),
            std::num::NonZeroU32::new(1).unwrap(),
        )
    }

    fn requested(n: u64, resource_type: ResourceType) -> NetworkEvent {
        NetworkEvent::Requested {
            id: id(n),
            url: format!("https://a.test/{n}"),
            method: "GET".into(),
            headers: Vec::new(),
            resource_type,
            timestamp: 10.0,
        }
    }

    fn responded(n: u64, headers: Vec<(String, String)>) -> NetworkEvent {
        NetworkEvent::Responded {
            id: id(n),
            status: 200,
            status_text: "OK".into(),
            timing_allow_origin: crate::fetch::timing_allow_origin_of(&headers),
            headers,
            final_url: format!("https://a.test/{n}"),
            mime_type: "text/css".into(),
            resource_type: ResourceType::Stylesheet,
            timestamp: 15.0,
        }
    }

    #[test]
    fn a_finished_request_becomes_one_entry() {
        let mut rec = TimingRecorder::default();
        assert_eq!(rec.observe(&requested(1, ResourceType::Stylesheet)), None);
        assert_eq!(
            rec.observe(&responded(1, vec![("a".into(), "bc".into())])),
            None
        );
        let entry = rec
            .observe(&NetworkEvent::Finished {
                id: id(1),
                encoded_len: 100,
                timestamp: 20.0,
            })
            .expect("entry");
        assert_eq!(entry.initiator_type(), "link");
        assert_eq!(
            (entry.start, entry.response_start, entry.end),
            (10.0, 15.0, 20.0)
        );
        assert_eq!((entry.encoded_len, entry.transfer_len), (100, 107));
        assert!(rec.open.is_empty());
    }

    #[test]
    fn documents_and_failures_yield_nothing() {
        let mut rec = TimingRecorder::default();
        rec.observe(&requested(1, ResourceType::Document));
        rec.observe(&responded(1, Vec::new()));
        let done = NetworkEvent::Finished {
            id: id(1),
            encoded_len: 1,
            timestamp: 1.0,
        };
        assert_eq!(rec.observe(&done), None);
        rec.observe(&requested(2, ResourceType::Image));
        rec.observe(&NetworkEvent::Failed {
            id: id(2),
            error: "x".into(),
            resource_type: ResourceType::Image,
            timestamp: 1.0,
        });
        assert!(rec.open.is_empty());
    }

    #[test]
    fn timing_allow_origin_lists_combine_and_match_exactly() {
        let mut rec = TimingRecorder::default();
        rec.observe(&requested(1, ResourceType::Image));
        rec.observe(&responded(
            1,
            vec![
                ("Timing-Allow-Origin".into(), "https://b.test".into()),
                ("timing-allow-origin".into(), "https://c.test".into()),
            ],
        ));
        let entry = rec
            .observe(&NetworkEvent::Finished {
                id: id(1),
                encoded_len: 0,
                timestamp: 1.0,
            })
            .unwrap();
        let tao = entry.timing_allow_origin.as_deref();
        assert!(timing_allowed(tao, "https://c.test"));
        assert!(!timing_allowed(tao, "https://c.test:8443"));
        assert!(timing_allowed(Some("*"), "https://x.test"));
        assert!(!timing_allowed(None, "https://x.test"));
    }
}
