//! lemmalog-cli: headless memory access on the same snapshot the MCP
//! server uses (`LEMMALOG_MCP_PATH`). For contexts where MCP tools are
//! unreachable — CLI sub-agents that don't inherit MCP connections,
//! scripts, cron jobs — every mutation here is visible to the next MCP
//! load and vice versa.
//!
//!   lemmalog-cli observe  --facts 'alice --works_at--> acme'
//!   lemmalog-cli query   --goal 'current("alice", R, O)'
//!                        (a fully-ground goal prints true/false)
//!   lemmalog-cli retract --facts 'alice --works_at--> acme'
//!   lemmalog-cli context --query 'where does alice work'
//!   lemmalog-cli why     --fact 'reports_to(alice, carol)'
//!   lemmalog-cli rules   --rules 'reach(X,Z) :- current(X,"dep",Y), reach(Y,Z).'
//!   lemmalog-cli dump    [--pred current]
//!
//! Concurrency: the engine takes an exclusive cross-process lock on
//! `<snapshot>.lock` (the same file the fork's sweep/export/import scripts
//! `flock`) around the WHOLE load-modify-save cycle, so concurrent agents add
//! facts instead of replacing each other's. See `write_locked`.
//!
//! Env: LEMMALOG_MCP_PATH (snapshot path; required for mutations,
//! optional for fresh-session queries). LEMMALOG_LOCK_WAIT_MS overrides how
//! long a writer waits for the lock (default 20000).

#![cfg(feature = "mcp")]

use lemmalog::agent::{AgentMemory, MockExtractor};
use std::io::Read;

fn snap_path() -> String {
    std::env::var("LEMMALOG_MCP_PATH").unwrap_or_else(|_| {
        eprintln!("lemmalog-cli: set LEMMALOG_MCP_PATH to the shared snapshot path");
        std::process::exit(2);
    })
}

/// Load the snapshot, returning the memory and the version stamp it was read at.
///
/// FORK CHANGE: a load failure on an EXISTING file is fatal (exit 3) instead of
/// silently starting fresh — upstream's fallback let the next save overwrite a
/// corrupt-but-recoverable snapshot with an empty one. A missing file is a
/// legitimate first use and still starts fresh.
fn load(path: &str) -> (AgentMemory<MockExtractor>, Option<lemmalog::store::Stamp>) {
    if !std::path::Path::new(path).exists() {
        return (AgentMemory::new(MockExtractor::new(0.9), "").expect("fresh memory"), None);
    }
    match AgentMemory::load(MockExtractor::new(0.9), path) {
        Ok(m) => {
            let st = lemmalog::store::Stamp::of(path);
            (m, st)
        }
        Err(e) => {
            eprintln!(
                "lemmalog-cli: snapshot load failed ({e}); refusing to start fresh — \
                 that would overwrite {path}. Move the file aside to reset."
            );
            std::process::exit(3);
        }
    }
}

/// Save, but refuse when another process rewrote the file after we loaded it.
///
/// FORK CHANGE: upstream saves unconditionally (`fs::write`), so a concurrent
/// writer's work is silently replaced. We cannot merge two whole-file states, so
/// the honest behaviour is to fail loudly and let the caller re-run the command
/// against the current file. Only reachable by a writer that ignores the lock.
fn save_checked(
    m: &AgentMemory<MockExtractor>,
    path: &str,
    at_load: Option<lemmalog::store::Stamp>,
) -> Result<(), String> {
    if let Some(st) = at_load {
        if lemmalog::store::Stamp::of(path) != Some(st) {
            return Err(format!(
                "{path} changed while this command was running — refusing to \
                 overwrite another writer's state. Re-run the command."
            ));
        }
    }
    m.save(path)
        .map_err(|e| format!("save {path} failed: {e}"))
}

/// How many times a writer re-loads and re-applies its mutation before giving
/// up. With the lock held this never loops; it is the guard for a writer that
/// does not take the lock (an older binary, a hand-run script).
const WRITE_RETRIES: usize = 4;

/// Run `op` against the snapshot under the exclusive store lock.
///
/// FORK CHANGE (br-gx5.15): the load and the save happen INSIDE one lock, so
/// two agents observing different facts both survive. Before this, `load` ran
/// unlocked and `save_checked` compared stamps — a check-then-write race: two
/// processes both loaded at stamp S, both saw S unchanged, and both wrote, one
/// fact vanishing with exit 0 and no log line (P6 T3.3, 3/3).
///
/// `op` must be re-runnable: on a stamp change (a writer that skipped the lock)
/// it is called again against freshly loaded state, so anything it prints would
/// print twice — callers return their output instead of printing it.
fn write_locked<T>(
    path: &str,
    mut op: impl FnMut(&mut AgentMemory<MockExtractor>) -> T,
) -> T {
    let lock = match lemmalog::store::StoreLock::acquire(path) {
        Ok(l) => l,
        Err(e) => {
            // No lock, no write: this is the loud failure the old code lacked.
            let hint = if e.kind() == std::io::ErrorKind::WouldBlock {
                "hint: another agent, sweep or export is saving right now — retry in a moment"
            } else {
                "hint: the snapshot directory must exist and be writable by this user"
            };
            eprintln!("lemmalog-cli: cannot write {path}: {e}\n{hint}");
            std::process::exit(3);
        }
    };
    let mut attempt = 0;
    loop {
        attempt += 1;
        let (mut m, at_load) = load(path);
        sync_clock(&mut m);
        let out = op(&mut m);
        match save_checked(&m, path, at_load) {
            Ok(()) => {
                // Hold the lock until the save is on disk, then release.
                drop(lock);
                return out;
            }
            Err(e) if attempt < WRITE_RETRIES => {
                eprintln!(
                    "lemmalog-cli: {e} (retrying {attempt}/{WRITE_RETRIES} under the store lock)"
                );
                continue;
            }
            Err(e) => {
                eprintln!("lemmalog-cli: {e} (gave up after {attempt} attempts)");
                std::process::exit(3);
            }
        }
    }
}


fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned().filter(|v| !v.starts_with("--")))
}

fn stdin_or_flag(args: &[String], name: &str) -> String {
    if let Some(v) = flag(args, name) {
        return v;
    }
    let mut buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut buf);
    buf
}


/// Wall clock in seconds, matching the MCP server's default for `observe`.
fn wall_clock() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// FORK ADDITION: answer reads at "now", exactly as the server does.
///
/// The MCP server calls its `sync_clock` before every read; the CLI did not, so the
/// same snapshot could answer `current/3` differently depending on which surface
/// asked (a fact whose validity ended, or has not begun, is invisible to one and
/// visible to the other). Advancing the clock only invalidates and recomputes
/// derived relations — no fact is added, retracted or persisted by this.
fn sync_clock(m: &mut AgentMemory<MockExtractor>) {
    let t = wall_clock();
    if t > m.engine.now {
        m.engine.set_now(t);
        m.engine.invalidate_derived();
        let _ = m.engine.run();
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().cloned().unwrap_or_default();
    match cmd.as_str() {
        "observe" => {
            let path = snap_path();
            let text = stdin_or_flag(&args, "--facts");
            let ts = flag(&args, "--ts").and_then(|t| t.parse::<i64>().ok());
            let (report, dropped) = write_locked(&path, |m| {
                let ts = ts.unwrap_or(m.engine.now);
                let (report, dropped) = m.observe_extracted(&text, ts);
                let _ = m.maintain(m.engine.now);
                (report, dropped)
            });
            println!(
                "added={} updated={} noop={} escalations={}",
                report.added,
                report.updated,
                report.noop,
                report.escalations.len()
            );
            for d in dropped.iter().take(5) {
                println!("dropped: {} ({})", d.0, d.1);
            }
        }
        "retract" => {
            let path = snap_path();
            let text = stdin_or_flag(&args, "--facts");
            let (done, missing, died) = write_locked(&path, |m| m.retract_facts(&text));
            println!("retracted {} fact(s)", done.len());
            if !died.is_empty() {
                println!("{} derived fact(s) died:", died.len());
                for d in died.iter().take(15) {
                    println!("  {d}");
                }
            }
            for mi in &missing {
                println!("not found: {mi}");
            }
        }
        "query" => {
            let (mut m, _stamp) = load(&snap_path());
            sync_clock(&mut m);
            let goal = stdin_or_flag(&args, "--goal");
            // FORK CHANGE (chk-n5va.161): a fully-ground goal is a yes/no
            // question. `ask` answers it with a single row that has no
            // bindings — printed as a blank line, exactly like "no answers" —
            // so answer with the verdict instead. No separate exit code:
            // nothing in this CLI distinguishes "no answers" (an empty
            // variable result is exit 0), so `false` is exit 0 too and the
            // text carries the answer.
            match m.holds(&goal) {
                Ok(Some(true)) => println!("true"),
                Ok(Some(false)) => println!("false"),
                Ok(None) => match m.ask(&goal) {
                    Ok(rows) => {
                        if rows.is_empty() {
                            println!("(no answers — asserted facts are current(S, rel, O))");
                        } else {
                            println!("{}", rows.join("\n"));
                        }
                    }
                    Err(e) => {
                        eprintln!("parse: {e}\nhint: quote entity names — bare capitalized words are variables");
                        std::process::exit(1);
                    }
                },
                Err(e) => {
                    eprintln!(
                        "parse: {e}\nhint: quote entity names — bare capitalized words are variables"
                    );
                    std::process::exit(1);
                }
            }
        }
        "context" => {
            let (mut m, _stamp) = load(&snap_path());
            sync_clock(&mut m);
            let query = stdin_or_flag(&args, "--query");
            let budget = flag(&args, "--budget")
                .and_then(|b| b.parse::<usize>().ok())
                .unwrap_or(1000);
            print!("{}", m.context_for_query_rich(&query, budget));
        }
        "why" => {
            let (mut m, _stamp) = load(&snap_path());
            sync_clock(&mut m);
            let fact = stdin_or_flag(&args, "--fact");
            println!("{}", m.why(&fact));
        }
        "rules" => {
            let path = snap_path();
            let rules = stdin_or_flag(&args, "--rules");
            let res = write_locked(&path, |m| match m.install_rules(&rules) {
                Ok(id) => {
                    let n = m.maintain(m.engine.now);
                    Ok((id.clone(), n, m.batch_conflicts(&id)))
                }
                Err(e) => Err(format!("install: {e}")),
            });
            match res {
                Ok((id, n, conflicts)) => {
                    println!("installed {id}; backfill derived +{n} facts");
                    for w in conflicts {
                        println!("WARNING: {w}");
                    }
                }
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        "rmrules" => {
            let path = snap_path();
            let id = flag(&args, "--id").expect("--id <batch>");
            let removed = write_locked(&path, |m| {
                if m.uninstall_rules(&id) {
                    let _ = m.maintain(m.engine.now);
                    true
                } else {
                    false
                }
            });
            if removed {
                println!("uninstalled {id}; derivations reverted");
            } else {
                eprintln!("no batch {id:?} (see: lemmalog-cli batches)");
                std::process::exit(1);
            }
        }
        "batches" => {
            let (mut m, _stamp) = load(&snap_path());
            sync_clock(&mut m);
            for (id, src) in m.rule_batches() {
                // FORK CHANGE: print the WHOLE batch. Upstream printed only
                // `src.lines().next()`, so a multi-line batch could not be
                // round-tripped out of the store.
                println!("{id}:");
                for line in src.lines() {
                    println!("  {line}");
                }
            }
        }
        "dump" => {
            let (mut m, _stamp) = load(&snap_path());
            sync_clock(&mut m);
            let pred = flag(&args, "--pred");
            let mut preds: Vec<&String> = m.engine.relations.keys().collect();
            preds.sort();
            for p in preds {
                if let Some(want) = &pred {
                    if p != want {
                        continue;
                    }
                }
                for key in m.engine.relation_keys(p) {
                    println!("{}", m.engine.render_fact(p, &key));
                }
            }
        }
        other => {
            eprintln!(
                "usage: lemmalog-cli observe|retract|query|context|why|rules|rmrules|batches|dump [flags]\n\
                 flags: --facts|--goal|--query|--fact|--rules|--id|--pred|--ts|--budget  (or stdin)\n\
                 unknown command {other:?}"
            );
            std::process::exit(2);
        }
    }
}
