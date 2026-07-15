//! Smoke test for `oxidepage eval` (Phase 2 exit criterion: eval works on
//! local HTML).

use std::process::Command;

fn write_temp(name: &str, contents: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(name);
    std::fs::write(&path, contents).expect("write temp html");
    path
}

#[test]
fn eval_loads_html_runs_scripts_and_prints_result() {
    let html = r#"<!DOCTYPE html>
        <html><head><title>Smoke</title></head><body>
        <script>
          document.title = document.title + ' + script';
          setTimeout(() => { document.title += ' + timer'; }, 1);
        </script>
        </body></html>"#;
    let path = write_temp("oxidepage-cli-smoke.html", html);

    let output = Command::new(env!("CARGO_BIN_EXE_oxidepage"))
        .args(["eval", path.to_str().unwrap()])
        .output()
        .expect("run oxidepage eval");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "Smoke + script + timer"
    );
}

#[test]
fn eval_reports_missing_file() {
    let output = Command::new(env!("CARGO_BIN_EXE_oxidepage"))
        .args(["eval", "/nonexistent/nope.html"])
        .output()
        .expect("run oxidepage eval");
    assert!(!output.status.success());
}

#[test]
fn eval_custom_expression() {
    let path = write_temp(
        "oxidepage-cli-expr.html",
        "<html><body><p>a</p><p>b</p></body></html>",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_oxidepage"))
        .args([
            "eval",
            path.to_str().unwrap(),
            "document.querySelectorAll('p').length",
        ])
        .output()
        .expect("run oxidepage eval");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "2");
}
