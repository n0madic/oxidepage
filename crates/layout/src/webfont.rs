//! WOFF/WOFF2 → raw sfnt decoding (Phase 7, WP-A, ADR-0008).
//!
//! A thin wrapper over the [`wuff`](https://docs.rs/wuff) crate (nicoburns'
//! pure-Rust WOFF and WOFF2 decoder, whose font patterns `fonts.rs` already
//! mirrors). We sniff the 4-byte sfnt/WOFF signature and route to the matching
//! decompressor; raw sfnt is passed through unchanged. Anything unrecognized or
//! any decode error yields `None` (non-fatal — the font simply never resolves).
//!
//! # Trust boundary
//!
//! `@font-face` bytes are network-supplied and may be hostile. wuff 0.2.5 is
//! *not* hardened against crafted input: it slices `input[0..total_compressed_
//! size]` with an attacker-controlled `u32` (out-of-bounds panic), `unwrap()`s
//! a corrupt-brotli `Result`, `unwrap()`s `num_glyphs`/`num_hmetrics`/`x_mins`
//! for a transformed `hmtx` without `glyf`/`hhea`, and `Vec::with_capacity`s the
//! declared `totalSfntSize` (up to ~4 GiB) *before* its compression-ratio guard.
//! On the page's network event handler such a panic would unwind out and crash
//! the engine, and the 4 GiB allocation is a memory-DoS.
//!
//! This module is therefore the trust boundary: [`decode_font`] (a) rejects
//! implausible declared sizes from the WOFF header before wuff runs, and (b)
//! wraps the wuff decompressors in [`std::panic::catch_unwind`] so any panic
//! that slips through becomes a clean `None` (`font-display` fallback) instead
//! of unwinding into the caller.

use std::panic::catch_unwind;

/// Upper bound on a decoded sfnt payload. The WOFF/WOFF2 header carries an
/// attacker-controlled `totalSfntSize` that wuff feeds straight into
/// `Vec::with_capacity` before any ratio guard, so a 4 GiB value would OOM the
/// process. 64 MiB comfortably exceeds real fonts (the largest CJK OpenType
/// faces stay well under 30 MiB) while bounding the allocation.
const MAX_DECODED_FONT_SIZE: usize = 64 * 1024 * 1024;

/// WOFF2 header length (spec §WOFF2Header): a fixed 48 bytes preceding the
/// table directory and the compressed data block.
const WOFF2_HEADER_SIZE: usize = 48;

/// WOFF1 header length (spec §WOFFHeader): a fixed 44 bytes.
const WOFF1_HEADER_SIZE: usize = 44;

/// Decodes a downloaded font resource to raw sfnt bytes.
///
/// - `wOF2` → [`wuff::decompress_woff2`] (brotli),
/// - `wOFF` → [`wuff::decompress_woff1`] (zlib),
/// - `OTTO` / `0x00010000` / `true` / `ttcf` → already sfnt, returned as-is,
/// - otherwise (or on a decode error / rejected header / caught panic) →
///   `None`.
#[must_use]
pub fn decode_font(bytes: &[u8]) -> Option<Vec<u8>> {
    let signature: [u8; 4] = bytes.get(0..4)?.try_into().ok()?;
    match &signature {
        b"wOF2" => decode_woff2(bytes),
        b"wOFF" => decode_woff1(bytes),
        // Raw sfnt: TrueType (0x00010000), OpenType/CFF ("OTTO"), the Apple
        // TrueType tag ("true"), and TrueType collections ("ttcf").
        b"OTTO" | b"true" | b"ttcf" | [0x00, 0x01, 0x00, 0x00] => Some(bytes.to_vec()),
        _ => None,
    }
}

/// Reads a big-endian `u32` at `offset`, or `None` when the slice is too short.
fn be_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_be_bytes(slice.try_into().ok()?))
}

/// Reads a big-endian `u16` at `offset`, or `None` when the slice is too short.
fn be_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let slice = bytes.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_be_bytes(slice.try_into().ok()?))
}

/// Size of one WOFF1 table-directory entry (spec §TableDirectoryEntry).
const WOFF1_DIR_ENTRY_SIZE: usize = 20;

/// Validates a WOFF1 table directory against the decode budget.
///
/// The header's `totalSfntSize` is *not* the only attacker-controlled size:
/// `decompress_woff1` inflates each table with `Vec::with_capacity(origLength)`,
/// a per-table `u32` it never cross-checks. A 200-byte blob declaring
/// `origLength = 0xFFFFFFFF` therefore asks for a ~4 GiB allocation before any
/// ratio guard runs — and an allocation failure aborts rather than unwinds, so
/// `catch_unwind` cannot contain it. Reject such directories up front.
fn woff1_tables_within_budget(bytes: &[u8]) -> bool {
    let Some(num_tables) = be_u16(bytes, 12) else {
        return false;
    };
    let mut total: usize = 0;
    for index in 0..usize::from(num_tables) {
        let Some(entry) = index
            .checked_mul(WOFF1_DIR_ENTRY_SIZE)
            .and_then(|o| o.checked_add(WOFF1_HEADER_SIZE))
        else {
            return false;
        };
        let (Some(offset), Some(comp_length), Some(orig_length)) = (
            be_u32(bytes, entry + 4),
            be_u32(bytes, entry + 8),
            be_u32(bytes, entry + 12),
        ) else {
            return false;
        };
        // The compressed block must lie inside the bytes we actually hold.
        if (offset as usize)
            .checked_add(comp_length as usize)
            .is_none_or(|end| end > bytes.len())
        {
            return false;
        }
        let Some(sum) = total.checked_add(orig_length as usize) else {
            return false;
        };
        total = sum;
        if total > MAX_DECODED_FONT_SIZE {
            return false;
        }
    }
    true
}

/// Decodes a WOFF2 blob, rejecting implausible declared sizes up front and
/// containing any wuff panic. See the module docs for the trust-boundary
/// rationale.
fn decode_woff2(bytes: &[u8]) -> Option<Vec<u8>> {
    // Header (spec §WOFF2Header): totalSfntSize @16, totalCompressedSize @20.
    let total_sfnt_size = be_u32(bytes, 16)? as usize;
    let total_compressed_size = be_u32(bytes, 20)? as usize;

    // Reject an implausible decompressed size before wuff allocates it (both
    // the brotli output buffer and the reassembly buffer are sized from this).
    if total_sfnt_size > MAX_DECODED_FONT_SIZE {
        return None;
    }
    // The compressed block sits after the fixed header and the table directory,
    // so it can never be larger than the bytes we actually hold; a larger
    // declared size is exactly the out-of-bounds-slice trigger. This is a
    // conservative lower bound on the offset — `catch_unwind` below still backs
    // up the residual gap left by the (variable-length) table directory.
    if WOFF2_HEADER_SIZE.saturating_add(total_compressed_size) > bytes.len() {
        return None;
    }

    decode_caught(|| wuff::decompress_woff2(bytes).ok())
}

/// Decodes a WOFF1 blob with the same guards as [`decode_woff2`].
fn decode_woff1(bytes: &[u8]) -> Option<Vec<u8>> {
    // Header (spec §WOFFHeader): length @8, totalSfntSize @16.
    let declared_length = be_u32(bytes, 8)? as usize;
    let total_sfnt_size = be_u32(bytes, 16)? as usize;

    if total_sfnt_size > MAX_DECODED_FONT_SIZE {
        return None;
    }
    // `length` is the total WOFF file size; if it claims more than we hold the
    // table directory offsets are untrustworthy (truncated download).
    if bytes.len() < WOFF1_HEADER_SIZE || declared_length > bytes.len() {
        return None;
    }
    // Per-table `origLength` sizes an inflate buffer of its own, unchecked.
    if !woff1_tables_within_budget(bytes) {
        return None;
    }

    decode_caught(|| wuff::decompress_woff1(bytes).ok())
}

/// Runs a wuff decompressor closure inside [`catch_unwind`], mapping a caught
/// unwind (or an oversized result that slipped past the header guard) to
/// `None`. The closure borrows only `&[u8]`/owns its output, both unwind-safe.
fn decode_caught(
    decode: impl FnOnce() -> Option<Vec<u8>> + std::panic::UnwindSafe,
) -> Option<Vec<u8>> {
    catch_unwind(decode)
        .ok()
        .flatten()
        .filter(|sfnt| sfnt.len() <= MAX_DECODED_FONT_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_WOFF2: &[u8] = include_bytes!("../assets/webfont/test.woff2");

    #[test]
    fn valid_woff2_decodes_to_sfnt() {
        let sfnt = decode_font(TEST_WOFF2).expect("valid woff2 decodes");
        assert!(!sfnt.is_empty());
        assert!(sfnt.len() <= MAX_DECODED_FONT_SIZE);
    }

    #[test]
    fn truncated_woff2_returns_none() {
        // Cut the file in half: the (intact) header still declares the full
        // compressed block, which no longer fits — must reject, not panic.
        let truncated = &TEST_WOFF2[..TEST_WOFF2.len() / 2];
        assert_eq!(decode_font(truncated), None);
        // A blob too short to even hold the size fields is also rejected.
        assert_eq!(decode_font(&TEST_WOFF2[..8]), None);
    }

    #[test]
    fn oversized_declared_sizes_return_none() {
        // Oversized totalCompressedSize @20 — the OOB-slice trigger in wuff.
        let mut oversized_compressed = TEST_WOFF2.to_vec();
        oversized_compressed[20..24].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(decode_font(&oversized_compressed), None);

        // Oversized totalSfntSize @16 — the ~4 GiB `Vec::with_capacity` trigger.
        let mut oversized_sfnt = TEST_WOFF2.to_vec();
        oversized_sfnt[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(decode_font(&oversized_sfnt), None);
    }

    const TEST_WOFF: &[u8] = include_bytes!("../assets/webfont/test.woff");

    #[test]
    fn valid_woff1_decodes_to_sfnt() {
        // Control: the per-table budget check must not reject a real font.
        let sfnt = decode_font(TEST_WOFF).expect("valid woff1 decodes");
        assert!(!sfnt.is_empty());
    }

    #[test]
    fn woff1_with_an_oversized_table_orig_length_returns_none() {
        // wuff inflates each table into `Vec::with_capacity(origLength)` before
        // any ratio guard, so a single table declaring ~4 GiB would try to
        // allocate it — and an allocation failure aborts instead of unwinding,
        // so `catch_unwind` cannot save us. The header's `totalSfntSize` stays
        // small and plausible, proving the *per-table* check is what rejects it.
        let mut hostile = TEST_WOFF.to_vec();
        let orig_length = WOFF1_HEADER_SIZE + 12;
        hostile[orig_length..orig_length + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(decode_font(&hostile), None);
    }

    #[test]
    fn woff1_table_beyond_the_blob_returns_none() {
        // A table whose compressed block runs past the end of the data.
        let mut hostile = TEST_WOFF.to_vec();
        let comp_length = WOFF1_HEADER_SIZE + 8;
        hostile[comp_length..comp_length + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(decode_font(&hostile), None);
    }

    #[test]
    fn corrupt_brotli_woff2_returns_none() {
        // Keep the header (and its size fields) intact so the pre-checks pass
        // and wuff is actually invoked, but scramble the compressed data block
        // so brotli fails: wuff `unwrap()`s that Result and panics, which
        // `catch_unwind` must turn into `None` rather than a crash.
        let mut corrupt = TEST_WOFF2.to_vec();
        for byte in &mut corrupt[TEST_WOFF2.len() / 2..] {
            *byte = 0xFF;
        }
        assert_eq!(decode_font(&corrupt), None);
    }
}
