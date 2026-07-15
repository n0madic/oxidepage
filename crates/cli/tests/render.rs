//! Smoke tests for `oxidepage render`: format detection from `-o`'s
//! extension, the `--format` override, and PNG/PDF/HTML output.

use std::process::Command;

fn write_temp(name: &str, contents: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(name);
    std::fs::write(&path, contents).expect("write temp html");
    path
}

fn temp_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(name)
}

const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// A page whose inline script mutates `document.title` after parse, so a
/// rendered-HTML output that shows the mutated title (not the source
/// literal) proves it serialized the live post-script DOM.
const TITLE_MUTATING_HTML: &str = r#"<!DOCTYPE html>
<html><head><title>Original</title></head><body>
<script>document.title = 'Rendered Title';</script>
</body></html>"#;

fn run_render(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_oxidepage"))
        .arg("render")
        .args(args)
        .output()
        .expect("run oxidepage render")
}

#[test]
fn png_extension_infers_png_format() {
    let input = write_temp("oxidepage-cli-render-png-in.html", "<p>hi</p>");
    let output = temp_path("oxidepage-cli-render-out.png");

    let result = run_render(&[input.to_str().unwrap(), "-o", output.to_str().unwrap()]);
    assert!(
        result.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );

    let bytes = std::fs::read(&output).expect("read png output");
    assert_eq!(&bytes[..8], &PNG_SIGNATURE);
}

#[test]
fn pdf_extension_infers_pdf_format() {
    let input = write_temp("oxidepage-cli-render-pdf-in.html", "<p>hi</p>");
    let output = temp_path("oxidepage-cli-render-out.pdf");

    let result = run_render(&[input.to_str().unwrap(), "-o", output.to_str().unwrap()]);
    assert!(
        result.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );

    let bytes = std::fs::read(&output).expect("read pdf output");
    assert!(bytes.starts_with(b"%PDF-"));
}

#[test]
fn html_extension_infers_html_format_of_post_script_dom() {
    let input = write_temp("oxidepage-cli-render-html-in.html", TITLE_MUTATING_HTML);
    let output = temp_path("oxidepage-cli-render-out.html");

    let result = run_render(&[input.to_str().unwrap(), "-o", output.to_str().unwrap()]);
    assert!(
        result.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );

    let text = std::fs::read_to_string(&output).expect("read html output");
    assert!(text.contains("<title>Rendered Title</title>"), "{text}");
    assert!(!text.contains("Original"), "{text}");
}

#[test]
fn format_flag_overrides_misleading_extension() {
    let input = write_temp("oxidepage-cli-render-override-in.html", TITLE_MUTATING_HTML);
    let output = temp_path("oxidepage-cli-render-override-out.png");

    let result = run_render(&[
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
        "--format",
        "html",
    ]);
    assert!(
        result.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );

    let bytes = std::fs::read(&output).expect("read output");
    assert_ne!(&bytes.get(..8).unwrap_or(&[]), &PNG_SIGNATURE);
    let text = String::from_utf8(bytes).expect("html output is UTF-8");
    assert!(text.contains("<title>Rendered Title</title>"), "{text}");
}

#[test]
fn unrecognized_extension_without_format_flag_fails_with_helpful_message() {
    let input = write_temp("oxidepage-cli-render-unknown-in.html", "<p>hi</p>");
    let output = temp_path("oxidepage-cli-render-out.weird");

    let result = run_render(&[input.to_str().unwrap(), "-o", output.to_str().unwrap()]);
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("--format"), "{stderr}");
}
