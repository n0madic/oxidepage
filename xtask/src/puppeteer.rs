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
//!
//! Everything that is not Puppeteer-specific lives in [`crate::nodeharness`],
//! shared with `cargo xtask playwright`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use oxidepage_cdp::{CdpServer, ServerOptions};
use oxidepage_engine::page_api::ResourcePolicy;
use oxidepage_engine::{
    Browser, BrowserOptions, ContextOptions, DEFAULT_DIALOG_TIMEOUT, DialogPolicy,
};

use crate::testserver::TestServer;

use crate::nodeharness;

/// This runner's name, in its own messages and in the `--update` banner.
const RUNNER: &str = "puppeteer";

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
    if let Err(message) = nodeharness::ensure_dependencies(RUNNER, &dir, "puppeteer-core") {
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
            // Deterministic like the golden and reftest runners: a fixture
            // is not a hostile document, and a loaded CI machine tripping the
            // 10 s layout deadline would fail a check with a blank capture
            // rather than a real result (ADR-0037 D8).
            layout_budget: Some(std::time::Duration::MAX),
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

    let outcome = nodeharness::run_harness(&dir, server.browser_ws_url(), &base);
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
        return nodeharness::write_expectations(RUNNER, &expectations_path(workspace), &results);
    }
    nodeharness::compare(RUNNER, &expectations_path(workspace), &results, filter)
}
