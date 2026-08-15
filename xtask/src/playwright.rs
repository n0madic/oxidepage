//! `cargo xtask playwright`: drives the CDP endpoint with a real Playwright.
//!
//! The stage-9 milestone (ADR-0033). Playwright runs **all** of its injected
//! script in a utility world created with `Page.createIsolatedWorld`, and
//! `addInitScript` and `exposeBinding` ride the same mechanism, so none of it
//! works without real isolated worlds — which is why this runner lands with
//! them rather than earlier.
//!
//! It mirrors [`crate::puppeteer`] exactly: the endpoint runs in process,
//! fixtures come off a loopback file server (CI never touches the internet),
//! and the expectations follow the same two-sided contract — a regression, an
//! unexpected pass and a stale entry all fail.
//!
//! `playwright-core` is the pin, not `playwright`: the full package downloads
//! browser binaries on install, and this runner connects over CDP to *this*
//! engine.
//!
//! Several checks are expected to fail until stage 10 lands the frame tree and
//! `Target.setAutoAttach` plumbing. Those are `FAIL` lines in
//! `expectations.tsv`, which is the contract working as designed.

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
const RUNNER: &str = "playwright";

fn harness_dir(workspace: &Path) -> PathBuf {
    workspace.join("tests/playwright")
}

fn expectations_path(workspace: &Path) -> PathBuf {
    harness_dir(workspace).join("expectations.tsv")
}

pub fn run(workspace: &Path, update: bool, filter: Option<&str>) -> ExitCode {
    if update && filter.is_some() {
        eprintln!("playwright: --update rewrites the whole file, so it cannot take --filter");
        return ExitCode::from(2);
    }
    let dir = harness_dir(workspace);
    if !dir.join("run.mjs").is_file() {
        eprintln!("playwright: {} is missing", dir.join("run.mjs").display());
        return ExitCode::FAILURE;
    }
    if let Err(message) = nodeharness::ensure_dependencies(RUNNER, &dir, "playwright-core") {
        eprintln!("playwright: {message}");
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
            eprintln!("playwright: failed to start the browser: {error}");
            return ExitCode::FAILURE;
        }
    };
    let server = match CdpServer::start(browser.clone(), ServerOptions::default()) {
        Ok(server) => server,
        Err(error) => {
            eprintln!("playwright: failed to start the endpoint: {error}");
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
            eprintln!("playwright: {message}");
            return ExitCode::FAILURE;
        }
    };
    if results.is_empty() {
        eprintln!("playwright: the harness produced no results");
        return ExitCode::FAILURE;
    }

    if update {
        return nodeharness::write_expectations(RUNNER, &expectations_path(workspace), &results);
    }
    nodeharness::compare(RUNNER, &expectations_path(workspace), &results, filter)
}
