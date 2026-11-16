# Our patches on top of upstream

`upstream` = `JordyZomer/lemmalog` (read-only). We run branch `custom-main`, which
`~/.hermes/scripts/lemmalog-fork-sync.sh` merges upstream into every day at 06:30.

**Why this file exists:** the daily sync audits it. When upstream implements one
of these itself, delete our version and keep theirs — do not keep two mechanisms.
Each entry names the files it touches, so a merge conflict is expected where it
is listed and nowhere else.

---

## 1. Atomic snapshot write

**Upstream:** `AgentMemory::save` ends in `std::fs::write(path, out)` — truncate in
place. A reader during a rewrite sees a prefix, and a crash mid-write leaves a
corrupt snapshot. The CLI's own doc comment already claims "atomic-rename"; the
code did not do it.

**Ours:** `src/store.rs::write_atomic` — same-directory temp file (pid-suffixed),
`write_all` + `sync_all`, then `rename` over the target; temp removed on any error.

**Files:** `src/store.rs` (new), `src/lib.rs` (+`pub mod store;`), `src/agent.rs`
(one line in `save`).

**Test:** `cargo test --lib store::` — includes `readers_never_see_a_partial_snapshot`
(2,000 rewrites against a concurrent reader; every read must parse).

## 2. A running server notices a foreign write

**Upstream:** the MCP server calls `AgentMemory::load` once at startup and never
again; no reload tool, no file watch. Two live servers (or a server plus the
CLI) therefore serve stale state and then overwrite the other's work on their
next mutation. This is what made `/reload-mcp` look necessary.

**Ours:** `State` carries the snapshot path and a `Stamp` (mtime-ns + length).
`reload_if_changed` runs at the top of every `tool_call`; when the file changed
under us it reloads before serving. Safe because every mutation is persisted
before its call returns, and derived facts are rebuilt by `load`.

**Files:** `src/bin/lemmalog-mcp.rs` — `State` (two fields), `reload_if_changed`,
the post-mutation persist block, the `lemmalog_save` arm.

**Test:** `tests/interop_store.py` — two servers, interleaved writes; A must see
B's fact with no reload command, and all three facts must survive.

## 3. The CLI refuses to destroy state

**Upstream:** `load()` starts a **fresh** memory when the snapshot fails to load,
and the next `save()` writes that fresh (empty) memory over the corrupt file — a
recoverable snapshot becomes an empty one. It also saves unconditionally, so a
concurrent writer's work is silently replaced.

**Ours:** a load failure on an existing file exits 3, leaving the file alone (a
*missing* file is still a legitimate first use). Every mutating command captures
the stamp at load and `save_checked` refuses to write if the file changed
meanwhile, telling the caller to re-run.

**Files:** `src/bin/lemmalog-cli.rs` — `load`, `save_checked`, 9 call sites.

**Test:** `tests/interop_store.py` — corrupt file gives exit 3 and is untouched;
missing file still creates.

## 4. A failed persist is reported, not swallowed

**Upstream:** the post-mutation persist is `let _ = state.memory.save(p);` — an
`observe` that fails to reach disk still answers with a success-shaped result.

**Ours:** the error is logged to stderr and the stamp is only refreshed on
success. The tool result is unchanged (the mutation did happen in memory), but the
failure is no longer invisible.

**Files:** `src/bin/lemmalog-mcp.rs` — post-mutation persist block.

---

## Known weaknesses we have NOT patched

- **Unknown tool arguments are silently ignored.** `args["goal"]` etc. are read
  individually, so a caller passing `query=` instead of `goal=` gets an empty goal
  and a parse error far from the cause. Requires schema-aware validation.
- **No cross-process merge.** Two writers still converge to *last whole file*; we
  removed the silent clobber and the torn read, not the lost update. A real fix
  needs a lock file taken across read-modify-write in both binaries (upstream has
  no `libc`/`fs2` dependency, so it is a Cargo change and a bigger merge surface).
- **`batches` (CLI) prints only the first line** of a multi-line rule batch; the
  MCP tool prints the full source. Rules cannot be exported from the CLI alone.

## Running the fork's checks

```bash
cargo build --release --features mcp
cargo test  --release --features mcp          # full suite
cargo test  --release --features mcp --lib store::
/usr/bin/python3 tests/interop_store.py target/release/lemmalog-mcp target/release/lemmalog-cli
```
