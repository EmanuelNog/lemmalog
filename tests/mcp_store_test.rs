//! The MCP server must never trade an unreadable store for a fresh one.
//!
//! MEASURED 2026-11-22: with a 36-byte file that is not a snapshot, the server logged the load
//! failure to stderr and then served a fresh engine — `lemmalog_query` answered
//! "(no answers — no relation named `current` …)" with `isError:false`, which is a false claim
//! about the schema, and `lemmalog_observe` + `lemmalog_save` replaced the file with a 168-byte
//! fresh store, both answering `isError:false`. The CLI already refuses to start fresh
//! ("refusing to start fresh — that would overwrite <path>"); the server now does the same.

#![cfg(feature = "mcp")]

use std::io::Write;
use std::process::{Command, Stdio};

fn run_server(store: &std::path::Path, requests: &[&str]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_lemmalog-mcp"))
        .env("LEMMALOG_MCP_PATH", store)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("server must spawn");
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for req in requests {
            writeln!(stdin, "{req}").expect("write request");
        }
    }
    child.wait_with_output().expect("wait")
}

const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#;
const OBSERVE: &str = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"lemmalog_observe","arguments":{"facts":"probe --status--> wrote"}}}"#;
const SAVE: &str = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"lemmalog_save","arguments":{}}}"#;

#[test]
fn an_unreadable_store_is_refused_and_left_untouched() {
    let dir = std::env::temp_dir();
    let store = dir.join(format!("lemmalog-mcp-corrupt-{}.snap", std::process::id()));
    let original = b"ORIGINAL-CONTENT-MUST-SURVIVE";
    std::fs::write(&store, original).expect("seed the non-snapshot file");

    let out = run_server(&store, &[INIT, OBSERVE, SAVE]);
    let err = String::from_utf8_lossy(&out.stderr);

    assert_eq!(
        out.status.code(),
        Some(3),
        "the server must refuse to start fresh; stderr: {err}"
    );
    assert!(
        err.contains("refusing to start fresh"),
        "the reason must say why: {err}"
    );
    // The whole point: the file is exactly as it was.
    let after = std::fs::read(&store).expect("store still present");
    assert_eq!(
        after, original,
        "an unreadable store must never be overwritten"
    );
    let _ = std::fs::remove_file(&store);
}

#[test]
fn a_healthy_store_still_serves() {
    let dir = std::env::temp_dir();
    let store = dir.join(format!("lemmalog-mcp-healthy-{}.snap", std::process::id()));
    let seed = format!("{INIT}\n{OBSERVE}\n{SAVE}\n");
    // Create a valid store through the CLI first.
    let mut cli = Command::new(env!("CARGO_BIN_EXE_lemmalog-cli"))
        .args(["observe", "--facts"])
        .env("LEMMALOG_MCP_PATH", &store)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cli must spawn");
    cli.stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"ok --status--> fine\n")
        .expect("write");
    assert!(cli.wait_with_output().expect("wait").status.success());

    let out = run_server(&store, &[INIT, OBSERVE]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("refusing to start fresh"),
        "a healthy store must load: {err}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("added=1"),
        "the healthy store must accept a write: {stdout}"
    );
    let _ = std::fs::remove_file(&store);
    let _ = seed;
}
