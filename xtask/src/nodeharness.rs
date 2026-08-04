//! The Node-harness half of `cargo xtask puppeteer` and `cargo xtask
//! playwright`.
//!
//! Both runners are the same shape — install pinned dependencies, run a
//! `run.mjs` that prints `STATUS\tname[\tmessage]` lines, and diff those
//! against a two-sided expectation file — and differ only in which driver they
//! load and how the browser is configured. Everything that does not depend on
//! the driver lives here, so a fix to the harness contract cannot apply to one
//! runner and not the other.
//!
//! The expectation contract is the same one WPT uses: a regression, an
//! **unexpected pass**, and a **stale entry** all fail, so fixing a check forces
//! the expectation edit into the same commit.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};

/// How long the whole Node harness gets before it is killed.
///
/// Every individual check is bounded inside `run.mjs` by `CHECK_TIMEOUT_MS`, so
/// a hung check fails as a *check*; this is the backstop for the harness hanging
/// as a whole, which a protocol bug can cause. Reaching it used to report
/// nothing but the elapsed time — see [`hang_context`].
pub const HARNESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// The parsed outcome of one harness run: check name -> (status, message).
pub type Results = BTreeMap<String, (String, String)>;

/// Installs the pinned Node dependencies if they are not there yet.
pub fn ensure_dependencies(runner: &str, dir: &Path, marker: &str) -> Result<(), String> {
    if dir.join("node_modules").join(marker).is_dir() {
        return Ok(());
    }
    eprintln!("{runner}: installing pinned Node dependencies…");
    let status = Command::new("npm")
        .args(["install", "--no-audit", "--no-fund"])
        .current_dir(dir)
        .status()
        .map_err(|error| {
            format!("could not run npm ({error}); a Node toolchain is required for this runner")
        })?;
    if !status.success() {
        return Err(String::from("npm install failed"));
    }
    Ok(())
}

/// Runs `run.mjs` and parses its `STATUS\tname[\tmessage]` lines.
pub fn run_harness(dir: &Path, endpoint: &str, base: &str) -> Result<Results, String> {
    let mut child = Command::new("node")
        .arg("run.mjs")
        .args(["--endpoint", endpoint, "--base", base])
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("could not run node ({error})"))?;

    // Drained on a thread: a harness that fills the pipe buffer while this
    // thread waits on `try_wait` would deadlock, which is the same trap
    // `wpt::run_single_subprocess` documents.
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut buffer = String::new();
        let mut stdout = stdout;
        let _ = stdout.read_to_string(&mut buffer);
        buffer
    });

    let deadline = std::time::Instant::now() + HARNESS_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                // The partial output, *not* a bare "it hung": `run.mjs` writes
                // each result as it happens, so the last line named is the check
                // before the one that stopped — which is the whole of what a CI
                // log has to go on. Discarding it here is how a 180 s timeout
                // came back saying nothing at all.
                let partial = reader.join().unwrap_or_default();
                return Err(format!(
                    "the harness hung for {HARNESS_TIMEOUT:?}{}",
                    hang_context(&partial)
                ));
            }
            Err(error) => return Err(format!("waiting for node failed: {error}")),
        }
    }
    let output = reader.join().map_err(|_| "the stdout reader panicked")?;

    let mut results = BTreeMap::new();
    for line in output.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(status), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        if status != "PASS" && status != "FAIL" {
            continue;
        }
        results.insert(
            name.to_owned(),
            (status.to_owned(), parts.next().unwrap_or("").to_owned()),
        );
    }
    Ok(results)
}

/// Says how far a killed harness got, for the error that reports the hang.
fn hang_context(partial: &str) -> String {
    let done: Vec<&str> = partial
        .lines()
        .filter(|line| line.starts_with("PASS\t") || line.starts_with("FAIL\t"))
        .filter_map(|line| line.split('\t').nth(1))
        .collect();
    match done.last() {
        Some(last) => format!(
            "; {} check(s) completed, the last being `{last}` — the hang is in \
             whichever check follows it",
            done.len()
        ),
        None => String::from("; no check completed at all"),
    }
}

/// Reads the expectation file: `name<TAB>status`, `#` comments skipped.
pub fn load_expectations(path: &Path) -> BTreeMap<String, String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#') && !line.trim().is_empty())
        .filter_map(|line| {
            let (name, status) = line.split_once('\t')?;
            Some((name.to_owned(), status.trim().to_owned()))
        })
        .collect()
}

pub fn write_expectations(runner: &str, path: &Path, results: &Results) -> ExitCode {
    let mut out = format!(
        "# {runner} conformance expectations. Regenerate with `cargo xtask {runner} --update`.\n\
         # Only non-PASS outcomes are listed; absent means the check is expected to pass.\n"
    );
    for (name, (status, _)) in results.iter().filter(|(_, (status, _))| status != "PASS") {
        out.push_str(&format!("{name}\t{status}\n"));
    }
    if let Err(error) = std::fs::write(path, out) {
        eprintln!("{runner}: could not write {}: {error}", path.display());
        return ExitCode::FAILURE;
    }
    let failures = results.values().filter(|(s, _)| s != "PASS").count();
    println!(
        "{runner}: wrote {} ({failures} expected failure(s) of {} check(s))",
        path.display(),
        results.len()
    );
    ExitCode::SUCCESS
}

pub fn compare(runner: &str, path: &Path, results: &Results, filter: Option<&str>) -> ExitCode {
    let expectations = load_expectations(path);
    let selected = |name: &str| filter.is_none_or(|needle| name.contains(needle));

    let mut regressions = Vec::new();
    let mut unexpected_passes = Vec::new();
    for (name, (status, message)) in results {
        if !selected(name) {
            continue;
        }
        let expected = expectations.get(name).map_or("PASS", String::as_str);
        match (status.as_str(), expected) {
            (actual, expected) if actual == expected => {}
            ("PASS", _) => unexpected_passes.push(name.clone()),
            (actual, _) => regressions.push(format!("{name}: {actual} — {message}")),
        }
    }
    // A stale entry is a failure too: an expectation for a check that no longer
    // exists hides the fact that the check went away.
    let mut stale: Vec<String> = expectations
        .keys()
        .filter(|name| selected(name) && !results.contains_key(*name))
        .cloned()
        .collect();
    stale.sort();

    let passed = results.values().filter(|(s, _)| s == "PASS").count();
    println!(
        "{runner}: {passed}/{} check(s) passed, {} expected failure(s)",
        results.len(),
        expectations.len()
    );

    if regressions.is_empty() && unexpected_passes.is_empty() && stale.is_empty() {
        return ExitCode::SUCCESS;
    }
    for line in &regressions {
        eprintln!("{runner}: REGRESSION {line}");
    }
    for name in &unexpected_passes {
        eprintln!("{runner}: UNEXPECTED PASS {name} — remove it from expectations.tsv");
    }
    for name in &stale {
        eprintln!("{runner}: STALE {name} — no such check; remove it from expectations.tsv");
    }
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hang_context_names_the_last_completed_check() {
        let partial = "PASS\tbrowser.version\nPASS\tpage.goto\nFAIL\tpage.pdf\tno\n";
        let context = hang_context(partial);
        assert!(context.contains("3 check(s) completed"), "{context}");
        assert!(context.contains("`page.pdf`"), "{context}");
    }

    #[test]
    fn hang_context_says_so_when_nothing_ran() {
        // The shape a harness that hung before its first check produces — and
        // the one the runner used to report for *every* hang, having thrown the
        // partial output away.
        assert!(hang_context("").contains("no check completed"));
    }

    #[test]
    fn hang_context_ignores_lines_that_are_not_results() {
        // `run.mjs` owns stdout, but a dependency logging a deprecation warning
        // there would otherwise be reported as the last check that ran.
        let partial = "PASS\tbrowser.version\n(node:1) DeprecationWarning: whatever\n";
        assert!(hang_context(partial).contains("`browser.version`"));
    }
}
