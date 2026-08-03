//! `cargo xtask puppeteer`: drives the CDP endpoint with a real Puppeteer.
//!
//! The endpoint is started **in process** (`oxidepage_cdp::CdpServer`) rather
//! than as an `oxidepage serve` subprocess: there is no pipe to babysit, no
//! stdout parsing, and a panic in the server shows up as a panic here instead
//! of as a mysteriously closed socket.
//!
//! Fixtures are served from a loopback file server — CI never touches the
//! internet (design §9) — and the expectation file follows the same two-sided
//! contract as WPT: a regression *and* an unexpected pass both fail, so fixing
//! something forces the expectation edit into the same commit.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use oxidepage_cdp::{CdpServer, ServerOptions};
use oxidepage_engine::page_api::ResourcePolicy;
use oxidepage_engine::{
    Browser, BrowserOptions, ContextOptions, DEFAULT_DIALOG_TIMEOUT, DialogPolicy,
};

use crate::testserver::TestServer;

/// How long the whole Node harness gets before it is killed.
///
/// Every individual check has its own timeout inside `run.mjs`; this is the
/// backstop for the harness hanging as a whole, which a protocol bug can cause.
const HARNESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

fn automation_dir(workspace: &Path) -> PathBuf {
    workspace.join("tests/automation")
}

fn expectations_path(workspace: &Path) -> PathBuf {
    automation_dir(workspace).join("expectations.tsv")
}

pub fn run(workspace: &Path, update: bool, filter: Option<&str>) -> ExitCode {
    if update && filter.is_some() {
        eprintln!("puppeteer: --update rewrites the whole file, so it cannot take --filter");
        return ExitCode::from(2);
    }
    let dir = automation_dir(workspace);
    if !dir.join("run.mjs").is_file() {
        eprintln!("puppeteer: {} is missing", dir.join("run.mjs").display());
        return ExitCode::FAILURE;
    }
    if let Err(message) = ensure_dependencies(&dir) {
        eprintln!("puppeteer: {message}");
        return ExitCode::FAILURE;
    }

    // Loopback is allowed so the fixture server is reachable; the default
    // policy blocks private hosts.
    let browser = match Browser::new(BrowserOptions {
        policy: ResourcePolicy::permissive_localhost(),
        default_context: ContextOptions {
            // What `oxidepage serve` uses: `Ask` is what makes a dialog
            // reportable and answerable over the protocol.
            dialog_policy: DialogPolicy::Ask {
                timeout: DEFAULT_DIALOG_TIMEOUT,
            },
            ..ContextOptions::default()
        },
        ..BrowserOptions::default()
    }) {
        Ok(browser) => browser,
        Err(error) => {
            eprintln!("puppeteer: failed to start the browser: {error}");
            return ExitCode::FAILURE;
        }
    };
    let server = match CdpServer::start(browser.clone(), ServerOptions::default()) {
        Ok(server) => server,
        Err(error) => {
            eprintln!("puppeteer: failed to start the endpoint: {error}");
            return ExitCode::FAILURE;
        }
    };
    let fixtures = TestServer::start(dir.join("fixtures"), String::new());
    let base = format!("http://127.0.0.1:{}", fixtures.port());

    let outcome = run_harness(&dir, server.browser_ws_url(), &base);
    // Ordering: stop accepting before the browser goes down, so an in-flight
    // command cannot outlive the page it names.
    server.shutdown();
    browser.close();

    let results = match outcome {
        Ok(results) => results,
        Err(message) => {
            eprintln!("puppeteer: {message}");
            return ExitCode::FAILURE;
        }
    };
    if results.is_empty() {
        eprintln!("puppeteer: the harness produced no results");
        return ExitCode::FAILURE;
    }

    if update {
        return write_expectations(&expectations_path(workspace), &results);
    }
    compare(&expectations_path(workspace), &results, filter)
}

/// Installs the pinned Node dependencies if they are not there yet.
fn ensure_dependencies(dir: &Path) -> Result<(), String> {
    if dir.join("node_modules/puppeteer-core").is_dir() {
        return Ok(());
    }
    eprintln!("puppeteer: installing pinned Node dependencies…");
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
fn run_harness(
    dir: &Path,
    endpoint: &str,
    base: &str,
) -> Result<BTreeMap<String, (String, String)>, String> {
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
                return Err(format!("the harness hung for {HARNESS_TIMEOUT:?}"));
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

/// Reads the expectation file: `name<TAB>status`, `#` comments skipped.
fn load_expectations(path: &Path) -> BTreeMap<String, String> {
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

fn write_expectations(path: &Path, results: &BTreeMap<String, (String, String)>) -> ExitCode {
    let mut out = String::from(
        "# Puppeteer conformance expectations. Regenerate with `cargo xtask puppeteer --update`.\n\
         # Only non-PASS outcomes are listed; absent means the check is expected to pass.\n",
    );
    for (name, (status, _)) in results.iter().filter(|(_, (status, _))| status != "PASS") {
        out.push_str(&format!("{name}\t{status}\n"));
    }
    if let Err(error) = std::fs::write(path, out) {
        eprintln!("puppeteer: could not write {}: {error}", path.display());
        return ExitCode::FAILURE;
    }
    let failures = results.values().filter(|(s, _)| s != "PASS").count();
    println!(
        "puppeteer: wrote {} ({failures} expected failure(s) of {} check(s))",
        path.display(),
        results.len()
    );
    ExitCode::SUCCESS
}

fn compare(
    path: &Path,
    results: &BTreeMap<String, (String, String)>,
    filter: Option<&str>,
) -> ExitCode {
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
        "puppeteer: {passed}/{} check(s) passed, {} expected failure(s)",
        results.len(),
        expectations.len()
    );

    if regressions.is_empty() && unexpected_passes.is_empty() && stale.is_empty() {
        return ExitCode::SUCCESS;
    }
    for line in &regressions {
        eprintln!("puppeteer: REGRESSION {line}");
    }
    for name in &unexpected_passes {
        eprintln!("puppeteer: UNEXPECTED PASS {name} — remove it from expectations.tsv");
    }
    for name in &stale {
        eprintln!("puppeteer: STALE {name} — no such check; remove it from expectations.tsv");
    }
    ExitCode::FAILURE
}
