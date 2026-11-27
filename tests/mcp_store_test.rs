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
fn mcp_and_cli_give_the_same_answer_for_an_absent_fact() {
    // MEASURED 2026-11-22: the MCP `lemmalog_why` had its own parser and called `engine.why`
    // directly, so the same question answered "why: no such fact — not in the store: X" on the
    // CLI and "[unknown fact]" through MCP. One question, one answer.
    let dir = std::env::temp_dir();
    let store = dir.join(format!("lemmalog-mcp-parity-{}.snap", std::process::id()));
    let mut cli = Command::new(env!("CARGO_BIN_EXE_lemmalog-cli"))
        .args(["observe", "--facts"])
        .env("LEMMALOG_MCP_PATH", &store)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cli must spawn");
    cli.stdin.as_mut().expect("stdin").write_all(b"svc --status--> up\n").expect("write");
    assert!(cli.wait_with_output().expect("wait").status.success());

    let call = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"lemmalog_why","arguments":{"fact":"current(svc, status, absentvalue)"}}}"#;
    let out = run_server(&store, &[INIT, call]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("not in the store"),
        "the MCP answer must name the real cause: {stdout}"
    );
    assert!(
        !stdout.contains("[unknown fact]"),
        "the old collapsed message must be gone: {stdout}"
    );

    let cli_why = Command::new(env!("CARGO_BIN_EXE_lemmalog-cli"))
        .args(["why", "--fact", "current(svc, status, absentvalue)"])
        .env("LEMMALOG_MCP_PATH", &store)
        .output()
        .expect("cli why");
    let cli_text = String::from_utf8_lossy(&cli_why.stdout);
    assert!(
        cli_text.contains("not in the store"),
        "both surfaces must agree: {cli_text}"
    );
    let _ = std::fs::remove_file(&store);
}

#[test]
fn the_variable_hint_reaches_the_mcp_surface_too() {
    // The CLI got this hint first; Hermes itself calls the MCP tools, so the path that matters
    // most was the one without it. Both surfaces must say the same thing.
    let dir = std::env::temp_dir();
    let store = dir.join(format!("lemmalog-mcp-hint-{}.snap", std::process::id()));
    let mut cli = Command::new(env!("CARGO_BIN_EXE_lemmalog-cli"))
        .args(["observe", "--facts"])
        .env("LEMMALOG_MCP_PATH", &store)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cli must spawn");
    cli.stdin.as_mut().expect("stdin").write_all(b"Svc --port--> 1\n").expect("write");
    assert!(cli.wait_with_output().expect("wait").status.success());

    let ask = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"lemmalog_query","arguments":{"goal":"current(Svc, port, P)"}}}"#;
    let out = run_server(&store, &[INIT, ask]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("read as a variable"),
        "the MCP query must carry the hint: {stdout}"
    );

    let quoted = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"lemmalog_query","arguments":{"goal":"current(\"Svc\", port, P)"}}}"#;
    let out2 = run_server(&store, &[INIT, quoted]);
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(
        !stdout2.contains("read as a variable"),
        "a quoted entity must not be flagged over MCP: {stdout2}"
    );
    let _ = std::fs::remove_file(&store);
}

#[test]
fn rules_guards_hold_on_the_mcp_surface_too() {
    // These guards were first written in the CLI's branch, which left the MCP tool free to
    // create duplicate and empty batches. They live in AgentMemory now, so both surfaces share
    // one implementation and cannot drift.
    let dir = std::env::temp_dir();
    let store = dir.join(format!("lemmalog-mcp-rules-{}.snap", std::process::id()));
    let rule = "zz(R) :- false(R).";
    let install = |i: usize, text: &str| {
        format!(
            r#"{{"jsonrpc":"2.0","id":{i},"method":"tools/call","params":{{"name":"lemmalog_install_rules","arguments":{{"rules":"{text}"}}}}}}"#
        )
    };

    let first = run_server(&store, &[INIT, &install(2, rule)]);
    let fout = String::from_utf8_lossy(&first.stdout);
    assert!(fout.contains("installed b1"), "first install: {fout}");

    let second = run_server(&store, &[INIT, &install(3, rule)]);
    let sout = String::from_utf8_lossy(&second.stdout);
    assert!(
        sout.contains("already installed"),
        "a repeat install must be a no-op over MCP: {sout}"
    );
    assert!(
        sout.contains("\"content\"") && sout.contains("\"isError\":false"),
        "the no-op must be a proper tool result, not a bare string: {sout}"
    );

    let empty = run_server(&store, &[INIT, &install(4, "")]);
    let eout = String::from_utf8_lossy(&empty.stdout);
    assert!(
        eout.contains("\"isError\":true") && eout.contains("nothing to install"),
        "empty rules must be refused over MCP: {eout}"
    );
    let _ = std::fs::remove_file(&store);
}

#[test]
fn a_checkpoint_from_an_earlier_run_is_flagged_not_called_up_to_date() {
    // The change feed is in memory and the epoch counter restarts with the process, so a
    // checkpoint saved by an earlier server run is incomparable. Answering "no changes since
    // that epoch" would tell a resyncing agent the store is current when nothing is known.
    let dir = std::env::temp_dir();
    let store = dir.join(format!("lemmalog-mcp-epoch-{}.snap", std::process::id()));
    let mut cli = Command::new(env!("CARGO_BIN_EXE_lemmalog-cli"))
        .args(["observe", "--facts"])
        .env("LEMMALOG_MCP_PATH", &store)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cli must spawn");
    cli.stdin.as_mut().expect("stdin").write_all(b"a --status--> one\n").expect("write");
    assert!(cli.wait_with_output().expect("wait").status.success());

    let ahead = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"lemmalog_changes","arguments":{"since":99}}}"#;
    let out = run_server(&store, &[INIT, ahead]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("AHEAD of this store's current epoch"),
        "an out-of-range checkpoint must be flagged: {text}"
    );
    assert!(
        !text.contains("no changes since that epoch"),
        "it must not claim the caller is up to date: {text}"
    );

    // A normal checkpoint still answers plainly.
    let normal = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"lemmalog_changes","arguments":{"since":0}}}"#;
    let out2 = run_server(&store, &[INIT, normal]);
    let text2 = String::from_utf8_lossy(&out2.stdout);
    assert!(!text2.contains("AHEAD"), "since=0 is never ahead: {text2}");
    let _ = std::fs::remove_file(&store);
}

#[test]
fn a_timestamp_checkpoint_survives_a_process_restart() {
    // The trigger for resync is a context reset, which starts a NEW process — and an epoch
    // checkpoint cannot cross one. A timestamp checkpoint can, because every `edge` row carries
    // its own validity window: opened = vf > ts, closed = vt > ts and vt != MAX. This test does
    // the whole thing across three separate processes.
    let dir = std::env::temp_dir();
    let store = dir.join(format!("lemmalog-mcp-tscp-{}.snap", std::process::id()));
    let observe = |facts: &str| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_lemmalog-cli"))
            .args(["observe", "--facts"])
            .env("LEMMALOG_MCP_PATH", &store)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("cli must spawn");
        c.stdin.as_mut().expect("stdin").write_all(facts.as_bytes()).expect("write");
        assert!(c.wait_with_output().expect("wait").status.success());
    };
    let changes_since = |ts: i64| {
        let call = format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"lemmalog_changes","arguments":{{"since_ts":{ts}}}}}}}"#
        );
        let out = run_server(&store, &[INIT, &call]);
        String::from_utf8_lossy(&out.stdout).to_string()
    };

    // Process 1: a first fact.
    observe("a --status--> one\n");
    let after_first = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    std::thread::sleep(std::time::Duration::from_millis(1100));

    // Process 2: supersede it and add another.
    observe("a --status--> two\nb --status--> new\n");

    // Process 3: ask with the checkpoint taken after process 1.
    let text = changes_since(after_first);
    // Envelope first: a `return` that escapes the dispatcher yields a bare string result, which
    // still CONTAINS the right text and still passes a text-only assertion.
    assert!(
        text.contains("\"content\"") && text.contains("\"isError\":false"),
        "the response must be a proper tool result: {text}"
    );
    assert!(
        !text.contains("\"result\":\""),
        "a bare string result means the envelope was skipped: {text}"
    );
    assert!(text.contains("+ edge(a, status, two"), "the addition must appear: {text}");
    assert!(text.contains("+ edge(b, status, new"), "the second addition must appear: {text}");
    assert!(text.contains("- edge(a, status, one"), "the retraction must appear: {text}");
    assert!(!text.contains("+ edge(a, status, two, 0"), "no placeholder rows: {text}");

    // A checkpoint taken now reports nothing new.
    // The response is JSON on stdout, so newlines arrive escaped (`\n` as two characters):
    // take the digits after `now_ts=` rather than splitting on a newline that is not there.
    let now_ts: i64 = text
        .split("now_ts=")
        .nth(1)
        .expect("response carries now_ts")
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .expect("now_ts is an integer");
    let text2 = changes_since(now_ts);
    assert!(
        text2.contains("no edges opened or closed"),
        "a fresh checkpoint must be quiet: {text2}"
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

#[test]
fn a_closed_fact_explains_its_absence_on_both_surfaces() {
    // The CLI and MCP must both answer `false` AND say that the value has
    // history — a bare `false` reads as "never known", which is a different
    // claim and sends the agent away from the tombstone.
    let dir = std::env::temp_dir();
    let store = dir.join(format!("lemmalog-mcp-history-{}.snap", std::process::id()));
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
        .write_all(b"svc --version--> 2.0.0\n")
        .expect("write");
    assert!(cli.wait_with_output().expect("wait").status.success());

    let close = Command::new(env!("CARGO_BIN_EXE_lemmalog-cli"))
        .args(["close", "--facts", "svc --version--> 2.0.0"])
        .env("LEMMALOG_MCP_PATH", &store)
        .output()
        .expect("cli close");
    assert!(close.status.success(), "{close:?}");

    let q = Command::new(env!("CARGO_BIN_EXE_lemmalog-cli"))
        .args([
            "query",
            "--goal",
            r#"current("svc", version, "2.0.0")"#,
        ])
        .env("LEMMALOG_MCP_PATH", &store)
        .output()
        .expect("cli query");
    let stdout = String::from_utf8_lossy(&q.stdout);
    let stderr = String::from_utf8_lossy(&q.stderr);
    assert!(stdout.contains("false"), "stdout: {stdout}");
    assert!(
        stderr.contains("1 tombstone"),
        "the CLI must explain the absence: {stderr}"
    );

    let goal = r#"current("svc", version, "2.0.0")"#;
    let ask = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"lemmalog_query","arguments":{{"goal":{}}}}}}}"#,
        serde_json::to_string(goal).expect("goal json")
    );
    let out = run_server(&store, &[INIT, &ask]);
    let mcp = String::from_utf8_lossy(&out.stdout);
    assert!(mcp.contains("false"), "{mcp}");
    assert!(
        mcp.contains("1 tombstone"),
        "the MCP query must carry the same hint: {mcp}"
    );
    let _ = std::fs::remove_file(&store);
}
