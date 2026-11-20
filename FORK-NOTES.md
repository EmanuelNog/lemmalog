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

## 5. Aggregates fold source rows, not projected `(group, value)` tuples

**Upstream:** `lower_agg_clause` lowers an aggregate head to a temp relation
`(group args..., inner terms...)` and the fold reads the temp relation. A
relation is a SET, so two body rows sharing the group and the value collapse:
five courses worth 4,4,3,3,2 answered `sum`=7, and `count` answered the number
of distinct values rather than the number of rows (24 credits totalling 76
answered `count`=2, `sum`=7).

**Ours:** the source row's columns — the args of the leftmost body atom binding
the aggregated variable — are appended to the temp key, so the fold sees one
row per source row. A value repeated by two rows counts twice (two 11-credit
courses are 22); the same row reached through a one-to-many join counts once
(one 3-credit course in two sections is 3). `count` is therefore COUNT(*) over
the source relation restricted by the rest of the body, not COUNT(DISTINCT
value). To fold over join solutions, aggregate over a relation keyed that way.

**Files:** `src/eval.rs` — `lower_agg_clause`, `collect_vars`, `atom_binds`.

**Test:** `cargo test --release --features mcp --test agg_test`.

## 6. A fully-ground goal answers `true` / `false`

**Upstream:** `Engine::ask` returns bindings; for a goal with no variables it
returns a single row rendered as `""`, which the CLI prints as a blank line and
the MCP tool as an empty string — identical to "no answers", so a yes/no
question could not be asked.

**Ours:** `Engine::holds` answers `Some(true|false)` for a ground atom and
`None` when the goal still binds something; `lemmalog-cli query` and the
`lemmalog_query` MCP tool print the verdict. No new exit code — this CLI has
none for "no answers" (an empty variable result is exit 0), so `false` is exit
0 too and the text is the verdict.

**Files:** `src/eval.rs` (`holds`), `src/agent.rs` (pass-through),
`src/bin/lemmalog-cli.rs` (query arm), `src/bin/lemmalog-mcp.rs` (query arm +
tool description).

**Test:** `cargo test --release --features mcp --test ground_query`,
`tests/interop_store.py`.

## 7. A backdated fact never closes a newer edge

**Upstream:** `apply_update` closes every open row for an exclusive relation at the new fact's
ts, including a row whose `valid_from` is *later*: `observe` v2.0.0 @2000 then v1.5.0 @1500
leaves `edge(2.0.0, VF=2000, VT=1500)` — an interval valid never — reports `updated=1`, and
`current/3` then answers 1.5.0 only. `retract` matches open rows, so the newer value is gone.

**Ours:** a row whose `VF` is greater than the fact's ts is never closed by it. The fact
becomes closed history `[ts, min(VF))`, closes only rows that started at or before it, and
is counted in `IngestReport::historical` (not yet printed by `lemmalog-cli`, which this
package does not own). Equal ts still supersedes; non-exclusive relations keep their ADD +
escalation path.

**Files:** `src/agent.rs` — `IngestReport::historical`, `apply_update`, `assert_historical`.

**Test:** `cargo test --release --features mcp --test agent_test backdated` and
`out_of_order_fact_never_closes_a_newer_edge`.

---

## Encoding a multi-valued fact that also needs revision

`exclusive("rel")` matches the relation name **exactly** — `apply_update` looks
the interned predicate up in the `exclusive` relation, not by prefix
(pinned by `tests/agent_test.rs::exclusive_matches_the_relation_name_exactly`).
The only prefix matching in the update policy is `MULTI`/`FUNCTIONAL` in
`src/agent.rs` (`evidence*`, `status*`, ...); `plan_*` is in neither list.

So a set-valued relation must not be declared exclusive: `exclusive("plan_spring_2027")`
on a plan holding three courses keeps only the last, because exclusivity means
one open object per (subject, relation). (chk-n5va.162 was filed as an engine
prefix-matching bug; the collapse came from the caller — the MemDerive harness
declared every `plan_` relation exclusive by prefix — so the bead is invalid.)

Put the revision in the relation name and let a view pick the latest one:

```prolog
% each revision is its own multi-valued relation; re-planning adds a v2
% (facts): milo --plan_spring_2027_v1--> crt_330 ; milo --plan_spring_2027_v2--> gls_220
plan_courses(S, C) :- current(S, "plan_spring_2027_v2", C).
% when a v3 arrives, retract the v2 view rule and add:
% plan_courses(S, C) :- current(S, "plan_spring_2027_v3", C).
```

Revisions are additive, and "latest" is a property of the view (one rule per
revision), not of the fact. Do not use exclusivity to mean "this set was
replaced" — a set needs the group key in its name.

---

## Known weaknesses we have NOT patched

- **No cross-process merge.** Two writers still converge to *last whole file*; we
  removed the silent clobber, the torn read and the failed-write-answered-as-success,
  not the lost update. A real fix needs a lock taken across read-modify-write in both
  binaries (a `libc`/`fs2` dependency = a Cargo change and a wider merge surface).
- **Extraction quality, not the store, is the ceiling.** The host model writes the
  facts; the engine only derives. Measured on MemDerive: goals whose vocabulary the
  store never received score 0 no matter how correct the store's mechanics are. Bash
  `observe` with a tight relation vocabulary beats a clever rule set over coarse facts.
## Running the fork's checks

```bash
cargo build --release --features mcp
cargo test  --release --features mcp          # full suite
cargo test  --release --features mcp --lib store::
/usr/bin/python3 tests/interop_store.py target/release/lemmalog-mcp target/release/lemmalog-cli
```
