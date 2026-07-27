//! Pixel-compare reftests for the paint/raster pipeline.
//!
//! A test is any `tests/reftests/*.html` carrying `<link rel="match" href=…>`;
//! it and its reference are rendered at 800×600 (dpr 1) and compared with a
//! per-channel fuzz tolerance from `<meta name="fuzzy" content="maxDiff;total">`
//! (WPT syntax; default `0;0`). On failure the test, reference, and diff PNGs
//! are written to `target/reftest-out/`. CI fails on any mismatch.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use oxidepage_page::{Page, PageOptions, RasterImage, RasterOptions, Viewport};

use crate::wpt::extract_attr;

const VIEWPORT: Viewport = Viewport {
    width: 800.0,
    height: 600.0,
    dpr: 1.0,
};
const SETTLE: Duration = Duration::from_secs(5);

/// A parsed fuzz tolerance: max per-channel difference and max differing
/// pixel count.
#[derive(Clone, Copy, Default)]
struct Fuzzy {
    max_diff: u8,
    max_pixels: u32,
}

/// `cargo xtask reftest [--filter <substr>]`.
pub fn run(workspace: &Path, filter: Option<&str>) -> ExitCode {
    // Only the bundled Ahem font may shape text, so a reftest can never pass or
    // fail because of a platform font (see `golden::run`).
    oxidepage_page::disable_system_fonts();

    let dir = workspace.join("tests/reftests");
    let mut tests: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "html"))
            .collect(),
        Err(e) => {
            eprintln!("reftest: cannot read {}: {e}", dir.display());
            return ExitCode::FAILURE;
        }
    };
    tests.sort();

    let out_dir = workspace.join("target/reftest-out");
    let mut ran = 0usize;
    let mut failures = 0usize;

    for test_path in &tests {
        let name = stem(test_path);
        let Ok(html) = std::fs::read_to_string(test_path) else {
            continue;
        };
        let Some(ref_href) = find_match_ref(&html) else {
            continue; // a reference file, not a test
        };
        if let Some(f) = filter
            && !name.contains(f)
        {
            continue;
        }
        ran += 1;

        let ref_path = dir.join(&ref_href);
        let fuzzy = parse_fuzzy(&html);

        let test_img = match render(test_path) {
            Ok(img) => img,
            Err(e) => {
                eprintln!("reftest {name}: rendering test: {e}");
                failures += 1;
                continue;
            }
        };
        let ref_img = match render(&ref_path) {
            Ok(img) => img,
            Err(e) => {
                eprintln!("reftest {name}: rendering reference: {e}");
                failures += 1;
                continue;
            }
        };

        match compare(&test_img, &ref_img, fuzzy) {
            Ok(()) => {}
            Err((count, diff)) => {
                failures += 1;
                eprintln!(
                    "reftest {name}: MISMATCH ({count} pixels exceed fuzz {};{})",
                    fuzzy.max_diff, fuzzy.max_pixels
                );
                let _ = std::fs::create_dir_all(&out_dir);
                write_png(&out_dir.join(format!("{name}-test.png")), &test_img);
                write_png(&out_dir.join(format!("{name}-ref.png")), &ref_img);
                write_png(&out_dir.join(format!("{name}-diff.png")), &diff);
            }
        }
    }

    if failures == 0 {
        println!("reftest: {ran} reftest(s) OK");
        ExitCode::SUCCESS
    } else {
        eprintln!("reftest: {failures}/{ran} FAILED (see target/reftest-out)");
        ExitCode::FAILURE
    }
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string()
}

/// Renders an HTML file to an 800×600 RGBA image.
fn render(path: &Path) -> Result<RasterImage, String> {
    let html =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let page = Page::new(PageOptions {
        url: Some(format!("file://{}", path.display())),
        viewport: Some(VIEWPORT),
        ..PageOptions::default()
    })
    .map_err(|e| format!("page setup: {e}"))?;
    page.load_html(&html).map_err(|e| format!("load: {e}"))?;
    page.settle(SETTLE);
    Ok(page.render_pixels(&RasterOptions::default()))
}

/// Compares two images with a fuzz tolerance. On failure returns the number of
/// out-of-tolerance pixels and a diff image (magenta where they differ).
fn compare(
    test: &RasterImage,
    reference: &RasterImage,
    fuzzy: Fuzzy,
) -> Result<(), (u32, RasterImage)> {
    if test.width != reference.width || test.height != reference.height {
        return Err((u32::MAX, test.clone()));
    }
    let mut diff = RasterImage {
        width: test.width,
        height: test.height,
        rgba: vec![0; (test.width * test.height * 4) as usize],
    };
    let mut count = 0u32;
    for y in 0..test.height {
        for x in 0..test.width {
            let a = test.pixel(x, y);
            let b = reference.pixel(x, y);
            let d = (0..4).map(|i| a[i].abs_diff(b[i])).max().unwrap_or(0);
            let i = ((y * test.width + x) * 4) as usize;
            if d > fuzzy.max_diff {
                count += 1;
                diff.rgba[i..i + 4].copy_from_slice(&[255, 0, 255, 255]);
            } else {
                // Dim the matching test pixel for context.
                diff.rgba[i..i + 4].copy_from_slice(&[a[0] / 2, a[1] / 2, a[2] / 2, 255]);
            }
        }
    }
    if count > fuzzy.max_pixels {
        Err((count, diff))
    } else {
        Ok(())
    }
}

fn write_png(path: &Path, image: &RasterImage) {
    if let Ok(bytes) = oxidepage_raster_skia::encode_png(image) {
        let _ = std::fs::write(path, bytes);
    }
}

/// Finds the `href` of the first `<link rel="match">` in `html`. Quote-aware
/// (single, double, and unquoted `rel`/`href`) via the shared attribute parser.
fn find_match_ref(html: &str) -> Option<String> {
    for tag in html.split("<link").skip(1) {
        let end = tag.find('>').unwrap_or(tag.len());
        let tag = &tag[..end];
        let is_match = extract_attr(tag, "rel")
            .is_some_and(|rel| rel.split_ascii_whitespace().any(|t| t == "match"));
        if is_match {
            return extract_attr(tag, "href");
        }
    }
    None
}

/// Parses `<meta name="fuzzy" content="maxDiff;totalPixels">`; each field is a
/// number or a `lo-hi` range (the upper bound is used). Defaults to `0;0`.
fn parse_fuzzy(html: &str) -> Fuzzy {
    for tag in html.split("<meta").skip(1) {
        let end = tag.find('>').unwrap_or(tag.len());
        let tag = &tag[..end];
        if !tag.contains("fuzzy") {
            continue;
        }
        let Some(content) = extract_attr(tag, "content") else {
            continue;
        };
        // Optional "label:" prefix.
        let content = content.rsplit(':').next().unwrap_or(&content);
        let mut parts = content.split(';');
        // A per-channel difference is 0..=255; clamp rather than truncate, or
        // `maxDifference=256` would wrap to 0 and demand a pixel-exact match.
        let max_diff = u8::try_from(parts.next().map(upper_bound).unwrap_or(0)).unwrap_or(u8::MAX);
        let max_pixels = parts.next().map(upper_bound).unwrap_or(0);
        return Fuzzy {
            max_diff,
            max_pixels,
        };
    }
    Fuzzy::default()
}

/// The upper bound of a `lo-hi` range (or the value itself).
fn upper_bound(field: &str) -> u32 {
    let field = field.trim();
    let hi = field.split('-').next_back().unwrap_or(field);
    hi.trim().parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_match_ref_handles_single_quotes() {
        // LOW 5: `rel='match'` / `href='…'` (single quotes) must be parsed,
        // otherwise the test is silently treated as a reference and skipped.
        let html = "<!doctype html><link rel='match' href='ref.html'><body>x</body>";
        assert_eq!(find_match_ref(html), Some("ref.html".to_string()));
    }

    #[test]
    fn find_match_ref_handles_double_quotes() {
        let html = r#"<link rel="match" href="ref.html">"#;
        assert_eq!(find_match_ref(html), Some("ref.html".to_string()));
    }

    #[test]
    fn find_match_ref_none_for_non_match_link() {
        let html = r#"<link rel="stylesheet" href="a.css">"#;
        assert_eq!(find_match_ref(html), None);
    }

    #[test]
    fn parse_fuzzy_handles_single_quoted_content() {
        let html = "<meta name='fuzzy' content='2;100'>";
        let fuzzy = parse_fuzzy(html);
        assert_eq!(fuzzy.max_diff, 2);
        assert_eq!(fuzzy.max_pixels, 100);
    }

    #[test]
    fn parse_fuzzy_clamps_an_out_of_range_max_difference() {
        // A per-channel difference above 255 used to wrap through `as u8`:
        // `256` became `0` (demanding a pixel-exact match) and `300` became `44`.
        assert_eq!(
            parse_fuzzy("<meta name=fuzzy content='256;10'>").max_diff,
            255
        );
        assert_eq!(
            parse_fuzzy("<meta name=fuzzy content='300;10'>").max_diff,
            255
        );
        // The in-range upper bound of a range is still honored.
        assert_eq!(
            parse_fuzzy("<meta name=fuzzy content='0-3;10'>").max_diff,
            3
        );
    }
}
