//! Standard base64, for the two commands that return bytes.
//!
//! `Page.captureScreenshot` and `Page.printToPDF` hand back their payload as a
//! base64 string, and `Network.getResponseBody` does for a binary body. That is
//! the whole requirement: standard alphabet, `=` padding, no line wrapping, no
//! URL-safe variant, and no decoder — nothing in the implemented surface takes
//! base64 *in*.
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
