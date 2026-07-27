//! Golden tests for the paint display list.
//!
//! Each `tests/goldens/<name>.html` fixture is laid out at a fixed 800×600
//! viewport and its display list serialized to stable JSON (floats to two
//! decimals; fonts referenced by resource ordinal). The JSON is compared to
//! the checked-in `tests/goldens/<name>.json`; `--update` regenerates them
//! (mirroring the WPT flow). CI fails on any mismatch.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use oxidepage_page::{Page, PageOptions, Viewport};

const VIEWPORT: Viewport = Viewport {
    width: 800.0,
    height: 600.0,
    dpr: 1.0,
};
const SETTLE: Duration = Duration::from_secs(5);

fn goldens_dir(workspace: &Path) -> PathBuf {
    workspace.join("tests/goldens")
}

/// Renders a fixture's display list to golden JSON.
fn render(html: &str, doc_url: String) -> Result<String, String> {
    let page = Page::new(PageOptions {
        url: Some(doc_url),
        viewport: Some(VIEWPORT),
        ..PageOptions::default()
    })
    .map_err(|e| format!("page setup: {e}"))?;
    page.load_html(html).map_err(|e| format!("load: {e}"))?;
    page.settle(SETTLE);
    Ok(page.display_list_json())
}

/// `cargo xtask golden [--update] [--filter <substr>]`.
pub fn run(workspace: &Path, update: bool, filter: Option<&str>) -> ExitCode {
    // Goldens are byte-compared across the 3-OS CI matrix, so no platform font
    // may reach shaping. Cargo feature unification keeps `layout/system_fonts`
    // compiled in regardless of what xtask declares, hence the runtime switch.
    oxidepage_page::disable_system_fonts();

    let dir = goldens_dir(workspace);
    let mut fixtures: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "html"))
            .collect(),
        Err(e) => {
            eprintln!("golden: cannot read {}: {e}", dir.display());
            return ExitCode::FAILURE;
        }
    };
    fixtures.sort();

    let out_dir = workspace.join("target/golden-out");
    let mut ran = 0usize;
    let mut failures = 0usize;
    let mut updated = 0usize;

    for html_path in &fixtures {
        let name = html_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        if let Some(f) = filter
            && !name.contains(f)
        {
            continue;
        }
        ran += 1;

        let html = match std::fs::read_to_string(html_path) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("golden {name}: cannot read fixture: {e}");
                failures += 1;
                continue;
            }
        };
        let doc_url = format!("file://{}", html_path.display());
        let actual = match render(&html, doc_url) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("golden {name}: {e}");
                failures += 1;
                continue;
            }
        };

        let golden_path = dir.join(format!("{name}.json"));
        if update {
            if let Err(e) = std::fs::write(&golden_path, &actual) {
                eprintln!("golden {name}: cannot write golden: {e}");
                failures += 1;
                continue;
            }
            updated += 1;
            continue;
        }

        // A missing golden is an explicit failure, never a silent pass: an
        // absent file must not compare equal to an empty render.
        match compare_golden(std::fs::read_to_string(&golden_path), &actual) {
            GoldenOutcome::Match => continue,
            GoldenOutcome::Missing => {
                failures += 1;
                eprintln!(
                    "golden {name}: no golden at {} (run `cargo xtask golden --update` to create it)",
                    golden_path.display()
                );
            }
            GoldenOutcome::Mismatch(expected) => {
                failures += 1;
                eprintln!("golden {name}: MISMATCH (run `cargo xtask golden --update` to accept)");
                // Write the actual output for inspection.
                let _ = std::fs::create_dir_all(&out_dir);
                let actual_path = out_dir.join(format!("{name}.actual.json"));
                if std::fs::write(&actual_path, &actual).is_ok() {
                    eprintln!("  actual written to {}", actual_path.display());
                }
                eprint!("{}", first_diff(&expected, &actual));
            }
        }
    }

    if update {
        println!("golden: updated {updated} golden(s)");
        if failures > 0 {
            eprintln!("golden: {failures} fixture(s) failed to render");
        }
        return exit_code(succeeded(failures));
    }
    if succeeded(failures) {
        println!("golden: {ran} golden(s) OK");
        exit_code(true)
    } else {
        eprintln!("golden: {failures}/{ran} FAILED");
        exit_code(false)
    }
}

/// Outcome of comparing a rendered fixture to its checked-in golden.
enum GoldenOutcome {
    Match,
    /// Differs from the golden; carries the expected content for diffing.
    Mismatch(String),
    /// The golden file was absent or unreadable — always a failure.
    Missing,
}

/// Compares a rendered display list against the golden read from disk. A read
/// error (typically a missing file) is [`GoldenOutcome::Missing`], never a pass.
fn compare_golden(golden: std::io::Result<String>, actual: &str) -> GoldenOutcome {
    match golden {
        Ok(expected) if expected == actual => GoldenOutcome::Match,
        Ok(expected) => GoldenOutcome::Mismatch(expected),
        Err(_) => GoldenOutcome::Missing,
    }
}

/// A run reports success only when nothing failed. Both the check and update
/// paths share this rule: an `--update` run that could not render or write a
/// fixture is a failure even though it wrote the goldens it could.
fn succeeded(failures: usize) -> bool {
    failures == 0
}

fn exit_code(ok: bool) -> ExitCode {
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// A compact report of the first differing line between expected and actual.
fn first_diff(expected: &str, actual: &str) -> String {
    for (i, (e, a)) in expected.lines().zip(actual.lines()).enumerate() {
        if e != a {
            return format!("  first diff at line {}:\n    - {e}\n    + {a}\n", i + 1);
        }
    }
    if expected.lines().count() != actual.lines().count() {
        return format!(
            "  line count differs: expected {}, actual {}\n",
            expected.lines().count(),
            actual.lines().count()
        );
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    #[test]
    fn compare_golden_missing_never_passes_even_for_empty_render() {
        // LOW 8: an absent golden must not compare equal to an empty display
        // list; it is a failure, not a silent pass.
        let missing = Err(Error::new(ErrorKind::NotFound, "no such file"));
        assert!(matches!(
            compare_golden(missing, ""),
            GoldenOutcome::Missing
        ));
    }

    #[test]
    fn compare_golden_matches_identical() {
        assert!(matches!(
            compare_golden(Ok("[]".to_string()), "[]"),
            GoldenOutcome::Match
        ));
    }

    #[test]
    fn compare_golden_mismatch_carries_expected() {
        match compare_golden(Ok("old".to_string()), "new") {
            GoldenOutcome::Mismatch(expected) => assert_eq!(expected, "old"),
            other => panic!("expected mismatch, got {}", variant(&other)),
        }
    }

    #[test]
    fn update_run_reports_failure_when_a_fixture_fails() {
        // MEDIUM 3: `--update` must not report success if a fixture failed to
        // render or write.
        assert!(!succeeded(1));
        assert!(succeeded(0));
    }

    fn variant(outcome: &GoldenOutcome) -> &'static str {
        match outcome {
            GoldenOutcome::Match => "Match",
            GoldenOutcome::Mismatch(_) => "Mismatch",
            GoldenOutcome::Missing => "Missing",
        }
    }
}
