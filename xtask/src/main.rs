//! Workspace automation tasks, invoked as `cargo xtask <command>`.
//!
//! Commands that back later phases (`codegen`, `wpt`, `reftest`) are
//! scaffolded here and error out until their phase lands.

mod golden;
mod nodeharness;
mod playwright;
mod puppeteer;
mod reftest;
mod testserver;
mod wpt;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const HTML5LIB_TESTS_REPO: &str = "https://github.com/html5lib/html5lib-tests";

/// Pinned revision: the last html5lib-tests commit that still carries the
/// tree-construction suite (it was subsequently moved into WPT). Pinning
/// also keeps the vendored corpus reproducible.
const HTML5LIB_TESTS_REV: &str = "9329e64694e7835d0dcff9811e22856ef6ad16f9";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("fetch-html5lib") => fetch_html5lib(),
        Some("codegen") => codegen(args.iter().any(|a| a == "--check")),
        Some("fetch-wpt") => wpt::fetch(&workspace_root()),
        Some("wpt") => {
            let update = args.iter().any(|a| a == "--update");
            let filter = args
                .iter()
                .position(|a| a == "--filter")
                .and_then(|i| args.get(i + 1))
                .map(String::as_str);
            wpt::run(&workspace_root(), update, filter)
        }
        Some("wpt-single") => match args.get(1) {
            Some(test) => wpt::run_single(&workspace_root(), Path::new(test)),
            None => {
                eprintln!("xtask wpt-single: missing test file");
                ExitCode::from(2)
            }
        },
        Some("golden") => {
            let update = args.iter().any(|a| a == "--update");
            let filter = args
                .iter()
                .position(|a| a == "--filter")
                .and_then(|i| args.get(i + 1))
                .map(String::as_str);
            golden::run(&workspace_root(), update, filter)
        }
        Some("reftest") => {
            let filter = args
                .iter()
                .position(|a| a == "--filter")
                .and_then(|i| args.get(i + 1))
                .map(String::as_str);
            reftest::run(&workspace_root(), filter)
        }
        Some("puppeteer") => {
            let update = args.iter().any(|a| a == "--update");
            let filter = args
                .iter()
                .position(|a| a == "--filter")
                .and_then(|i| args.get(i + 1))
                .map(String::as_str);
            puppeteer::run(&workspace_root(), update, filter)
        }
        Some("playwright") => {
            let update = args.iter().any(|a| a == "--update");
            let filter = args
                .iter()
                .position(|a| a == "--filter")
                .and_then(|i| args.get(i + 1))
                .map(String::as_str);
            playwright::run(&workspace_root(), update, filter)
        }
        Some(other) => {
            eprintln!("xtask: unknown command `{other}`");
            usage();
            ExitCode::from(2)
        }
        None => {
            usage();
            ExitCode::from(2)
        }
    }
}

fn usage() {
    eprintln!(
        "usage: cargo xtask <command>\n\n\
         commands:\n\
         \x20 fetch-html5lib     vendor html5lib-tests tree-construction data into tests/\n\
         \x20 codegen [--check]  regenerate crates/bindings/src/generated.rs from WebIDL\n\
         \x20 fetch-wpt          vendor WPT subsets (resources, dom/nodes, dom/events)\n\
         \x20 wpt [--update] [--filter <substr>]\n\
         \x20                    run vendored WPT subsets against expectations\n\
         \x20 golden [--update] [--filter <substr>]\n\
         \x20                    compare display-list JSON goldens (tests/goldens)\n\
         \x20 reftest [--filter <substr>]\n\
         \x20                    run pixel-compare reftests (tests/reftests)\n\
         \x20 puppeteer [--update] [--filter <substr>]\n\
         \x20                    drive the CDP endpoint with a real Puppeteer\n\
         \x20                    (tests/automation; needs a Node toolchain)\n\
         \x20 playwright [--update] [--filter <substr>]\n\
         \x20                    drive the CDP endpoint with a real Playwright\n\
         \x20                    (tests/playwright; needs a Node toolchain)"
    );
}

/// Regenerates the WebIDL bindings glue. With `--check`, verifies the
/// checked-in file is up to date instead of writing (CI freshness gate).
fn codegen(check: bool) -> ExitCode {
    let root = workspace_root();
    let idl_dir = root.join("crates/idl/webidl");
    let out_path = root.join("crates/bindings/src/generated.rs");

    let generated = match oxidepage_idl::generate(&idl_dir) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("xtask codegen: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Normalize through rustfmt so the checked-in file passes `cargo fmt --check`.
    let formatted = rustfmt(&generated).unwrap_or(generated);

    if check {
        match std::fs::read_to_string(&out_path) {
            Ok(existing) if existing == formatted => {
                println!("codegen: {} is up to date", out_path.display());
                ExitCode::SUCCESS
            }
            _ => {
                eprintln!(
                    "codegen: {} is stale; run `cargo xtask codegen`",
                    out_path.display()
                );
                ExitCode::FAILURE
            }
        }
    } else {
        if let Err(e) = std::fs::write(&out_path, &formatted) {
            eprintln!("xtask codegen: writing {}: {e}", out_path.display());
            return ExitCode::FAILURE;
        }
        println!("codegen: wrote {}", out_path.display());
        ExitCode::SUCCESS
    }
}

/// Runs `rustfmt` over a generated source string, if rustfmt is available.
fn rustfmt(source: &str) -> Option<String> {
    use std::io::Write;
    let mut child = Command::new("rustfmt")
        .args(["--edition", "2024", "--emit", "stdout"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    child
        .stdin
        .take()
        .expect("stdin piped")
        .write_all(source.as_bytes())
        .ok()?;
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

fn workspace_root() -> PathBuf {
    // xtask lives at <root>/xtask, so the manifest dir's parent is the root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask crate must live inside the workspace")
        .to_path_buf()
}

/// Fetch html5lib-tests at the pinned revision and vendor the
/// tree-construction suite into `tests/html5lib-tests/tree-construction`.
fn fetch_html5lib() -> ExitCode {
    let root = workspace_root();
    let dest = root.join("tests/html5lib-tests/tree-construction");
    let tmp = root.join("target/html5lib-tests-checkout");

    if tmp.exists()
        && let Err(e) = std::fs::remove_dir_all(&tmp)
    {
        eprintln!("xtask fetch-html5lib: cannot clean {}: {e}", tmp.display());
        return ExitCode::FAILURE;
    }
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        eprintln!("xtask fetch-html5lib: cannot create {}: {e}", tmp.display());
        return ExitCode::FAILURE;
    }

    // Shallow-fetch the pinned revision (plain `clone --depth 1` would miss
    // it: the suite was removed from the branch tip).
    let steps: &[&[&str]] = &[
        &["init", "--quiet"],
        &["remote", "add", "origin", HTML5LIB_TESTS_REPO],
        &[
            "fetch",
            "--quiet",
            "--depth",
            "1",
            "origin",
            HTML5LIB_TESTS_REV,
        ],
        &["checkout", "--quiet", "FETCH_HEAD"],
    ];
    for args in steps {
        let status = Command::new("git").args(*args).current_dir(&tmp).status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => {
                eprintln!("xtask fetch-html5lib: git {} exited with {s}", args[0]);
                return ExitCode::FAILURE;
            }
            Err(e) => {
                eprintln!("xtask fetch-html5lib: failed to run git: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    let src = tmp.join("tree-construction");
    if let Err(e) = std::fs::remove_dir_all(&dest)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("xtask fetch-html5lib: cannot clean {}: {e}", dest.display());
        return ExitCode::FAILURE;
    }
    if let Err(e) = std::fs::create_dir_all(&dest) {
        eprintln!(
            "xtask fetch-html5lib: cannot create {}: {e}",
            dest.display()
        );
        return ExitCode::FAILURE;
    }

    // Vendor only the flat *.dat files. The `scripted/` subdirectory needs a
    // JS-enabled document.write, which is out of scope (design doc, section 12).
    let entries = match std::fs::read_dir(&src) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("xtask fetch-html5lib: cannot read {}: {e}", src.display());
            return ExitCode::FAILURE;
        }
    };
    let mut copied = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "dat") {
            let file_name = path.file_name().expect("*.dat entry has a file name");
            if let Err(e) = std::fs::copy(&path, dest.join(file_name)) {
                eprintln!("xtask fetch-html5lib: copy {} failed: {e}", path.display());
                return ExitCode::FAILURE;
            }
            copied += 1;
        }
    }

    let _ = std::fs::remove_dir_all(&tmp);
    println!("vendored {copied} .dat files into {}", dest.display());
    ExitCode::SUCCESS
}
