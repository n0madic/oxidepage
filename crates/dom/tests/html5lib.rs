//! html5lib-tests tree-construction conformance runner (Phase 1 exit
//! criterion, design doc §10).
//!
//! Runs every vendored `.dat` test through our `TreeSink` and compares the
//! html5lib tree dump. Known failures live in
//! `tests/html5lib-expectations.txt`; the run fails on regressions **and**
//! on unexpected passes, so expectation updates land with behavior changes.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use html5ever::{LocalName, ns};
use oxidepage_dom::dump::dump_document;
use oxidepage_dom::{DomTree, ParseOptions, QualName};

struct TestCase {
    /// `file.dat:index`, 0-based within the file.
    id: String,
    data: String,
    fragment_context: Option<String>,
    script_mode: Option<bool>,
    expected: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Section {
    Data,
    Skip,
    FragmentContext,
    Document,
}

fn parse_dat(file_name: &str, content: &str) -> Vec<TestCase> {
    let mut tests = Vec::new();
    let mut current: Option<TestCase> = None;
    let mut section = Section::Skip;

    for line in content.lines() {
        match line {
            "#data" => {
                if let Some(mut test) = current.take() {
                    finalize(&mut test);
                    tests.push(test);
                }
                current = Some(TestCase {
                    id: format!("{file_name}:{}", tests.len()),
                    data: String::new(),
                    fragment_context: None,
                    script_mode: None,
                    expected: String::new(),
                });
                section = Section::Data;
                continue;
            }
            "#errors" | "#new-errors" => {
                section = Section::Skip;
                continue;
            }
            "#document-fragment" => {
                section = Section::FragmentContext;
                continue;
            }
            "#script-on" => {
                if let Some(test) = &mut current {
                    test.script_mode = Some(true);
                }
                section = Section::Skip;
                continue;
            }
            "#script-off" => {
                if let Some(test) = &mut current {
                    test.script_mode = Some(false);
                }
                section = Section::Skip;
                continue;
            }
            "#document" => {
                section = Section::Document;
                continue;
            }
            _ => {}
        }
        let Some(test) = &mut current else { continue };
        match section {
            Section::Data => {
                test.data.push_str(line);
                test.data.push('\n');
            }
            Section::FragmentContext => {
                if !line.is_empty() {
                    test.fragment_context = Some(line.to_string());
                }
            }
            Section::Document => {
                // The blank separator line between tests is not part of the
                // expected dump (dump lines always start with "|").
                if !line.is_empty() {
                    test.expected.push_str(line);
                    test.expected.push('\n');
                }
            }
            Section::Skip => {}
        }
    }
    if let Some(mut test) = current.take() {
        finalize(&mut test);
        tests.push(test);
    }
    tests
}

fn finalize(test: &mut TestCase) {
    // Section lines were accumulated with trailing newlines; the input
    // itself does not include the final one.
    if test.data.ends_with('\n') {
        test.data.pop();
    }
}

fn fragment_context_name(context: &str) -> QualName {
    match context.split_once(' ') {
        Some(("svg", local)) => QualName::new(None, ns!(svg), LocalName::from(local)),
        Some(("math", local)) => QualName::new(None, ns!(mathml), LocalName::from(local)),
        _ => QualName::new(None, ns!(html), LocalName::from(context)),
    }
}

/// Runs one test in one scripting mode; returns the actual dump.
fn run_test(test: &TestCase, scripting_enabled: bool) -> String {
    let opts = ParseOptions {
        scripting_enabled,
        ..ParseOptions::default()
    };
    match &test.fragment_context {
        None => {
            let parsed = oxidepage_dom::parse_document(&test.data, opts);
            dump_document(&parsed.tree)
        }
        Some(context) => {
            let parsed = oxidepage_dom::parse_fragment(
                &test.data,
                fragment_context_name(context),
                vec![],
                opts,
            );
            let tree: &DomTree = &parsed.tree;
            let root = tree
                .document_element()
                .expect("fragment parsing creates an html root");
            oxidepage_dom::dump::dump_tree(tree, root)
        }
    }
}

fn suite_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/html5lib-tests/tree-construction")
}

fn expectations_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/html5lib-expectations.txt")
}

fn load_expectations() -> BTreeSet<String> {
    match std::fs::read_to_string(expectations_path()) {
        Ok(content) => content
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(String::from)
            .collect(),
        Err(_) => BTreeSet::new(),
    }
}

#[test]
fn tree_construction() {
    let dir = suite_dir();
    assert!(
        dir.is_dir(),
        "vendored suite missing at {}; run `cargo xtask fetch-html5lib`",
        dir.display()
    );

    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read suite dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "dat"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no .dat files vendored");

    let expectations = load_expectations();
    let mut executed = 0usize;
    let mut passed = 0usize;
    let mut regressions: Vec<String> = Vec::new();
    let mut unexpected_passes: Vec<String> = Vec::new();
    let mut seen_expected: BTreeSet<String> = BTreeSet::new();

    for file in &files {
        let file_name = file.file_name().unwrap().to_string_lossy().into_owned();
        let content = std::fs::read_to_string(file).expect("read .dat file");
        for test in parse_dat(&file_name, &content) {
            let modes: &[bool] = match test.script_mode {
                Some(true) => &[true],
                Some(false) => &[false],
                None => &[true, false],
            };
            for &scripting in modes {
                let mode_id = format!(
                    "{}:{}",
                    test.id,
                    if scripting { "script-on" } else { "script-off" }
                );
                executed += 1;
                let actual = std::panic::catch_unwind(|| run_test(&test, scripting));
                let ok = matches!(&actual, Ok(dump) if *dump == test.expected);
                let expected_to_fail = expectations.contains(&mode_id);
                if expected_to_fail {
                    seen_expected.insert(mode_id.clone());
                }
                match (ok, expected_to_fail) {
                    (true, false) => passed += 1,
                    (true, true) => unexpected_passes.push(mode_id),
                    (false, true) => {}
                    (false, false) => {
                        let mut report = String::new();
                        let _ = writeln!(report, "=== {mode_id}");
                        let _ = writeln!(report, "--- input:\n{}", test.data);
                        match &actual {
                            Ok(dump) => {
                                let _ = writeln!(
                                    report,
                                    "--- expected:\n{}--- actual:\n{}",
                                    test.expected, dump
                                );
                            }
                            Err(_) => {
                                let _ = writeln!(report, "--- panicked");
                            }
                        }
                        regressions.push(report);
                    }
                }
            }
        }
    }

    let stale: Vec<&String> = expectations.difference(&seen_expected).collect();

    println!(
        "html5lib tree-construction: {passed}/{executed} passed, \
         {} known failures, {} regressions, {} unexpected passes",
        expectations.len(),
        regressions.len(),
        unexpected_passes.len()
    );

    let mut problems = String::new();
    if !regressions.is_empty() {
        let shown = regressions.len().min(10);
        let _ = writeln!(
            problems,
            "{} regression(s), first {shown}:\n{}",
            regressions.len(),
            regressions[..shown].join("\n")
        );
    }
    if !unexpected_passes.is_empty() {
        let _ = writeln!(
            problems,
            "{} test(s) pass but are listed in html5lib-expectations.txt; \
             remove them: {unexpected_passes:?}",
            unexpected_passes.len()
        );
    }
    if !stale.is_empty() {
        let _ = writeln!(
            problems,
            "{} expectation entr(y/ies) match no executed test: {stale:?}",
            stale.len()
        );
    }
    assert!(problems.is_empty(), "{problems}");
}
