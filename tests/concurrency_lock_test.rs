//! Cross-PROCESS write safety: two real `lemmalog-cli` processes (and a live
//! MCP server racing the CLI) share one snapshot.
//!
//! Regression for the two measured defects:
//!
//!   br-gx5.15 — LOST UPDATE. Two `observe` processes loaded at the same stamp
//!   and both saved; one fact vanished with no exit 3 and no log line. The
//!   stamp check was a check-then-write TOCTOU, and nothing serialised
//!   load-modify-save. The fix is an exclusive `flock` on `<snapshot>.lock`
//!   (the SAME lock the fork's sweep/export/import scripts take) around the
//!   whole cycle, plus a stamp re-check under the lock.
//!
//!   br-gx5.16 — MODE DOWNGRADE. `write_atomic`'s temp file took the default
//!   mode, so every save rewrote a 0600 store as 0644.
//!
//! These tests spawn REAL processes: a lock that only works in-process would
//! pass a threaded test and fail here.
//!
//! Run: cargo test --release --features mcp --test concurrency_lock_test

#![cfg(feature = "mcp")]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn cli() -> &'static str {
    env!("CARGO_BIN_EXE_lemmalog-cli")
}

fn mcp() -> &'static str {
    env!("CARGO_BIN_EXE_lemmalog-mcp")
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("llconc-{}-{}-{}", tag, std::process::id(), nanos()));
    fs::create_dir_all(&d).unwrap();
    d
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'\''"#))
}

/// Run one CLI command to completion and return (exit code, stdout, stderr).
fn run_cli(snap: &Path, args: &[&str], stdin: Option<&str>) -> (i32, String, String) {
    let mut c = Command::new(cli());
    c.args(args)
        .env("LEMMALOG_MCP_PATH", snap)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = c.spawn().expect("spawn lemmalog-cli");
    if let Some(text) = stdin {
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn observe(snap: &Path, facts: &str) -> (i32, String, String) {
    run_cli(snap, &["observe"], Some(facts))
}

/// Spawn `observe` behind a barrier file so every process enters
/// load-modify-save at the same instant. `sh -c 'until [ -e go ]; do sleep
/// 0.002; done; exec cli observe'` — the facts go through stdin, so nothing has
/// to be quoted into the shell.
fn spawn_behind_barrier(snap: &Path, go: &Path, facts: &str) -> Child {
    let script = format!(
        "while [ ! -e {go} ]; do sleep 0.002; done; exec {cli} observe",
        go = sh_quote(&go.to_string_lossy()),
        cli = sh_quote(cli()),
    );
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .env("LEMMALOG_MCP_PATH", snap)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn observer");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(facts.as_bytes())
        .unwrap();
    child
}

/// br-gx5.15. `K` real processes observe distinct facts at the same moment, on
/// the same snapshot. Every fact must survive, and nobody may exit 3 (a loud
/// refusal is better than a silent clobber, but with a real lock it must not
/// happen at all). Repeated rounds: a single lucky pass proves nothing.
#[test]
fn concurrent_cli_observers_keep_every_fact() {
    const K: usize = 6;
    const ROUNDS: usize = 5;

    for round in 0..ROUNDS {
        let dir = scratch(&format!("lost{round}"));
        let snap = dir.join("memory.snap");
        // A store with real weight: load/save take long enough that the
        // pre-fix window (stamp check, then write) is reliably hit.
        let seed: String = (0..200)
            .map(|i| format!("seeded{i} --note--> bulk\n"))
            .collect();
        let (code, out, err) = observe(&snap, &seed);
        assert_eq!(code, 0, "seed failed: {out}{err}");

        let go = dir.join("go");
        let mut kids: Vec<(usize, Child)> = (0..K)
            .map(|i| {
                (
                    i,
                    spawn_behind_barrier(&snap, &go, &format!("proc{i} --on--> store\n")),
                )
            })
            .collect();
        fs::write(&go, b"go").unwrap(); // release the barrier

        let mut failures: Vec<String> = Vec::new();
        for (i, child) in kids.drain(..) {
            let out = child.wait_with_output().unwrap();
            let code = out.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            if code != 0 {
                failures.push(format!(
                    "round {round} proc{i}: exit {code} — {}{}",
                    stdout.trim(),
                    stderr.trim()
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "a writer failed:\n{}",
            failures.join("\n")
        );

        let (code, dump, err) = run_cli(&snap, &["dump", "--pred", "current"], None);
        assert_eq!(code, 0, "dump failed: {err}");
        let missing: Vec<String> = (0..K)
            .map(|i| format!("current(proc{i}, on, store)"))
            .filter(|want| !dump.lines().any(|l| l.trim() == want))
            .collect();
        assert!(
            missing.is_empty(),
            "round {round}: LOST UPDATE — {} of {K} concurrent facts vanished: {missing:?}\n\
             dump was:\n{dump}",
            missing.len()
        );
        fs::remove_dir_all(&dir).ok();
    }
}

/// A live MCP server and the CLI racing on one snapshot. The server keeps an
/// in-memory copy, so its load-modify-save must be serialised against the CLI's
/// too — otherwise whichever saves last erases the other's facts.
#[test]
fn mcp_server_and_cli_keep_each_others_facts() {
    const N: usize = 8;
    let dir = scratch("mcprace");
    let snap = dir.join("memory.snap");
    let (code, out, err) = observe(&snap, "seed --on--> store\n");
    assert_eq!(code, 0, "seed failed: {out}{err}");

    let mut server = Server::new_server(&snap);
    // CLI writer thread and server writer, interleaved as fast as each can go.
    let cli_snap = snap.clone();
    let writer = std::thread::spawn(move || {
        let mut codes = Vec::new();
        for i in 0..N {
            let (code, out, err) = observe(&cli_snap, &format!("cli{i} --on--> store\n"));
            codes.push((i, code, format!("{}{}", out.trim(), err.trim())));
        }
        codes
    });
    for i in 0..N {
        let text = server.tool(
            "lemmalog_observe",
            &format!("{{\"facts\":\"mcp{i} --on--> store\"}}"),
        );
        assert!(
            !text.contains("isError\":true"),
            "server observe {i} failed: {text}"
        );
    }
    let cli_results = writer.join().unwrap();
    for (i, code, detail) in &cli_results {
        assert_eq!(*code, 0, "cli writer {i} failed: {detail}");
    }
    server.stop();

    let (code, dump, err) = run_cli(&snap, &["dump", "--pred", "current"], None);
    assert_eq!(code, 0, "dump failed: {err}");
    let missing: Vec<String> = (0..N)
        .flat_map(|i| {
            [
                format!("current(cli{i}, on, store)"),
                format!("current(mcp{i}, on, store)"),
            ]
        })
        .filter(|want| !dump.lines().any(|l| l.trim() == want))
        .collect();
    assert!(
        missing.is_empty(),
        "LOST UPDATE across surfaces — missing {missing:?}\ndump was:\n{dump}"
    );
    fs::remove_dir_all(&dir).ok();
}

/// br-gx5.16: an engine save over an existing 0600 store must leave it 0600.
/// The store holds the user's whole assertion history; the mode is the only
/// thing keeping it private on a shared host.
#[cfg(unix)]
#[test]
fn cli_save_keeps_a_0600_store_private() {
    use std::os::unix::fs::PermissionsExt;

    let dir = scratch("mode");
    let snap = dir.join("memory.snap");
    let (code, out, err) = observe(&snap, "alice --works_at--> acme\n");
    assert_eq!(code, 0, "seed failed: {out}{err}");

    fs::set_permissions(&snap, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        fs::metadata(&snap).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let (code, out, err) = observe(&snap, "bob --manager--> carol\n");
    assert_eq!(code, 0, "second observe failed: {out}{err}");
    let mode = fs::metadata(&snap).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "MODE DOWNGRADE — a save rewrote the 0600 store as {mode:03o}"
    );
    let leftovers: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp."))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );
    fs::remove_dir_all(&dir).ok();
}

/// Minimal stdio JSON-RPC driver — enough to be a real second writer.
struct Server {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl Server {
    fn new_server(snap: &Path) -> Server {
        let mut child = Command::new(mcp())
            .env("LEMMALOG_MCP_PATH", snap)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn lemmalog-mcp");
        let reader = BufReader::new(child.stdout.take().unwrap());
        let mut s = Server {
            child,
            reader,
            next_id: 0,
        };
        s.rpc(
            "initialize",
            "{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"clientInfo\":{\"name\":\"conc\",\"version\":\"1\"}}",
        );
        s
    }

    fn rpc(&mut self, method: &str, params: &str) -> String {
        self.next_id += 1;
        let id = self.next_id;
        let line = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"{method}\",\"params\":{params}}}\n"
        );
        self.child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(line.as_bytes())
            .unwrap();
        self.child.stdin.as_mut().unwrap().flush().unwrap();
        loop {
            let mut buf = String::new();
            let n = self.reader.read_line(&mut buf).unwrap();
            assert!(n > 0, "server died during {method}");
            if buf.contains(&format!("\"id\":{id}")) {
                return buf;
            }
        }
    }

    fn tool(&mut self, name: &str, args: &str) -> String {
        self.rpc(
            "tools/call",
            &format!("{{\"name\":\"{name}\",\"arguments\":{args}}}"),
        )
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[allow(dead_code)]
fn settle() {
    std::thread::sleep(Duration::from_millis(50));
}
