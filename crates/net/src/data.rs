//! `data:` URL decoding — the Fetch "data: URL processor".
//!
//! A `data:` URL carries its own bytes, so there is no address to resolve and
//! nothing for the SSRF filter or the cookie jar to act on. It is therefore
//! handled at the *top* of [`fetch_inner`](crate::fetch), beside `file://`,
//! rather than behind [`ResourcePolicy::scheme_allowed`]: every consumer of the
//! fetch pipeline — classic and module scripts, `<link>` stylesheets,
//! `@import`, images, fonts, `fetch`/XHR — then gets it without a special case,
//! and asynchronous consumers keep their normal `NetEvent` timing.
//!
//! Being above the gate but *outside* the redirect loop is deliberate: that
//! loop re-checks `scheme_allowed` on every hop, which is what keeps an
//! `http:` response redirecting to `data:` a network error, as Fetch requires.
//!
//! [`ResourcePolicy::scheme_allowed`]: crate::policy::ResourcePolicy::scheme_allowed

use url::{Position, Url};

use crate::error::{NetError, NetResult};

/// A decoded `data:` URL: the body and the MIME type it declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataBody {
    pub bytes: Vec<u8>,
    /// The declared MIME type *with its parameters* — the same shape a
    /// `Content-Type` header carries, so `charset=` survives for the text
    /// consumers (scripts, CSS) that decode bytes through it.
    pub content_type: String,
}

/// What a `data:` URL means when it declares no MIME type, or one that fails to
/// parse (Fetch, data: URL processor step 14).
const DEFAULT_MIME: &str = "text/plain;charset=US-ASCII";

/// Decodes a whole `data:` URL.
pub fn load_data(url: &Url) -> NetResult<DataBody> {
    // Step 2: the serialization with the fragment excluded. A `data:` URL's
    // query is part of its body; its fragment is not.
    let input = url[..Position::AfterQuery]
        .strip_prefix("data:")
        .ok_or_else(|| NetError::invalid_url(format!("not a data: URL: {url}")))?
        .to_owned();
    decode(&input).ok_or_else(|| NetError::invalid_url(format!("malformed data: URL: {url}")))
}

/// Decodes the part of a `data:` URL *after* the `data:` prefix.
///
/// Exposed separately because the page's image and `@font-face` paths hold an
/// already-serialized URL string and decode inline, without entering the fetch
/// pipeline at all.
#[must_use]
pub fn decode(input: &str) -> Option<DataBody> {
    // Steps 5–8: the MIME type runs up to the first comma; no comma is failure.
    let (mime, encoded_body) = input.split_once(',')?;
    // Step 6.
    let mut mime = mime
        .trim_matches(|c: char| c.is_ascii_whitespace())
        .to_owned();

    // Step 10: percent-decode *first*, base64 only after. The order is
    // load-bearing, not cosmetic: `Url`'s serializer percent-encodes `=`, so a
    // base64 body reaches us spelled `...ZDIgPSAndHdvJzs%3D`, and base64-first
    // rejects it on the `%`. Whole payloads can also arrive percent-encoded.
    let body: Vec<u8> = percent_encoding::percent_decode_str(encoded_body).collect();

    // Step 11.
    let body = match strip_base64_marker(&mime) {
        Some(rest) => {
            mime = rest.to_owned();
            forgiving_base64(&body)?
        }
        None => body,
    };

    // Step 12.
    if mime.starts_with(';') {
        mime.insert_str(0, "text/plain");
    }
    // Steps 13–14: there is no MIME parser here, so only the empty type — the
    // one case that certainly fails to parse — takes the default.
    if mime.is_empty() {
        mime = DEFAULT_MIME.to_owned();
    }

    Some(DataBody {
        bytes: body,
        content_type: mime,
    })
}

/// Step 11's guard and steps 11.4–11.6's removal in one: a MIME type ending in
/// `;` + optional spaces + an ASCII case-insensitive `base64` is base64-encoded,
/// and the marker is not part of the type.
fn strip_base64_marker(mime: &str) -> Option<&str> {
    let split = mime.len().checked_sub("base64".len())?;
    // `get` rather than indexing: a multi-byte tail is not a char boundary.
    if !mime.get(split..)?.eq_ignore_ascii_case("base64") {
        return None;
    }
    mime[..split].trim_end_matches(' ').strip_suffix(';')
}

/// The Infra "forgiving-base64 decode": ASCII whitespace anywhere is ignored,
/// one or two trailing `=` are stripped when the length is a multiple of four,
/// and a remainder of one character is a hard failure.
fn forgiving_base64(input: &[u8]) -> Option<Vec<u8>> {
    let mut data: Vec<u8> = input
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    if data.len().is_multiple_of(4) {
        for _ in 0..2 {
            if data.last() == Some(&b'=') {
                data.pop();
            } else {
                break;
            }
        }
    }
    if data.len() % 4 == 1 {
        return None;
    }

    let mut out = Vec::with_capacity(data.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &b in &data {
        acc = (acc << 6) | u32::from(base64_value(b)?);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

fn base64_value(b: u8) -> Option<u8> {
    Some(match b {
        b'A'..=b'Z' => b - b'A',
        b'a'..=b'z' => b - b'a' + 26,
        b'0'..=b'9' => b - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(input: &str) -> Option<String> {
        decode(input).map(|d| String::from_utf8(d.bytes).unwrap())
    }

    #[test]
    fn plain_body_is_percent_decoded() {
        let d = decode("text/javascript,d1%20%3D%20'one'%3B").unwrap();
        assert_eq!(d.bytes, b"d1 = 'one';");
        assert_eq!(d.content_type, "text/javascript");
    }

    #[test]
    fn base64_body_is_percent_decoded_before_base64() {
        // `Url`'s serializer percent-encodes the `=` padding; decoding base64
        // first would choke on the `%`.
        let d = decode("text/javascript;base64,ZDIgPSAndHdvJzs%3D").unwrap();
        assert_eq!(d.bytes, b"d2 = 'two';");
        assert_eq!(d.content_type, "text/javascript");
    }

    #[test]
    fn fully_percent_encoded_base64_body() {
        let d = decode(
            "text/javascript;base64,%5a%44%4d%67%50%53%41%6e%64%47%68%79%5a%57%55%6e%4f%77%3D%3D",
        )
        .unwrap();
        assert_eq!(d.bytes, b"d3 = 'three';");
    }

    #[test]
    fn base64_ignores_ascii_whitespace_including_percent_encoded() {
        let d = decode("text/javascript;base64,%20ZD%20Qg%0D%0APS%20An%20Zm91cic%0D%0A%207%20")
            .unwrap();
        assert_eq!(d.bytes, b"d4 = 'four';");
    }

    #[test]
    fn percent_decoding_does_not_re_enter() {
        // `%2520` is a literal `%20`, not a space: one pass only.
        assert_eq!(text("text/plain,a%2520b").unwrap(), "a%20b");
    }

    #[test]
    fn missing_comma_is_failure() {
        assert!(decode("text/plain").is_none());
        assert!(decode("").is_none());
    }

    #[test]
    fn empty_mime_takes_the_default() {
        let d = decode(",hello").unwrap();
        assert_eq!(d.bytes, b"hello");
        assert_eq!(d.content_type, DEFAULT_MIME);
        // `;base64` alone leaves an empty type behind, which also defaults.
        let d = decode(";base64,aGk=").unwrap();
        assert_eq!(d.bytes, b"hi");
        assert_eq!(d.content_type, DEFAULT_MIME);
    }

    #[test]
    fn leading_semicolon_gains_text_plain() {
        let d = decode(";charset=utf-8,hi").unwrap();
        assert_eq!(d.content_type, "text/plain;charset=utf-8");
    }

    #[test]
    fn mime_parameters_survive() {
        let d = decode("text/javascript;charset=utf-8,x").unwrap();
        assert_eq!(d.content_type, "text/javascript;charset=utf-8");
        let d = decode("text/css;charset=utf-8;base64,YQ==").unwrap();
        assert_eq!(d.content_type, "text/css;charset=utf-8");
        assert_eq!(d.bytes, b"a");
    }

    #[test]
    fn base64_marker_is_case_insensitive_and_space_tolerant() {
        assert_eq!(text("text/plain;BASE64,aGk=").unwrap(), "hi");
        assert_eq!(text("text/plain; base64,aGk=").unwrap(), "hi");
        // Only a *trailing* `;base64` is a marker; a parameter that merely
        // starts with the word is not, so the body stays literal text.
        let d = decode("text/plain;base64=1,aGk%3D").unwrap();
        assert_eq!(d.bytes, b"aGk=");
        assert_eq!(d.content_type, "text/plain;base64=1");
    }

    #[test]
    fn multibyte_mime_tail_is_not_sliced() {
        // `strip_base64_marker` must not index into the middle of a char.
        assert!(decode("текст,hi").is_some());
    }

    #[test]
    fn forgiving_base64_rules() {
        // Padding is optional.
        assert_eq!(text("x;base64,aGk").unwrap(), "hi");
        assert_eq!(text("x;base64,aGk=").unwrap(), "hi");
        // A remainder of one is a hard failure.
        assert!(decode("x;base64,aGkAA").is_none());
        // Non-alphabet characters are rejected.
        assert!(decode("x;base64,a*k=").is_none());
    }

    #[test]
    fn binary_body_round_trips() {
        let d = decode("image/png;base64,iVBORw0KGgo=").unwrap();
        assert_eq!(d.bytes, [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        assert_eq!(d.content_type, "image/png");
    }

    #[test]
    fn load_data_keeps_the_query_and_drops_the_fragment() {
        // A `data:` URL's query is body; its fragment is not.
        let url = Url::parse("data:text/plain,a?b#c").unwrap();
        assert_eq!(load_data(&url).unwrap().bytes, b"a?b");
    }

    #[test]
    fn load_data_rejects_a_malformed_url() {
        let url = Url::parse("data:text/plain").unwrap();
        assert!(load_data(&url).is_err());
    }
}
