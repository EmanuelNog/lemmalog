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
fn context_without_a_question_is_a_usage_error_not_an_empty_answer() {
    // `context` used to answer whatever empty query it got: a positional was ignored, so
    // `context "where does alice work"` printed the empty-query result — exit 0, no facts, no
    // warning — and with stdin on a pipe it blocked reading it. Measured 2026-11-22.
    let out = run(&["context"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stdout: {}", String::from_utf8_lossy(&out.stdout));
    assert!(err.contains("usage:"), "stderr: {err}");
}

#[test]
fn a_positional_question_is_the_question() {
    // A store the tool must actually read, so the assertion is about the query being used.
    let dir = std::env::temp_dir().join(format!("lemmalog-ctx-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let store = dir.join("ctx.snap");
    let seed = Command::new(env!("CARGO_BIN_EXE_lemmalog-cli"))
        .args(["observe", "--facts", "swordfish --status--> rotating"])
        .env("LEMMALOG_MCP_PATH", &store)
        .output()
        .expect("seed must run");
    assert!(seed.status.success(), "seed: {}", String::from_utf8_lossy(&seed.stderr));

    let out = Command::new(env!("CARGO_BIN_EXE_lemmalog-cli"))
        .args(["context", "swordfish status"])   // POSITIONAL, not --query
        .env("LEMMALOG_MCP_PATH", &store)
        .output()
        .expect("context must run");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(
        text.contains("swordfish --status--> rotating") || text.contains("swordfish, status, rotating"),
        "the positional must reach retrieval, got: {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
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

fn run_with_stdin_env(args: &[&str], input: &str, store: &std::path::Path) -> std::process::Output {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_lemmalog-cli"))
        .args(args)
        .env("LEMMALOG_MCP_PATH", store)
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
fn installing_an_empty_rule_set_is_a_usage_error() {
    // Measured 2026-11-22: `rules --rules ''` printed `installed b1; backfill derived +0 facts`,
    // creating a batch that derives nothing and cannot be told from a real one in `batches`.
    let store = std::env::temp_dir().join(format!("lemmalog-empty-{}.snap", std::process::id()));
    let out = run_with_stdin_env(&["rules", "--rules", "   "], "", &store);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr: {err}");
    assert!(err.contains("usage:"), "stderr: {err}");

    let list = run_with_stdin_env(&["batches"], "", &store);
    let bouts = String::from_utf8_lossy(&list.stdout);
    assert!(
        !bouts.contains("b1:"),
        "no batch may be created for empty rules: {bouts}"
    );
    let _ = std::fs::remove_file(&store);
}

#[test]
fn installing_the_same_rules_twice_is_a_no_op() {
    // Measured 2026-11-22: `rules --rules X` twice created b1 and b2 with identical text and
    // warned that the definitions UNION. Identical text derives identical facts, so a retry
    // must not accumulate batches.
    let store = std::env::temp_dir().join(format!("lemmalog-rules-{}.snap", std::process::id()));
    let rule = "zz(R) :- false(R).";
    let first = run_with_stdin_env(&["rules", "--rules", rule], "", &store);
    let fout = String::from_utf8_lossy(&first.stdout);
    assert!(fout.contains("installed"), "first install: {fout}");

    let second = run_with_stdin_env(&["rules", "--rules", rule], "", &store);
    let sout = String::from_utf8_lossy(&second.stdout);
    assert!(
        sout.contains("already installed"),
        "a repeat install must say so: {sout}"
    );
    assert!(
        !sout.contains("installed b2"),
        "a repeat install must not create a second batch: {sout}"
    );
    let list = run_with_stdin_env(&["batches"], "", &store);
    let bouts = String::from_utf8_lossy(&list.stdout);
    assert_eq!(
        bouts.matches("zz(R)").count(),
        1,
        "exactly one batch must hold the text: {bouts}"
    );
    let _ = std::fs::remove_file(&store);
}

#[test]
fn a_capitalized_entity_is_flagged_when_a_goal_reads_it_as_a_variable() {
    // The store allows `Svc` as an entity, but Datalog reads a bare capitalized word as a
    // variable, so `current(Svc, port, P)` silently returns every subject. Output stays the same;
    // the hint goes to stderr.
    let store = std::env::temp_dir().join(format!("lemmalog-case-{}.snap", std::process::id()));
    let seed = run_with_stdin_env(&["observe", "--facts"], "Svc --port--> 1\n", &store);
    assert_eq!(seed.status.code(), Some(0));

    let out = run_with_stdin_env(&["query", "--goal", "current(Svc, port, P)"], "", &store);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("read as a variable"),
        "must warn that Svc became a variable: {err}"
    );

    // quoted, it is an entity: no hint
    let quoted = run_with_stdin_env(&["query", "--goal", "current(\"Svc\", port, P)"], "", &store);
    let qerr = String::from_utf8_lossy(&quoted.stderr);
    assert!(
        !qerr.contains("read as a variable"),
        "a quoted entity must not be flagged: {qerr}"
    );
    let _ = std::fs::remove_file(&store);
}

#[test]
fn why_without_fact_is_a_usage_error_not_an_empty_parse() {
    let out = run(&["why"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr: {err}");
    assert!(err.contains("usage:"), "stderr: {err}");
    assert!(!err.contains("cannot parse"), "stderr: {err}");
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
