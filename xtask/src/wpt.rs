//! WPT runner (design doc §9): runs vendored `dom/nodes` and `dom/events`
//! subsets under testharness.js against the engine, comparing outcomes with
//! a tracked expectations file. CI fails on regressions *and* on unexpected
//! passes (which force an expectations update).
//!
//! Layout:
//! - `tests/wpt/vendor/` — files vendored by `xtask fetch-wpt` at [`WPT_REV`]
//! - `tests/wpt/expectations.tsv` — `file<TAB>subtest<TAB>status` for every
//!   expected non-PASS outcome (`__harness__` rows track whole-file status)
//!
//! Each test runs in a subprocess (`xtask wpt-single`) so an engine panic is
//! a `CRASH` outcome, not a runner abort.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

pub const WPT_REPO: &str = "https://github.com/web-platform-tests/wpt";
/// Pinned WPT revision (master as of 2026-07-03); bump deliberately.
pub const WPT_REV: &str = "ced6794ecc2b7ca5f0e30149571d829d88a3c3ba";

/// Directories vendored from the WPT tree (top-level files only). Beyond the
/// two test directories, tests pull shared helpers from `dom/` (`common.js`,
/// `constants.js`), the `support/` subdirs, and `resources`/`common`.
const VENDOR_DIRS: &[&str] = &[
    "resources",
    "common",
    "dom",
    "dom/nodes",
    "dom/nodes/support",
    "dom/events",
    "dom/events/support",
    "custom-elements",
    "custom-elements/resources",
    "custom-elements/support",
    "url",
    "url/resources",
    // Shared by the css/* subsets: `/css/support/inheritance-testcommon.js` and
    // `/css/support/shorthand-testcommon.js` are pulled in by absolute path, so
    // they are not covered by any one subset's own `support/` dir.
    "css/support",
    "css/cssom",
    "css/cssom/support",
    "css/cssom-view",
    "css/cssom-view/support",
    "css/css-flexbox",
    "css/css-flexbox/support",
];

/// Directories whose vendored `.html` files are filtered to testharness tests
/// (`css/css-flexbox` is overwhelmingly reftests — ~1000 files that cannot run
/// before paint exists; only the testharness/check-layout subset is committed,
/// design ADR-0006 §10).
const VENDOR_TESTHARNESS_ONLY: &[&str] = &["css/css-flexbox", "css/cssom-view"];

/// Test directories the runner executes. Every one of them runs over a loopback
/// [`TestServer`] rooted at the vendor tree: the engine navigates to the test and
/// fetches its scripts, sheets, images and fonts over the real network stack.
const RUN_DIRS: &[&str] = &[
    "dom/nodes",
    "dom/events",
    "custom-elements",
    "url",
    "css/cssom",
    "css/cssom-view",
    "css/css-flexbox",
];

/// `url/` test files skipped: IDL-harness conformance and the huge IDNA
/// (punycode) suites, which are out of Phase 3 scope.
const URL_SKIP: &[&str] = &["idlharness", "IdnaTestV2", "historical"];

/// Filename substrings marking tests the runner skips outright, because their
/// outcome is a slow, machine-load-sensitive TIMEOUT that can never become a
/// PASS under OxidePage's constraints — excluding them keeps the suite fast and
/// deterministic rather than baking a hang into the baseline.
///
/// - `NodeList-static-length-getter-tampered`: a JS micro-benchmark
///   (`<meta name=timeout content=long>`) spinning ~10^10 property accesses to
///   probe O(1) `NodeList` indexing — unreachable under QuickJS-NG's non-JIT
///   throughput ceiling (design doc §3.3); the O(n)-per-access design is
///   ADR-0003 §5.
/// - CSS animation/transition event tests: OxidePage has no animation engine
///   (ADR-0005 v1: `DocumentAnimationSet` is empty), so `animation*`/`transition*`
///   events never fire and the tests wait until testharness's timeout. They
///   expose no passing subtests, so skipping loses no coverage. (Phase 4 gave
///   these `el.style`, so they now reach the event wait instead of failing fast
///   at `elem.style.transition = …`, which is why their harness flipped
///   `OK`→`TIMEOUT`.)
/// - `javascript-urls`: each subtest clicks an `<a href="javascript:…">` and
///   awaits the navigation that executes it. Anchor activation now exists
///   (ADR-0022), so the click *does* reach "follow the hyperlink" — but the
///   `javascript:` scheme does not: evaluating a URL as script is a separate
///   navigation mode this engine deliberately does not implement, so the
///   activation warns and stops. The awaited promise therefore still never
///   settles and the file waits out the harness budget. This is the
///   animation-events story again: before `click()` existed the call threw and
///   the file failed *fast*, which is the only reason it was not a TIMEOUT.
const SKIP_SUBSTRINGS: &[&str] = &[
    "NodeList-static-length-getter-tampered",
    "Event-dispatch-on-disabled-elements",
    "EventListener-invoke-legacy",
    "handler-count",
    "webkit-animation",
    "webkit-transition",
    "javascript-urls",
    // Surfaced once `.any.js`/`.window.js` files began running. Both need a
    // *second* global (an iframe or `window.open`), which v1 does not have
    // ("Iframes: not loaded", design §12), so they can only ever wait out the
    // harness budget. `EventListener-addEventListener.sub` also needs WPT's
    // server-side `.sub.` substitution, which `TestServer` does not implement.
    "event-global-extra",
    "EventListener-addEventListener.sub",
    // === css/cssom-view impossibilities in Phase 5 v1 (ADR-0006) ===
    // No subdocuments: every iframe-driven test waits on a load that never
    // fires and times out at the harness budget.
    "iframe",
    "Frame",
    "frameElement",
    // Smooth scrolling is out of scope (ADR-0023): there is no animation
    // timeline, so these async tests wait for a scroll animation forever.
    // `scrollIntoView` itself is implemented and no longer skipped — it treats
    // `behavior: "smooth"` as instant.
    "smooth-scroll",
    "scrollBy-scrollTo-arguments",
    "visualViewport",
    // Requires window.open / multiple browsing contexts.
    "window-open",
    "elementsFromPoint-iframes",
    // `scrollIntoView` is implemented (ADR-0023) and its files run. This one
    // is the exception: it asserts propagation to *outer frames*, so it waits
    // on a subdocument load that never happens and can only time out. The
    // writing-mode and smooth-scroll files are left running — they FAIL
    // honestly against the engine's vertical-writing-mode and animation
    // limits, and a FAIL belongs in the expectations, not in this list.
    "scrollIntoView-container",
    // Drives the pointer through WPT's `test_driver` protocol, which needs a
    // browser-side driver the runner does not implement — so it waits forever.
    // `mouseEvent.html` itself needs no driver and runs.
    "mouseEvent-offsetXY-svg",
];

/// Wall-clock budget per test before the runner records `TIMEOUT`. Generous
/// headroom over the ~3–4s real tests take, so a loaded CI machine does not
/// flake a passing test into a spurious `HANG`.
const SINGLE_BUDGET: Duration = Duration::from_secs(45);

/// The completion hook substituted for `testharnessreport.js`: serializes
/// results into a global the runner reads back.
///
/// `output: false` is load-bearing, not cosmetic. `testharness.js` defaults to
/// rendering its progress and results *into the page under test* — it writes
/// "Running, 0 complete, 1 remain" into `#log` the moment the first `test()`
/// starts, i.e. before that test's own assertions run. A layout test whose
/// `#log` sits inside the box being measured then measures the harness's own
/// text: `shrinking-column-flexbox.html` puts `#log` in a `height: 600px`
/// column flex container, so the status line ate ~20px and the item under test
/// came out 240px instead of 250px. Real `wptrunner` sets this too; without it
/// the harness perturbs the very geometry it is asked to check.
///
/// `explicit_timeout: true` is load-bearing for the same reason wptrunner sets
/// it: **the runner owns the clock, not the page.** Left to itself
/// `testharness.js` gives a file 10s, or 60s when it declares
/// `<meta name=timeout content=long>` — two deadlines the runner has to guess
/// around, and its own settle budget then has to exceed the larger of them or a
/// long file is cut off mid-run and reports nothing at all. With the harness's
/// timer disabled there is one deadline, [`settle_budget`], and the runner ends
/// the file itself by calling `timeout()` — which is a no-op *unless*
/// `explicit_timeout` is set, so the two halves only work together.
const REPORT_HOOK: &str = r#"
setup({ output: false, explicit_timeout: true });
add_completion_callback(function (tests, harness_status) {
    var clean = function (s) { return String(s).replace(/[\r\n\t]/g, " "); };
    var status_name = function (code) {
        return code === 0 ? "PASS"
            : code === 1 ? "FAIL"
            : code === 2 ? "TIMEOUT"
            : code === 3 ? "NOTRUN"
            : "PRECONDITION_FAILED";
    };
    var lines = [];
    lines.push("HARNESS\t" + (harness_status.status === 0 ? "OK"
        : harness_status.status === 1 ? "ERROR" : "TIMEOUT"));
    for (var i = 0; i < tests.length; i++) {
        lines.push(status_name(tests[i].status) + "\t" + clean(tests[i].name));
    }
    globalThis.__wpt_output = lines.join("\n");
});
"#;

fn vendor_root(workspace: &Path) -> PathBuf {
    workspace.join("tests/wpt/vendor")
}

fn expectations_path(workspace: &Path) -> PathBuf {
    workspace.join("tests/wpt/expectations.tsv")
}

// === fetch-wpt ===

/// Vendors the WPT subsets via a sparse, shallow checkout at [`WPT_REV`].
pub fn fetch(workspace: &Path) -> ExitCode {
    let tmp = workspace.join("target/wpt-checkout");
    if tmp.exists()
        && let Err(e) = std::fs::remove_dir_all(&tmp)
    {
        eprintln!("fetch-wpt: cannot clean {}: {e}", tmp.display());
        return ExitCode::FAILURE;
    }
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        eprintln!("fetch-wpt: cannot create {}: {e}", tmp.display());
        return ExitCode::FAILURE;
    }

    let git = |args: &[&str]| -> bool {
        match Command::new("git").args(args).current_dir(&tmp).status() {
            Ok(s) if s.success() => true,
            Ok(s) => {
                eprintln!("fetch-wpt: git {} exited with {s}", args.join(" "));
                false
            }
            Err(e) => {
                eprintln!("fetch-wpt: failed to run git: {e}");
                false
            }
        }
    };
    let steps: Vec<Vec<&str>> = vec![
        vec!["init", "--quiet"],
        vec!["remote", "add", "origin", WPT_REPO],
        vec!["sparse-checkout", "init", "--no-cone"],
        vec!["sparse-checkout", "set", "--no-cone"]
            .into_iter()
            .chain(VENDOR_DIRS.iter().map(|d| {
                // top-level files of each dir only
                Box::leak(format!("/{d}/*").into_boxed_str()) as &str
            }))
            .collect(),
        vec![
            "fetch",
            "--quiet",
            "--depth",
            "1",
            "--filter=blob:none",
            "origin",
            WPT_REV,
        ],
        vec!["checkout", "--quiet", "FETCH_HEAD"],
    ];
    for step in &steps {
        if !git(step) {
            return ExitCode::FAILURE;
        }
    }

    // Copy top-level files of each vendored dir.
    let dest_root = vendor_root(workspace);
    let _ = std::fs::remove_dir_all(&dest_root);
    let mut copied = 0usize;
    for dir in VENDOR_DIRS {
        let src_dir = tmp.join(dir);
        // Some listed dirs (e.g. dom/events/support) may not exist upstream;
        // skip them rather than fail.
        if !src_dir.is_dir() {
            continue;
        }
        let dest_dir = dest_root.join(dir);
        if let Err(e) = std::fs::create_dir_all(&dest_dir) {
            eprintln!("fetch-wpt: cannot create {}: {e}", dest_dir.display());
            return ExitCode::FAILURE;
        }
        let entries = match std::fs::read_dir(&src_dir) {
            Ok(entries) => entries,
            Err(e) => {
                eprintln!("fetch-wpt: cannot read {}: {e}", src_dir.display());
                return ExitCode::FAILURE;
            }
        };
        let harness_only = VENDOR_TESTHARNESS_ONLY.contains(dir);
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let name = path.file_name().expect("file has a name");
                // Reftest-heavy suites: commit only testharness tests (plus
                // non-HTML helpers); reftests and their references cannot
                // run before paint exists.
                if harness_only && path.extension().is_some_and(|e| e == "html") {
                    let is_harness_test = std::fs::read_to_string(&path)
                        .is_ok_and(|content| content.contains("testharness.js"));
                    let is_reference = name.to_string_lossy().contains("-ref.");
                    if !is_harness_test || is_reference {
                        continue;
                    }
                }
                if std::fs::copy(&path, dest_dir.join(name)).is_err() {
                    eprintln!("fetch-wpt: copy failed for {}", path.display());
                    return ExitCode::FAILURE;
                }
                copied += 1;
            }
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
    println!(
        "fetch-wpt: vendored {copied} files at {} into {}",
        &WPT_REV[..12],
        dest_root.display()
    );
    ExitCode::SUCCESS
}

// === wpt (parent runner) ===

pub fn run(workspace: &Path, update: bool, filter: Option<&str>) -> ExitCode {
    let vendor = vendor_root(workspace);
    if !vendor.exists() {
        eprintln!("wpt: no vendored tests; run `cargo xtask fetch-wpt` first");
        return ExitCode::from(2);
    }
    // `--update` rewrites the whole expectations file from the run's results,
    // so pairing it with `--filter` would silently drop every other test's
    // expectations. Refuse the combination.
    if update && filter.is_some() {
        eprintln!(
            "wpt: --update cannot be combined with --filter (it would drop other tests' expectations)"
        );
        return ExitCode::from(2);
    }

    let mut test_files: Vec<PathBuf> = Vec::new();
    for dir in RUN_DIRS {
        let Ok(entries) = std::fs::read_dir(vendor.join(dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            // `.any.js` / `.window.js` are testharness tests with no committed
            // `.html`: the server synthesizes the wrapper. They used to be picked
            // up only under `url/`, because that was the only subset served over
            // HTTP; every other dir was rewritten and loaded from a string, which
            // could not produce a wrapper. Now that every test is served, they run
            // wherever they appear — 22 files in `dom/` alone were simply invisible.
            let is_generated_wrapper = name.ends_with(".any.js") || name.ends_with(".window.js");
            let is_test = if is_generated_wrapper {
                // `SKIP_SUBSTRINGS` is global — it names tests that can only ever
                // hang, wherever they live — and `URL_SKIP` *adds* the `url/`-only
                // exclusions. Consulting only one of the two silently ignored a
                // `SKIP_SUBSTRINGS` entry for a `url/` test, which is how a
                // known-hanging file got its TIMEOUT baked into the baseline.
                let skipped = SKIP_SUBSTRINGS.iter().any(|s| name.contains(s))
                    || (*dir == "url" && URL_SKIP.iter().any(|s| name.contains(s)));
                !skipped
            } else if *dir == "url" {
                // `url/` has no runnable `.html`; its `.html` files are wrappers
                // committed upstream for other harnesses.
                false
            } else {
                path.extension().is_some_and(|e| e == "html")
                    && !name.contains("-ref.")
                    && !name.ends_with(".tentative.html")
                    && !SKIP_SUBSTRINGS.iter().any(|s| name.contains(s))
                    // Layout suites mix reftests in; only testharness files
                    // are runnable (the vendoring filter enforces the same
                    // for the reftest-heavy dirs).
                    && std::fs::read_to_string(&path)
                        .is_ok_and(|content| content.contains("testharness.js"))
            };
            if is_test {
                test_files.push(path);
            }
        }
    }
    test_files.sort();
    if let Some(filter) = filter {
        test_files.retain(|p| p.to_string_lossy().contains(filter));
    }
    if test_files.is_empty() {
        eprintln!("wpt: no tests matched");
        return ExitCode::from(2);
    }

    let exe = std::env::current_exe().expect("current exe");
    // Each test runs in its own subprocess; the loop is embarrassingly
    // parallel (every `url/` test binds its own ephemeral loopback port), so
    // run up to `available_parallelism` subprocesses at once. Debug builds are
    // slow — CI runs this job with `--release` (see .github/workflows/ci.yml).
    let concurrency = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(4);
    // (file, subtest) → status; subtest "__harness__" is the file outcome.
    let results_mutex: std::sync::Mutex<BTreeMap<(String, String), String>> =
        std::sync::Mutex::new(BTreeMap::new());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let started = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..concurrency {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(path) = test_files.get(index) else {
                        break;
                    };
                    let rel = path
                        .strip_prefix(&vendor)
                        .expect("test under vendor root")
                        .to_string_lossy()
                        .replace('\\', "/");
                    // Retry once on a transient whole-file outcome (subprocess
                    // hang, crash, or no harness output): a genuinely-broken
                    // test fails both attempts, while a test that merely lost a
                    // scheduling race under machine load passes the retry. Keeps
                    // flaky-but-passing tests in the suite without polluting
                    // expectations.
                    let mut outcome = run_single_subprocess(&exe, path);
                    let transient = outcome.iter().any(|(subtest, status)| {
                        subtest == "__harness__" && is_transient_status(status)
                    });
                    if transient {
                        outcome = run_single_subprocess(&exe, path);
                    }
                    let mut results = results_mutex.lock().expect("results mutex");
                    insert_outcome(&mut results, &rel, outcome);
                }
            });
        }
    });
    let results = results_mutex.into_inner().expect("results mutex");
    eprintln!(
        "wpt: ran {} files in {:.1}s ({concurrency}-way parallel)",
        test_files.len(),
        started.elapsed().as_secs_f64()
    );

    let expectations = load_expectations(&expectations_path(workspace));

    if update {
        // A baseline must never bake in a transient failure: a timed-out file
        // records only `__harness__ -> HANG` and drops all its real subtest
        // rows, so accepting it would silently erase coverage. Require a clean
        // run before writing.
        //
        // This guards *whole-file* transients only. A subtest that flips
        // PASS↔FAIL inside an otherwise-OK harness run is neither retried nor
        // blocked, so a flaky FAIL captured here is baked into the baseline and
        // the next clean run reports it as an unexpected pass. Re-run `--update`
        // if that happens; per-subtest retries would multiply an 11-minute suite.
        let transient = transient_results(&results);
        if !transient.is_empty() {
            eprintln!(
                "wpt: refusing to update expectations — {} transient result(s) (HANG/CRASH/NORESULT). \
                 Re-run until clean before accepting a baseline:",
                transient.len()
            );
            for t in &transient {
                eprintln!("  {t}");
            }
            return ExitCode::FAILURE;
        }
        let mut out = String::from(
            "# WPT expectations: every expected non-PASS outcome, one per line.\n\
             # Format: <file>\\t<subtest>\\t<status>. Regenerate: cargo xtask wpt --update\n",
        );
        for ((file, subtest), status) in &results {
            if status != "PASS" {
                out.push_str(&format!("{file}\t{subtest}\t{status}\n"));
            }
        }
        if let Err(e) = std::fs::write(expectations_path(workspace), out) {
            eprintln!("wpt: cannot write expectations: {e}");
            return ExitCode::FAILURE;
        }
        let non_pass = results.values().filter(|s| *s != "PASS").count();
        let pass = results.len() - non_pass;
        println!("wpt: expectations updated ({pass} PASS, {non_pass} tracked non-PASS)");
        return ExitCode::SUCCESS;
    }

    // Compare.
    let mut regressions = Vec::new();
    let mut unexpected_passes = Vec::new();
    for ((file, subtest), status) in &results {
        let expected = expectations
            .get(&(file.clone(), subtest.clone()))
            .map(String::as_str)
            .unwrap_or("PASS");
        if status != expected {
            if status == "PASS" {
                unexpected_passes.push(format!("{file} :: {subtest} (expected {expected})"));
            } else {
                regressions.push(format!(
                    "{file} :: {subtest} — {status} (expected {expected})"
                ));
            }
        }
    }
    // Expected entries that no longer exist at all (renamed/removed subtests).
    for (file, subtest) in expectations.keys() {
        if !results.contains_key(&(file.clone(), subtest.clone())) {
            unexpected_passes.push(format!("{file} :: {subtest} (expectation is stale)"));
        }
    }

    let pass = results.values().filter(|s| *s == "PASS").count();
    println!(
        "wpt: {pass}/{} subtests PASS; {} expected non-PASS tracked",
        results.len(),
        expectations.len()
    );
    if !regressions.is_empty() {
        eprintln!("\nwpt: {} REGRESSION(S):", regressions.len());
        for r in &regressions {
            eprintln!("  {r}");
        }
    }
    if !unexpected_passes.is_empty() {
        eprintln!(
            "\nwpt: {} UNEXPECTED PASS(ES) — run `cargo xtask wpt --update`:",
            unexpected_passes.len()
        );
        for u in &unexpected_passes {
            eprintln!("  {u}");
        }
    }
    if regressions.is_empty() && unexpected_passes.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn load_expectations(path: &Path) -> BTreeMap<(String, String), String> {
    let mut map = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return map;
    };
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(3, '\t');
        if let (Some(file), Some(subtest), Some(status)) =
            (parts.next(), parts.next(), parts.next())
        {
            map.insert((file.to_owned(), subtest.to_owned()), status.to_owned());
        }
    }
    map
}

/// Whole-file outcomes that indicate a transient/incomplete run rather than a
/// deterministic result: a hang under load, an engine crash, or no harness
/// output at all. These trigger a retry and block a baseline update — accepting
/// one would bake a machine-load artifact into the expectations.
fn is_transient_status(status: &str) -> bool {
    matches!(status, "HANG" | "CRASH" | "NORESULT")
}

/// Lists every transient result in a run, formatted for reporting. Used to gate
/// `--update`: a non-empty list means the run is not clean enough to baseline.
fn transient_results(results: &BTreeMap<(String, String), String>) -> Vec<String> {
    results
        .iter()
        .filter(|(_, status)| is_transient_status(status))
        .map(|((file, subtest), status)| format!("{file} :: {subtest} — {status}"))
        .collect()
}

/// Records one file's `(subtest, status)` outcomes into the shared results map,
/// disambiguating duplicate subtest names (which testharness permits) by
/// suffixing `#N`. Without this, a later same-named PASS would overwrite an
/// earlier FAIL under the `BTreeMap` key and mask the regression.
fn insert_outcome(
    results: &mut BTreeMap<(String, String), String>,
    rel: &str,
    outcome: Vec<(String, String)>,
) {
    for (subtest, status) in outcome {
        let key = if results.contains_key(&(rel.to_owned(), subtest.clone())) {
            let mut n = 2;
            let name = loop {
                let candidate = format!("{subtest} #{n}");
                if !results.contains_key(&(rel.to_owned(), candidate.clone())) {
                    break candidate;
                }
                n += 1;
            };
            eprintln!("wpt: {rel}: duplicate subtest name {subtest:?} recorded as {name:?}");
            (rel.to_owned(), name)
        } else {
            (rel.to_owned(), subtest)
        };
        results.insert(key, status);
    }
}

/// Runs one test file in a subprocess; converts crashes and hangs into
/// outcomes.
///
/// A dedicated thread drains the child's stdout: reports with more than a
/// pipe buffer of output (large `testharness` result sets, ~100 KB) would
/// otherwise deadlock — the child blocks writing to a full pipe while the
/// parent waits for it to exit before reading.
fn run_single_subprocess(exe: &Path, test: &Path) -> Vec<(String, String)> {
    let child = Command::new(exe)
        .arg("wpt-single")
        .arg(test)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(_) => return vec![("__harness__".into(), "CRASH".into())],
    };
    let stdout = child.stdout.take().expect("stdout piped");
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = String::new();
        let _ = std::io::BufReader::new(stdout).read_to_string(&mut buf);
        buf
    });

    let deadline = Instant::now() + SINGLE_BUDGET;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = reader.join().unwrap_or_default();
                if !status.success() {
                    return vec![("__harness__".into(), "CRASH".into())];
                }
                return parse_single_output(&output);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    // The reader unblocks once the killed child's stdout
                    // closes; drop its result.
                    let _ = reader.join();
                    return vec![("__harness__".into(), "HANG".into())];
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                // Reap the child and its reader thread before reporting CRASH,
                // matching the timeout path — otherwise a `try_wait` error
                // leaves a zombie and a detached reader blocked on the pipe.
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return vec![("__harness__".into(), "CRASH".into())];
            }
        }
    }
}

fn parse_single_output(output: &str) -> Vec<(String, String)> {
    let mut results = Vec::new();
    for line in output.lines() {
        let mut parts = line.splitn(2, '\t');
        match (parts.next(), parts.next()) {
            (Some("HARNESS"), Some(status)) => {
                results.push(("__harness__".to_owned(), status.to_owned()));
            }
            (Some(status), Some(name)) => {
                results.push((name.to_owned(), status.to_owned()));
            }
            _ => {}
        }
    }
    if results.is_empty() {
        results.push(("__harness__".into(), "NORESULT".into()));
    }
    results
}

// === wpt-single (child) ===

/// Runs one testharness.js file in-process, printing TSV results.
pub fn run_single(workspace: &Path, test: &Path) -> ExitCode {
    let vendor = vendor_root(workspace);
    // The path may be absolute (the parallel runner builds it that way) or
    // relative to the workspace (`xtask wpt-single tests/wpt/vendor/…`, as the
    // docs spell it). Anchor it before stripping: the remainder is the test's
    // path *on the server*, and a stray `tests/wpt/vendor/` prefix left in it
    // would 404 every subresource.
    let absolute = if test.is_absolute() {
        test.to_path_buf()
    } else {
        workspace.join(test)
    };
    let Ok(rel) = absolute.strip_prefix(&vendor) else {
        eprintln!(
            "wpt-single: {} is outside the vendor tree ({})",
            absolute.display(),
            vendor.display()
        );
        return ExitCode::FAILURE;
    };
    let rel = rel.to_string_lossy().replace('\\', "/");

    run_over_test_server(&vendor, &rel)
}

/// Runs a test over a loopback [`TestServer`]: the engine navigates to the
/// vendored file (or, for `.any.js`/`.window.js`, to the server-generated
/// harness wrapper) and pulls *every* subresource over the real network stack.
///
/// The runner used to inline `<script src>` and `<link rel=stylesheet>` into the
/// markup instead. That covered scripts and sheets but nothing else, so an
/// `<img src="support/solidblue.png">` never loaded: `naturalWidth` stayed 0 and
/// any layout sized from the image collapsed. Serving the tree instead of
/// rewriting it means images, fonts and `fetch()` all just work, and the engine
/// is exercised through the same code path a real page takes.
fn run_over_test_server(vendor: &Path, rel: &str) -> ExitCode {
    let html_rel = if let Some(stem) = rel.strip_suffix(".any.js") {
        format!("{stem}.any.html")
    } else if let Some(stem) = rel.strip_suffix(".window.js") {
        format!("{stem}.window.html")
    } else {
        rel.to_owned()
    };

    let server = crate::testserver::TestServer::start(vendor.to_path_buf(), REPORT_HOOK.to_owned());
    let url = format!("http://127.0.0.1:{}/{html_rel}", server.port());

    let page = match oxidepage_page::Page::new(oxidepage_page::PageOptions {
        // Loopback test server: keep HTTP(S)-only + budgets, allow 127.0.0.1.
        policy: Some(oxidepage_page::ResourcePolicy::permissive_localhost()),
        // The runners must be deterministic, and a live layout deadline is
        // not: a loaded CI machine (or a debug build, where layout is an order
        // of magnitude slower) would abort a flush and produce a *blank*
        // golden, reference or report — a confusing pixel diff rather than a
        // timeout. `Page`'s 10 s default exists to bound a hostile document,
        // which a committed fixture is not (ADR-0037 D8).
        layout_budget: Some(std::time::Duration::MAX),
        ..Default::default()
    }) {
        Ok(page) => page,
        Err(e) => {
            eprintln!("wpt-single: page creation failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = page.navigate(&url, oxidepage_page::WaitUntil::Load) {
        eprintln!("wpt-single: navigation failed: {e}");
        return ExitCode::FAILURE;
    }
    let mut output = read_wpt_output(&page);
    if output.is_none() {
        page.settle(settle_budget(vendor, &html_rel));
        output = read_wpt_output(&page);
    }
    if output.is_none() {
        // The budget ran out with the file still going. End it the way
        // `wptrunner` does — `timeout()` marks the harness TIMEOUT and runs the
        // completion callbacks — so what it *had* finished is reported instead
        // of the whole file coming back `NORESULT`. Only reachable because
        // `REPORT_HOOK` set `explicit_timeout`.
        let _ = page.eval("if (typeof timeout === 'function') { timeout(); }");
        page.settle(Duration::from_secs(2));
        output = read_wpt_output(&page);
    }
    print_output(output)
}

/// Normal settle budget: how long a file may run before the runner ends it.
///
/// This is the *only* deadline — `REPORT_HOOK` disables the harness's own — so
/// it is what a `TIMEOUT` in the expectations means.
const SETTLE_NORMAL: Duration = Duration::from_secs(12);

/// The budget for a file declaring `<meta name=timeout content=long>`.
///
/// The declaration is the file telling the runner it is slow by nature — WPT's
/// own runner reads the same meta and multiplies its budget. Not the 60s
/// `testharness.js` would have used: the point is headroom for a file that
/// legitimately needs more than 12s, not to wait out one that is stuck. The
/// files that still overrun it report `TIMEOUT` with whatever they finished,
/// exactly as they did at the harness's 10s.
const SETTLE_LONG: Duration = Duration::from_secs(20);

/// Which of the two a file gets, read from its own `<meta name=timeout>`.
///
/// Source text rather than the parsed DOM: this decides how long to run the
/// page *before* running it. A file we cannot read gets the normal budget —
/// the same answer as a file with no such meta.
fn settle_budget(vendor: &Path, html_rel: &str) -> Duration {
    let Ok(source) = std::fs::read_to_string(vendor.join(html_rel)) else {
        // `.any.html` / `.window.html` are generated by the server and carry no
        // meta of their own; the generated wrapper is the normal case.
        return SETTLE_NORMAL;
    };
    let long = source.split('<').any(|tag| {
        is_tag(tag, "meta")
            && extract_attr(tag, "name").as_deref() == Some("timeout")
            && extract_attr(tag, "content").as_deref() == Some("long")
    });
    if long { SETTLE_LONG } else { SETTLE_NORMAL }
}

/// Whether the text after a `<` opens the element `name`.
///
/// A prefix test is not enough in either direction. HTML tag names are ASCII
/// case-insensitive and WPT files are not normalized, so `<META name=timeout
/// content=long>` must match — a file that declared itself slow in uppercase
/// silently got the 12s budget and reported a partial `TIMEOUT` instead of its
/// results. And the name has to *end*: `starts_with("meta")` also matched
/// `<metadata …>`, which is a different element entirely.
fn is_tag(tag: &str, name: &str) -> bool {
    let Some(rest) = tag.get(..name.len()) else {
        return false;
    };
    if !rest.eq_ignore_ascii_case(name) {
        return false;
    }
    // The name is delimited by whitespace, the tag's own `>`, or a self-closing
    // `/`. Nothing at all (a truncated file) is not a tag.
    match tag[name.len()..].chars().next() {
        Some(c) => c.is_ascii_whitespace() || c == '>' || c == '/',
        None => false,
    }
}

fn print_output(output: Option<String>) -> ExitCode {
    match output {
        Some(text) => println!("{text}"),
        None => println!("HARNESS\tNORESULT"),
    }
    ExitCode::SUCCESS
}

fn read_wpt_output(page: &oxidepage_page::Page) -> Option<String> {
    match page.eval("globalThis.__wpt_output") {
        Ok(oxidepage_js::JsValue::String(s)) => Some(s),
        _ => None,
    }
}

/// Extracts an attribute value, handling quoted (`x="v"`, `x='v'`) and
/// unquoted (`x=v`) forms — WPT files use all three. Shared with the reftest
/// runner, whose `<link>`/`<meta>` tags use single quotes too.
pub(crate) fn extract_attr(tag: &str, name: &str) -> Option<String> {
    // Find the attribute: preceded by whitespace or the `<tag` token, followed
    // by `=` (so `data-src`/`crossorigin` and the tag name itself don't match).
    let bytes = tag.as_bytes();
    let mut search = 0;
    let idx = loop {
        let rel = tag[search..].find(name)?;
        let at = search + rel;
        let before_ok = at == 0 || bytes[at - 1].is_ascii_whitespace();
        let after = tag[at + name.len()..].trim_start();
        if before_ok && after.starts_with('=') {
            break at;
        }
        search = at + name.len();
    };
    let after = tag[idx + name.len()..].trim_start();
    let after = after.strip_prefix('=')?.trim_start();
    match after.chars().next()? {
        '"' => after[1..].find('"').map(|end| after[1..1 + end].to_owned()),
        '\'' => after[1..]
            .find('\'')
            .map(|end| after[1..1 + end].to_owned()),
        _ => {
            // Unquoted: value runs to whitespace or the tag's `>`.
            let end = after
                .find(|c: char| c.is_ascii_whitespace() || c == '>')
                .unwrap_or(after.len());
            (end > 0).then(|| after[..end].to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_status_covers_hang_crash_noresult() {
        // LOW 4: NORESULT is as load-sensitive as HANG/CRASH and must retry.
        assert!(is_transient_status("HANG"));
        assert!(is_transient_status("CRASH"));
        assert!(is_transient_status("NORESULT"));
        assert!(!is_transient_status("PASS"));
        assert!(!is_transient_status("FAIL"));
        assert!(!is_transient_status("TIMEOUT"));
    }

    #[test]
    fn transient_results_flags_a_hang_and_blocks_update() {
        // MEDIUM 2: a run containing a HANG is not clean enough to baseline.
        let mut results = BTreeMap::new();
        results.insert(
            ("dom/foo.html".to_string(), "__harness__".to_string()),
            "HANG".to_string(),
        );
        results.insert(
            ("dom/bar.html".to_string(), "sub".to_string()),
            "FAIL".to_string(),
        );
        let flagged = transient_results(&results);
        assert_eq!(flagged.len(), 1);
        assert!(flagged[0].contains("HANG"));
    }

    #[test]
    fn transient_results_clean_run_is_empty() {
        let mut results = BTreeMap::new();
        results.insert(("a".into(), "s1".into()), "PASS".to_string());
        results.insert(("a".into(), "s2".into()), "FAIL".to_string());
        results.insert(("a".into(), "__harness__".into()), "OK".to_string());
        assert!(transient_results(&results).is_empty());
    }

    #[test]
    fn insert_outcome_disambiguates_duplicate_subtests() {
        // LOW 6: a later same-named PASS must not mask an earlier FAIL.
        let mut results = BTreeMap::new();
        insert_outcome(
            &mut results,
            "a.html",
            vec![
                ("dup".into(), "FAIL".into()),
                ("dup".into(), "PASS".into()),
                ("dup".into(), "TIMEOUT".into()),
                ("unique".into(), "PASS".into()),
            ],
        );
        assert_eq!(results.len(), 4);
        assert_eq!(
            results
                .get(&("a.html".into(), "dup".into()))
                .map(String::as_str),
            Some("FAIL")
        );
        assert_eq!(
            results
                .get(&("a.html".into(), "dup #2".into()))
                .map(String::as_str),
            Some("PASS")
        );
        assert_eq!(
            results
                .get(&("a.html".into(), "dup #3".into()))
                .map(String::as_str),
            Some("TIMEOUT")
        );
        assert_eq!(
            results
                .get(&("a.html".into(), "unique".into()))
                .map(String::as_str),
            Some("PASS")
        );
    }

    #[test]
    fn extract_attr_handles_single_and_double_quotes() {
        // LOW 5: the reftest runner reuses this; single quotes must parse.
        assert_eq!(
            extract_attr("<link rel='match' href='ref.html'", "href").as_deref(),
            Some("ref.html")
        );
        assert_eq!(
            extract_attr(r#"<link rel="match" href="ref.html""#, "href").as_deref(),
            Some("ref.html")
        );
        assert_eq!(
            extract_attr("<script src=raw.js>", "src").as_deref(),
            Some("raw.js")
        );
    }

    /// The `<meta name=timeout content=long>` detector must not answer on a
    /// tag name it merely prefixes, and must answer whatever the case.
    ///
    /// A prefix test called `<metadata>` a `<meta>`, and a case-sensitive one
    /// missed `<META>` — WPT files are not normalized, and a slow file that got
    /// the 12s budget reports a partial `TIMEOUT` instead of its results.
    #[test]
    fn is_tag_ends_the_name_and_ignores_case() {
        assert!(is_tag("meta name=timeout>", "meta"));
        assert!(is_tag("META name=timeout>", "meta"));
        assert!(is_tag("meta>", "meta"));
        assert!(is_tag("meta/>", "meta"));
        assert!(!is_tag("metadata name=timeout>", "meta"));
        assert!(!is_tag("met", "meta"));
        assert!(!is_tag("meta", "meta"));
    }
}
