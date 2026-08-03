//! Standard base64, for the two commands that return bytes.
//!
//! `Page.captureScreenshot` and `Page.printToPDF` hand back their payload as a
//! base64 string, and `Network.getResponseBody` does for a binary body. That is
//! the whole requirement: standard alphabet, `=` padding, no line wrapping, no
//! URL-safe variant.
//!
//! The decoder arrived with request interception (ADR-0032):
//! `Fetch.fulfillRequest` and `Fetch.continueRequest` take a base64 body *in*.
//! It is strict — whitespace, a URL-safe character or a bad length answers
//! `None` — because the input is an untrusted frame and a lenient decoder would
//! turn a driver's typo into a silently truncated stub response.
//!
//! Twenty lines of table lookup rather than a `base64` entry in
//! `[workspace.dependencies]`, on the same reasoning as
//! `crates/paint/src/json.rs`: a dependency should carry more than a function.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encodes `bytes` as standard, padded base64.
#[must_use]
pub fn encode(bytes: &[u8]) -> String {
    // Exactly 4 characters per 3-byte group, rounded up — one allocation.
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        // A 1-byte tail encodes 2 characters plus `==`; a 2-byte tail, 3 plus `=`.
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[triple as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

/// Decodes standard, padded base64. Strict: any character outside the alphabet
/// (including whitespace), or a length that is not a multiple of four, is
/// `None` rather than a best-effort prefix.
#[must_use]
pub fn decode(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        // Padding is only legal in the final group, and only as the last one or
        // two characters.
        let padding = chunk.iter().rev().take_while(|b| **b == b'=').count();
        if padding > 2
            || (padding > 0 && !std::ptr::eq(chunk.as_ptr(), bytes[bytes.len() - 4..].as_ptr()))
        {
            return None;
        }
        let mut acc = 0u32;
        for (index, byte) in chunk.iter().enumerate() {
            let value = if index >= 4 - padding {
                0
            } else {
                u32::from(value_of(*byte)?)
            };
            acc = (acc << 6) | value;
        }
        for shift in [16, 8, 0].iter().take(3 - padding) {
            out.push(((acc >> shift) & 0xff) as u8);
        }
    }
    Some(out)
}

fn value_of(byte: u8) -> Option<u8> {
    Some(match byte {
        b'A'..=b'Z' => byte - b'A',
        b'a'..=b'z' => byte - b'a' + 26,
        b'0'..=b'9' => byte - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_rfc_4648_vectors() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg==");
        assert_eq!(encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn decodes_the_rfc_4648_vectors() {
        for (bytes, text) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(decode(text).as_deref(), Some(bytes), "decoding {text:?}");
        }
    }

    #[test]
    fn decode_round_trips_every_byte_value() {
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(decode(&encode(&all)), Some(all));
    }

    #[test]
    fn a_malformed_body_is_refused_rather_than_truncated() {
        // The input is an untrusted frame — `Fetch.fulfillRequest`'s body. A
        // lenient decoder would turn a driver's typo into a silently short stub
        // response, which is a far worse failure than an error.
        for bad in [
            "Zg=",       // not a multiple of four
            "Zm9vYmFyX", // ditto, with a trailing partial group
            "Zm 9v",     // whitespace is not skipped
            "Zm9v\n",    // nor is a trailing newline
            "Zg===",     // over-padded
            "====",      // padding only
            "Zg==Zg==",  // padding in a non-final group
            "Z-_v",      // URL-safe alphabet is a different encoding
            "Zm9v!!!!",  // outside the alphabet entirely
        ] {
            assert!(decode(bad).is_none(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn encodes_every_byte_value() {
        // Catches a sign-extension or masking slip in the high bits, which a
        // pure-ASCII test never reaches.
        let all: Vec<u8> = (0..=255u8).collect();
        let encoded = encode(&all);
        assert_eq!(encoded.len(), 344);
        assert!(encoded.starts_with("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8g"));
        assert!(encoded.ends_with("+/w=="));
        assert!(
            encoded.bytes().all(|b| ALPHABET.contains(&b) || b == b'='),
            "produced a character outside the standard alphabet"
        );
    }

    #[test]
    fn output_length_is_always_a_multiple_of_four() {
        for len in 0..64 {
            let bytes = vec![0xABu8; len];
            assert_eq!(encode(&bytes).len() % 4, 0, "length {len}");
        }
    }

    #[test]
    fn a_png_header_round_trips_to_the_expected_text() {
        // The real payload shape: `Page.captureScreenshot` hands back PNG bytes.
        assert_eq!(
            encode(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]),
            "iVBORw0KGgo="
        );
    }
}
