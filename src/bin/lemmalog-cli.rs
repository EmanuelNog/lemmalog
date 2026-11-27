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

/// Positional words, skipping flags and the values they take.
///
/// `context "open a ticket"` looked like it worked: the positional was IGNORED, so the query was
/// empty (or stdin was read instead), and the tool answered a different question than the one
/// asked — silently, with exit 0. Measured 2026-11-22: every `lemmalog-cli context <text>` run
/// returned the empty-query answer, and with stdin on a pipe it blocked forever reading it.
fn positional_text(args: &[String], value_flags: &[&str]) -> String {
    let mut words: Vec<String> = Vec::new();
    let mut skip_next = false;
    for (i, a) in args.iter().enumerate() {
        if i == 0 {
            continue; // the subcommand
        }
        if skip_next {
            skip_next = false;
            continue;
        }
        if a.starts_with("--") {
            if value_flags.contains(&a.as_str()) {
                skip_next = true;
            }
            continue;
        }
        words.push(a.clone());
    }
    words.join(" ")
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
            // A bad --ts used to vanish: `.and_then(parse.ok())` turned a typo into None, so
            // `--ts abc` silently stored at "now" and reported success (measured 2026-11-22).
            let ts = match flag(&args, "--ts") {
                None => None,
                Some(raw) => match raw.trim().parse::<i64>() {
                    Ok(v) => Some(v),
                    Err(_) => {
                        eprintln!("usage: --ts takes whole epoch seconds (got {raw:?})");
                        std::process::exit(2);
                    }
                },
            };
            if let Some(v) = ts {
                // Mirror of the sweep's future_ts_problem (br-gx5.2), which exists because
                // `--ts` SETS the store clock: a future-dated fact is not one wrong row, it
                // holds that (subject, relation) `current` until real time catches up and the
                // engine refuses every true fact for the slot in the meantime.
                if let Ok(d) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
                    let now = d.as_secs() as i64;
                    if v > now + 300 {
                        eprintln!(
                            "refused: --ts {v} is {}s in the future (tolerance 300s) — a \
future-dated fact freezes its slot until real time catches up",
                            v - now
                        );
                        std::process::exit(2);
                    }
                }
            }
            let (report, dropped) = write_locked(&path, |m| {
                let ts = ts.unwrap_or(m.engine.now);
                let (report, dropped) = m.observe_extracted(&text, ts);
                let _ = m.maintain(m.engine.now);
                (report, dropped)
            });
            println!(
                // The sweep parses this line with `added=.. updated=.. noop=.. escalations=..`:
                // new fields go at the END, or every count silently reads zero (br-gx5.85).
                "added={} updated={} noop={} escalations={} suppressed={}",
                report.added,
                report.updated,
                report.noop,
                report.escalations.len(),
                report.suppressed
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
        "close" => {
            let path = snap_path();
            let text = stdin_or_flag(&args, "--facts");
            let (done, missing, died) = write_locked(&path, |m| m.close_facts(&text));
            println!("closed {} fact(s) (kept as history, tombstoned)", done.len());
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
        "reopen" => {
            let path = snap_path();
            let text = stdin_or_flag(&args, "--facts");
            let (done, missing) = write_locked(&path, |m| m.reopen_facts(&text));
            println!("reopened {} fact(s)", done.len());
            for mi in &missing {
                println!("no tombstone: {mi}");
            }
        }
        "query" => {
            // Checked BEFORE the store is opened: a usage slip must not touch the snapshot (on a
            // live host that is the real store) and must not report an internal parse error.
            let goal = stdin_or_flag(&args, "--goal");
            if goal.trim().is_empty() {
                eprintln!("usage: lemmalog-cli query --goal '<goal>'   (e.g. current(S, R, O))");
                std::process::exit(2);
            }
            let (mut m, _stamp) = load(&snap_path());
            sync_clock(&mut m);
            // FORK CHANGE (chk-n5va.161): a fully-ground goal is a yes/no
            // question. `ask` answers it with a single row that has no
            // bindings — printed as a blank line, exactly like "no answers" —
            // so answer with the verdict instead. No separate exit code:
            // nothing in this CLI distinguishes "no answers" (an empty
            // variable result is exit 0), so `false` is exit 0 too and the
            // text carries the answer.
            match m.holds(&goal) {
                Ok(Some(true)) => println!("true"),
                Ok(Some(false)) => {
                    println!("false");
                    // FORK ADDITION: `false` alone cannot distinguish "never
                    // asserted" from "closed"; name the history on stderr so
                    // stdout stays a parseable verdict.
                    if let Some(h) = m.history_hint(&goal) {
                        eprintln!("{h}");
                    }
                }
                Ok(None) => match m.ask(&goal) {
                    Ok(rows) => {
                        if rows.is_empty() {
                            println!("(no answers — asserted facts are current(S, rel, O))");
                            if let Some(h) = m.history_hint(&goal) {
                                eprintln!("{h}");
                            }
                        } else {
                            println!("{}", rows.join("\n"));
                        }
                        // A capitalized name in a goal is a VARIABLE in Datalog, but the store
                        // also allows capitalized entities (`Svc`), so `current(Svc, port, P)`
                        // silently returns every subject instead of that entity. The parse-error
                        // path already carries the hint; a SUCCESSFUL query needs it too.
                        // stderr only: output and exit code are unchanged.
                        for name in lemmalog::agent::variable_like_names(&goal) {
                            if m.engine.interner.lookup(&name).is_some() {
                                eprintln!(
                                    "hint: '{name}' was read as a variable; it is also an \
entity. Quote it (\"{name}\") to match that entity."
                                );
                            }
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
            // Order: --query, then the positional, then stdin (the pipe convention). Reading
            // stdin when a positional was given is what made `context <text>` HANG on a pipe
            // while looking like a normal call (measured 2026-11-22).
            let mut query = flag(&args, "--query").unwrap_or_default();
            if query.trim().is_empty() {
                query = positional_text(&args, &["--query", "--budget"]);
            }
            if query.trim().is_empty() {
                query = stdin_or_flag(&args, "--query");
            }
            // Checked before the store is opened, like `query` and `why`: a usage slip must not
            // read the live snapshot and must not answer a question nobody asked.
            if query.trim().is_empty() {
                eprintln!(
                    "usage: lemmalog-cli context --query '<question>' [--budget N]\n                            (a positional works too: lemmalog-cli context 'where does alice work')"
                );
                std::process::exit(2);
            }
            let (mut m, _stamp) = load(&snap_path());
            sync_clock(&mut m);
            let budget = flag(&args, "--budget")
                .and_then(|b| b.parse::<usize>().ok())
                .unwrap_or(1000);
            print!("{}", m.context_for_query_rich(&query, budget));
        }
        "why" => {
            // `why current(x, r, o)` positionally used to reach m.why("") and report
            // `cannot parse ""` — a usage slip dressed as a parse error (measured 2026-11-22).
            let fact = stdin_or_flag(&args, "--fact");
            if fact.trim().is_empty() {
                eprintln!("usage: lemmalog-cli why --fact 'current(s, r, o)'");
                std::process::exit(2);
            }
            let (mut m, _stamp) = load(&snap_path());
            sync_clock(&mut m);
            println!("{}", m.why(&fact));
        }
        "rules" => {
            let path = snap_path();
            let rules = stdin_or_flag(&args, "--rules");
            // An empty rule set installed a batch that derives nothing (`installed b1; backfill
            // derived +0 facts`) — noise in `batches` with no way to tell it from a real one.
            if rules.trim().is_empty() {
                eprintln!("usage: lemmalog-cli rules --rules '<datalog text>'   (nothing to install)");
                std::process::exit(2);
            }
            let res = write_locked(&path, |m| {
                // Re-installing the SAME text used to create a second batch and warn that the
                // definitions UNION (measured 2026-11-22: `rules --rules X` twice gave b1 and
                // b2). Identical text derives identical facts, so a retry must be a no-op
                // instead of accumulating duplicate batches.
                if let Some(id) = m.batch_with_text(&rules) {
                    return Ok((id, 0, Vec::new(), true));
                }
                match m.install_rules(&rules) {
                Ok(id) => {
                    let n = m.maintain(m.engine.now);
                    Ok((id.clone(), n, m.batch_conflicts(&id), false))
                }
                Err(e) => Err(format!("install: {e}")),
                }
            });
            match res {
                Ok((id, n, conflicts, noop)) => {
                    if noop {
                        println!("already installed {id}; nothing changed");
                    } else {
                        println!("installed {id}; backfill derived +{n} facts");
                    }
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
            // A missing flag is a usage mistake, not a crash: `.expect()` here panicked with
            // `thread 'main' panicked at ...: --id <batch>`, which reads like engine corruption.
            let id = match flag(&args, "--id") {
                Some(v) if !v.trim().is_empty() => v,
                _ => {
                    eprintln!("usage: lemmalog-cli rmrules --id <batch>   (list ids with `batches`)");
                    std::process::exit(2);
                }
            };
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

