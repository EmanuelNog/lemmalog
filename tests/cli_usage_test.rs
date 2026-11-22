//! CLI usage errors must be usage errors.
//!
//! Found by probing the binary directly (tools/probe-bugs.py): `rmrules` without `--id` PANICKED
//! (`thread 'main' panicked at src/bin/lemmalog-cli.rs:300:42: --id <batch>`), and `query`
//! without `--goal` reported `expected predicate, found Some(Punct("."))`. Both read like engine
//! corruption for what is a typo on the command line. These tests run the real binary.
#![cfg(feature = "mcp")]

use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_lemmalog-cli"))
        .args(args)
        // never the live store: these cases exit before touching it
        .env("LEMMALOG_MCP_PATH", "/nonexistent/usage-test.snap")
        .output()
        .expect("binary must run")
}

#[test]
fn rmrules_without_id_is_a_usage_error_not_a_panic() {
    let out = run(&["rmrules"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr: {err}");
    assert!(err.contains("usage:"), "stderr: {err}");
    assert!(!err.contains("panicked"), "a missing flag must not panic: {err}");
}

#[test]
fn query_without_goal_is_a_usage_error_not_a_parse_error() {
    let out = run(&["query"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr: {err}");
    assert!(err.contains("usage:"), "stderr: {err}");
    assert!(
        !err.contains("parse error"),
        "an absent goal is not a parse error: {err}"
    );
}

fn run_with_stdin(args: &[&str], input: &str) -> std::process::Output {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_lemmalog-cli"))
        .args(args)
        .env("LEMMALOG_MCP_PATH", "/nonexistent/usage-test.snap")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("binary must spawn");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait")
}

#[test]
fn a_typo_in_ts_is_a_usage_error_not_a_silent_default() {
    // Measured 2026-11-22: `--ts abc` stored at "now" and printed added=1, so a mis-dated write
    // looked successful. A timestamp that cannot be parsed must stop the write.
    let out = run_with_stdin(&["observe", "--facts", "--ts", "abc"], "svc --status--> up\n");
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr: {err}");
    assert!(err.contains("epoch seconds"), "stderr: {err}");
}

#[test]
fn a_future_ts_is_refused_because_it_freezes_the_slot() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let future = (now + 86_400).to_string();
    let out = run_with_stdin(
        &["observe", "--facts", "--ts", &future],
        "svc --status--> up\n",
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr: {err}");
    assert!(err.contains("future"), "stderr: {err}");
}

#[test]
fn a_past_ts_still_writes() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let past = (now - 3600).to_string();
    let store = std::env::temp_dir().join(format!("lemmalog-ts-{}.snap", std::process::id()));
    let mut child = Command::new(env!("CARGO_BIN_EXE_lemmalog-cli"))
        .args(["observe", "--facts", "--ts", &past])
        .env("LEMMALOG_MCP_PATH", &store)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    use std::io::Write;
    child.stdin.as_mut().unwrap().write_all(b"svc --status--> up\n").unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "stdout: {stdout}");
    assert!(stdout.contains("added=1"), "stdout: {stdout}");
    let _ = std::fs::remove_file(&store);
}
