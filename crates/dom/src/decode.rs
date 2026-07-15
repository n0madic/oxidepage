//! Document byte-stream decoding per the HTML Standard's encoding sniffing
//! algorithm (§13.2.3): BOM sniffing, transport-layer encoding, `<meta>`
//! prescan of the first 1024 bytes, then the windows-1252 fallback.
//!
//! Phase 1 decodes a complete byte buffer up front; incremental decoding and
//! restart-on-late-`<meta>` arrive with streaming network loads (Phase 3).

use encoding_rs::{Encoding, UTF_8, WINDOWS_1252};
use html5ever::tendril::StrTendril;

/// How sure the sniffer is about the chosen encoding (spec "confidence").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Confidence {
    Certain,
    Tentative,
}

/// The outcome of decoding a document byte stream.
pub struct DecodedInput {
    pub text: StrTendril,
    pub encoding: &'static Encoding,
    pub confidence: Confidence,
}

/// Sniffs the encoding of `bytes` and decodes them to UTF-8 text.
///
/// `transport_encoding` is the encoding the transport layer supplied
/// (e.g. an HTTP `Content-Type` charset), which outranks in-band metadata.
#[must_use]
pub fn decode_document_bytes(
    bytes: &[u8],
    transport_encoding: Option<&'static Encoding>,
) -> DecodedInput {
    let (encoding, confidence) = sniff(bytes, transport_encoding);
    // decode_with_bom_removal: the BOM (when present) selected the encoding
    // above and must not appear in the text.
    let (text, _had_errors) = encoding.decode_with_bom_removal(bytes);
    DecodedInput {
        text: StrTendril::from_slice(&text),
        encoding,
        confidence,
    }
}

fn sniff(
    bytes: &[u8],
    transport_encoding: Option<&'static Encoding>,
) -> (&'static Encoding, Confidence) {
    if let Some((encoding, _bom_len)) = Encoding::for_bom(bytes) {
        return (encoding, Confidence::Certain);
    }
    if let Some(encoding) = transport_encoding {
        return (encoding, Confidence::Certain);
    }
    if let Some(encoding) = prescan(&bytes[..bytes.len().min(1024)]) {
        return (encoding, Confidence::Tentative);
    }
    (WINDOWS_1252, Confidence::Tentative)
}

/// Spec "prescan a byte stream to determine its encoding" (§13.2.3.2),
/// without the `<meta>`-in-`<head>`-only restriction (matching browsers).
fn prescan(bytes: &[u8]) -> Option<&'static Encoding> {
    let mut pos = 0;
    while pos < bytes.len() {
        if bytes[pos..].starts_with(b"<!--") {
            // Skip the comment; the terminating "-->" may share dashes with
            // the opener per spec ("<!-->" is a complete comment).
            pos += 2;
            match find_subsequence(&bytes[pos..], b"-->") {
                Some(i) => pos += i + 3,
                None => return None,
            }
            continue;
        }
        if starts_with_ignore_case(&bytes[pos..], b"<meta")
            && matches!(
                bytes.get(pos + 5),
                Some(b'\t' | b'\n' | b'\x0c' | b'\r' | b' ' | b'/')
            )
        {
            pos += 5;
            if let Some(encoding) = prescan_meta(bytes, &mut pos) {
                return Some(encoding);
            }
            continue;
        }
        if pos + 1 < bytes.len()
            && bytes[pos] == b'<'
            && (bytes[pos + 1].is_ascii_alphabetic()
                || (bytes[pos + 1] == b'/'
                    && bytes.get(pos + 2).is_some_and(u8::is_ascii_alphabetic)))
        {
            // A start or end tag: skip its name, then consume attributes.
            pos += 1;
            while pos < bytes.len()
                && !matches!(bytes[pos], b'\t' | b'\n' | b'\x0c' | b'\r' | b' ' | b'>')
            {
                pos += 1;
            }
            while get_attribute(bytes, &mut pos).is_some() {}
            pos += 1;
            continue;
        }
        if bytes[pos] == b'<' && matches!(bytes.get(pos + 1), Some(b'!' | b'/' | b'?')) {
            // Markup declaration or bogus comment: skip to '>'.
            match bytes[pos..].iter().position(|&b| b == b'>') {
                Some(i) => pos += i + 1,
                None => return None,
            }
            continue;
        }
        pos += 1;
    }
    None
}

/// The `<meta>` attribute-processing loop of the prescan algorithm.
fn prescan_meta(bytes: &[u8], pos: &mut usize) -> Option<&'static Encoding> {
    let mut seen: Vec<Vec<u8>> = Vec::new();
    let mut got_pragma = false;
    let mut need_pragma: Option<bool> = None;
    let mut charset: Option<&'static Encoding> = None;

    while let Some((name, value)) = get_attribute(bytes, pos) {
        if seen.contains(&name) {
            continue;
        }
        seen.push(name.clone());
        match name.as_slice() {
            b"http-equiv" if value.eq_ignore_ascii_case(b"content-type") => {
                got_pragma = true;
            }
            b"content" if charset.is_none() => {
                if let Some(encoding) = extract_encoding_from_content(&value) {
                    charset = Some(encoding);
                    need_pragma = Some(true);
                }
            }
            b"charset" => {
                charset = Encoding::for_label(&value);
                need_pragma = Some(false);
            }
            _ => {}
        }
    }

    let need_pragma = need_pragma?;
    if need_pragma && !got_pragma {
        return None;
    }
    let mut encoding = charset?;
    // The prescan looked at ASCII-compatible bytes, so a UTF-16 answer is
    // self-contradictory; the spec substitutes UTF-8. x-user-defined maps to
    // windows-1252 for compatibility.
    if encoding == encoding_rs::UTF_16LE || encoding == encoding_rs::UTF_16BE {
        encoding = UTF_8;
    }
    if encoding == encoding_rs::X_USER_DEFINED {
        encoding = WINDOWS_1252;
    }
    Some(encoding)
}

/// Spec "get an attribute" (§13.2.3.2): returns lowercased name and value.
fn get_attribute(bytes: &[u8], pos: &mut usize) -> Option<(Vec<u8>, Vec<u8>)> {
    while *pos < bytes.len() && matches!(bytes[*pos], b'\t' | b'\n' | b'\x0c' | b'\r' | b' ' | b'/')
    {
        *pos += 1;
    }
    if *pos >= bytes.len() || bytes[*pos] == b'>' {
        return None;
    }

    let mut name = Vec::new();
    let mut value = Vec::new();
    loop {
        if *pos >= bytes.len() {
            return None;
        }
        match bytes[*pos] {
            b'=' if !name.is_empty() => {
                *pos += 1;
                break;
            }
            b'\t' | b'\n' | b'\x0c' | b'\r' | b' ' => {
                // Spaces after the name: only an '=' continues into a value.
                while *pos < bytes.len()
                    && matches!(bytes[*pos], b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
                {
                    *pos += 1;
                }
                if *pos >= bytes.len() {
                    return None;
                }
                if bytes[*pos] != b'=' {
                    return Some((name, value));
                }
                *pos += 1;
                break;
            }
            b'/' | b'>' => return Some((name, value)),
            c => {
                name.push(c.to_ascii_lowercase());
                *pos += 1;
            }
        }
    }

    while *pos < bytes.len() && matches!(bytes[*pos], b'\t' | b'\n' | b'\x0c' | b'\r' | b' ') {
        *pos += 1;
    }
    if *pos >= bytes.len() {
        return None;
    }

    match bytes[*pos] {
        quote @ (b'"' | b'\'') => {
            *pos += 1;
            while *pos < bytes.len() {
                let c = bytes[*pos];
                *pos += 1;
                if c == quote {
                    return Some((name, value));
                }
                value.push(c.to_ascii_lowercase());
            }
            None
        }
        b'>' => Some((name, value)),
        _ => {
            while *pos < bytes.len()
                && !matches!(bytes[*pos], b'\t' | b'\n' | b'\x0c' | b'\r' | b' ' | b'>')
            {
                value.push(bytes[*pos].to_ascii_lowercase());
                *pos += 1;
            }
            Some((name, value))
        }
    }
}

/// Spec "extract a character encoding from a meta element" over a `content`
/// attribute value (e.g. `text/html; charset=utf-8`).
fn extract_encoding_from_content(content: &[u8]) -> Option<&'static Encoding> {
    let mut search_from = 0;
    loop {
        let idx = find_subsequence_ignore_case(&content[search_from..], b"charset")? + search_from;
        let mut pos = idx + b"charset".len();
        while pos < content.len() && matches!(content[pos], b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
        {
            pos += 1;
        }
        if content.get(pos) != Some(&b'=') {
            search_from = pos.max(idx + 1);
            continue;
        }
        pos += 1;
        while pos < content.len() && matches!(content[pos], b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
        {
            pos += 1;
        }
        let value = match content.get(pos) {
            Some(&quote @ (b'"' | b'\'')) => {
                let end = content[pos + 1..].iter().position(|&b| b == quote)?;
                &content[pos + 1..pos + 1 + end]
            }
            Some(_) => {
                let end = content[pos..]
                    .iter()
                    .position(|&b| matches!(b, b'\t' | b'\n' | b'\x0c' | b'\r' | b' ' | b';'))
                    .unwrap_or(content.len() - pos);
                &content[pos..pos + end]
            }
            None => return None,
        };
        return Encoding::for_label(value);
    }
}

fn starts_with_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len() && haystack[..needle.len()].eq_ignore_ascii_case(needle)
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn find_subsequence_ignore_case(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bom_wins_over_everything() {
        let bytes = b"\xef\xbb\xbf<html>";
        let decoded = decode_document_bytes(bytes, Some(WINDOWS_1252));
        assert_eq!(decoded.encoding, UTF_8);
        assert_eq!(decoded.confidence, Confidence::Certain);
        assert_eq!(&*decoded.text, "<html>");
    }

    #[test]
    fn transport_encoding_beats_meta() {
        let bytes = b"<meta charset=\"koi8-r\"><p>hi</p>";
        let decoded = decode_document_bytes(bytes, Some(UTF_8));
        assert_eq!(decoded.encoding, UTF_8);
        assert_eq!(decoded.confidence, Confidence::Certain);
    }

    #[test]
    fn meta_charset_is_sniffed() {
        let bytes = b"<!doctype html><meta charset=windows-1251><p>\xcf\xf0\xe8\xe2\xe5\xf2";
        let decoded = decode_document_bytes(bytes, None);
        assert_eq!(decoded.encoding, encoding_rs::WINDOWS_1251);
        assert_eq!(decoded.confidence, Confidence::Tentative);
        assert!(decoded.text.contains("Привет"));
    }

    #[test]
    fn meta_content_type_pragma_is_sniffed() {
        let bytes: &[u8] =
            b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=windows-1251\">";
        let decoded = decode_document_bytes(bytes, None);
        assert_eq!(decoded.encoding, encoding_rs::WINDOWS_1251);
    }

    #[test]
    fn content_without_pragma_is_ignored() {
        let bytes: &[u8] = b"<meta content=\"text/html; charset=windows-1251\">";
        let decoded = decode_document_bytes(bytes, None);
        assert_eq!(decoded.encoding, WINDOWS_1252);
    }

    #[test]
    fn meta_inside_comment_is_ignored() {
        let bytes: &[u8] = b"<!-- <meta charset=\"koi8-r\"> -->";
        let decoded = decode_document_bytes(bytes, None);
        assert_eq!(decoded.encoding, WINDOWS_1252);
    }

    #[test]
    fn utf16_meta_maps_to_utf8() {
        let bytes: &[u8] = b"<meta charset=\"utf-16\">hello";
        let decoded = decode_document_bytes(bytes, None);
        assert_eq!(decoded.encoding, UTF_8);
    }

    #[test]
    fn default_is_windows_1252() {
        let decoded = decode_document_bytes(b"<p>plain</p>", None);
        assert_eq!(decoded.encoding, WINDOWS_1252);
        assert_eq!(decoded.confidence, Confidence::Tentative);
    }
}
