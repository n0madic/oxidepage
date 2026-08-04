//! Smoke tests for `oxidepage dump`: the two `--format` values, the default,
//! `-o` for either of them, and the errors around them.

use std::process::Command;

fn write_temp(name: &str, contents: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(name);
    std::fs::write(&path, contents).expect("write temp html");
    path
}

fn temp_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(name)
}

const HTML: &str = "<!DOCTYPE html><html><body><p>hi</p></body></html>";

fn run_dump(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_oxidepage"))
        .arg("dump")
        .args(args)
        .output()
        .expect("run oxidepage dump")
}

fn stdout_of(result: &std::process::Output) -> String {
    assert!(
        result.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8_lossy(&result.stdout).into_owned()
}

#[test]
fn defaults_to_the_layout_tree() {
    let input = write_temp("oxidepage-cli-dump-default.html", HTML);
    let stdout = stdout_of(&run_dump(&[input.to_str().unwrap()]));

    // The box tree, not JSON.
    assert!(stdout.contains("BLOCK"), "unexpected dump: {stdout}");
    assert!(!stdout.trim_start().starts_with('{'), "got JSON: {stdout}");
}

#[test]
fn explicit_layout_format_matches_the_default() {
    let input = write_temp("oxidepage-cli-dump-layout.html", HTML);
    let path = input.to_str().unwrap();

    let implicit = stdout_of(&run_dump(&[path]));
    let explicit = stdout_of(&run_dump(&[path, "--format", "layout"]));
    assert_eq!(implicit, explicit);
}

#[test]
fn display_list_format_prints_json() {
    let input = write_temp("oxidepage-cli-dump-dl.html", HTML);
    let stdout = stdout_of(&run_dump(&[
        input.to_str().unwrap(),
        "--format",
        "display-list",
    ]));

    assert!(stdout.trim_start().starts_with('{'), "not JSON: {stdout}");
    assert!(stdout.contains("\"items\""), "unexpected JSON: {stdout}");
}

#[test]
fn output_flag_writes_either_format_to_a_file() {
    let input = write_temp("oxidepage-cli-dump-out.html", HTML);
    let path = input.to_str().unwrap();

    for (format, out_name) in [
        ("layout", "oxidepage-cli-dump-out.txt"),
        ("display-list", "oxidepage-cli-dump-out.json"),
    ] {
        let output = temp_path(out_name);
        let _ = std::fs::remove_file(&output);
        let result = run_dump(&[
            path,
            "--format",
            format,
            "-o",
            output.to_str().unwrap(),
            "--quiet",
        ]);
        assert!(
            result.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(result.stdout.is_empty(), "-o should not also print");
        let written = std::fs::read_to_string(&output).expect("read dump output");
        assert!(!written.is_empty(), "{format} dump is empty");
    }
}

#[test]
fn unknown_format_is_a_usage_error() {
    let input = write_temp("oxidepage-cli-dump-badfmt.html", HTML);
    let result = run_dump(&[input.to_str().unwrap(), "--format", "boxes"]);

    assert_eq!(result.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("--format"),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn missing_input_is_a_usage_error() {
    let result = run_dump(&[]);
    assert_eq!(result.status.code(), Some(2));
}

/// The old two-command surface is gone, not silently aliased.
#[test]
fn old_subcommand_names_are_rejected() {
    for old in ["dump-layout", "dump-display-list"] {
        let result = Command::new(env!("CARGO_BIN_EXE_oxidepage"))
            .arg(old)
            .arg("x.html")
            .output()
            .expect("run oxidepage");
        assert_eq!(result.status.code(), Some(2), "`{old}` should not run");
    }
}
