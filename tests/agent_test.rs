use lemmalog::{AgentMemory, MockExtractor, Value};

fn mem(extra: &str) -> AgentMemory<MockExtractor> {
    AgentMemory::new(MockExtractor::new(0.9), extra).unwrap()
}

#[test]
fn ingest_adds_facts_with_provenance() {
    let mut m = mem("");
    let r = m.observe("alice --works_at--> acme\nalice --manager--> bob");
    assert_eq!(r.added, 2);
    assert_eq!(m.maintain(100), 2); // 2 current facts
    let rows = m.ask("current(\"alice\", R, O)").unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|r| r.contains("acme")));
    // provenance points at the episode
    let out = m.why("current(alice, works_at, acme)");
    assert!(out.contains("ep1"), "why: {out}");
}

#[test]
fn reobservation_is_noop() {
    let mut m = mem("");
    m.observe("alice --works_at--> acme");
    let r = m.observe("alice --works_at--> acme");
    assert_eq!(r.noop, 1);
    assert_eq!(r.added, 0);
    m.maintain(100);
    assert_eq!(
        m.ask("current(\"alice\", \"works_at\", O)").unwrap().len(),
        1
    );
}

#[test]
fn exclusive_pred_supersedes_deterministically() {
    let mut m = mem("");
    m.observe("alice --works_at--> acme");
    m.maintain(100);
    assert_eq!(
        m.ask("current(\"alice\", \"works_at\", O)").unwrap(),
        vec!["O=acme".to_string()]
    );
    let r = m.observe_at("alice --works_at--> gigant", 200);
    assert_eq!(r.updated, 1, "deterministic UPDATE, no escalation");
    assert!(r.escalations.is_empty());
    m.maintain(200);
    assert_eq!(
        m.ask("current(\"alice\", \"works_at\", O)").unwrap(),
        vec!["O=gigant".to_string()],
        "knowledge update applied"
    );
    // the old edge is closed, not deleted: history preserved
    let closed = m.engine.query(
        "edge",
        &[
            None,
            None,
            None,
            None,
            Some(lemmalog::Value::Int(200)),
            None,
        ],
    );
    assert_eq!(closed.len(), 1, "exactly one edge closed at t=200");
}

#[test]
fn non_exclusive_conflict_escalates() {
    let mut m = mem("");
    m.observe("alice --likes--> bob");
    let r = m.observe("alice --likes--> carol");
    assert_eq!(r.added, 1);
    assert_eq!(r.escalations.len(), 1);
    assert!(r.escalations[0].contains("conflict"));
    assert_eq!(m.escalations().len(), 1);
    m.maintain(100);
    // both remain open pending agent resolution
    assert_eq!(m.ask("current(\"alice\", \"likes\", O)").unwrap().len(), 2);
    m.resolve_escalation(0);
    assert_eq!(m.escalations().len(), 0);
}

#[test]
fn context_assembly_is_positional_and_budgeted() {
    let mut m = mem("");
    m.observe("alice --works_at--> acme\nalice --manager--> bob");
    m.maintain(100);
    let ctx = m.context(&["alice"], 200);
    let dist = ctx.find("== memory").unwrap();
    let src = ctx.find("== source").unwrap();
    assert!(dist < src, "distilled facts before verbatim sources");
    assert!(ctx.contains("alice --works_at--> acme"));
    assert!(ctx.contains("[ep1]"), "verbatim episode text included");

    // tiny budget truncates but keeps both sections
    let tiny = m.context(&["alice"], 15);
    assert!(tiny.contains("== memory") && tiny.contains("== source"));
    assert!(tiny.len() < ctx.len());
}

#[test]
fn derivation_rules_compose_with_ingestion() {
    let mut m = mem("reports_to(X,Y) :- current(X,\"manager\",Y).\n\
         trans: reports_to(X,Z) :- reports_to(X,Y), reports_to(Y,Z).");
    m.observe("alice --manager--> bob\nbob --manager--> carol");
    let derived = m.maintain(100);
    assert_eq!(derived, 5, "2 current + 2 direct + 1 transitive");
    let rows = m.ask("reports_to(\"alice\", Y)").unwrap();
    assert_eq!(rows.len(), 2); // bob, carol
    let proof = m.why("reports_to(alice, carol)");
    assert!(proof.contains("via trans") && proof.contains("ep1"));
}

#[test]
fn context_reports_new_memory() {
    let mut m = mem("");
    m.observe("alice --works_at--> acme");
    m.maintain(100);
    let ctx = m.context(&["alice"], 200);
    assert!(ctx.contains("new in memory"), "{ctx}");
    assert!(
        ctx.contains("current(alice, works_at, acme)"),
        "derived fact reported: {ctx}"
    );
    // a turn with nothing new has no news section
    m.maintain(200);
    let ctx2 = m.context(&["alice"], 200);
    assert!(!ctx2.contains("new in memory"), "{ctx2}");
    // and a new observation shows up in the next context
    m.observe("alice --manager--> bob");
    m.maintain(300);
    let ctx3 = m.context(&["alice"], 200);
    assert!(ctx3.contains("edge(alice, manager, bob"), "{ctx3}");
}

#[test]
fn llm_extractor_pluggable_and_memoized() {
    use lemmalog::LlmExtractor;
    let mut _calls = 0;
    let mut m = AgentMemory::new(
        LlmExtractor::new(move |prompt| {
            _calls += 1;
            assert!(prompt.contains("Extract the factual triples"));
            Ok("alice --works_at[0.7]--> acme\nbob --manager--> alice".to_string())
        }),
        "",
    )
    .unwrap();
    let r = m.observe_at("any episode text", 100);
    assert_eq!(r.added, 2);
    m.maintain(100);
    // per-fact confidence honored by the protocol
    let emp = m.ask("current(\"alice\", \"works_at\", O)").unwrap();
    assert_eq!(emp, vec!["O=acme".to_string()]);
    let (a, wa, ac) = (
        m.engine.sym("alice"),
        m.engine.sym("works_at"),
        m.engine.sym("acme"),
    );
    // asserted_at is wall time now (the transaction clock), so look the
    // row up by its stable positions and read the annotation off it
    let rows = m.engine.query(
        "edge",
        &[Some(a), Some(wa), Some(ac), Some(Value::Int(100)), Some(Value::Int(i64::MAX)), None],
    );
    assert_eq!(rows.len(), 1);
    assert!((rows[0].1.conf - 0.7).abs() < 1e-9, "conf = {}", rows[0].1.conf);
    // extraction errors degrade to zero facts, not poison
    // (checked via a second memory below)
    let mut m2 =
        AgentMemory::new(LlmExtractor::new(|_| Err("provider down".to_string())), "").unwrap();
    let r2 = m2.observe("whatever");
    assert_eq!(r2.added, 0);
}

#[test]
fn snapshot_roundtrip_rebuilds_derived_relations() {
    let dir = std::env::temp_dir().join("lemmalog-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("mem.snapshot");
    let path = path.to_str().unwrap();

    let mut m = mem("reports_to(X,Y) :- current(X,\"manager\",Y).\n\
                    trans: reports_to(X,Z) :- reports_to(X,Y), reports_to(Y,Z).");
    m.observe_at("alice --manager--> bob\nbob --manager--> carol", 100);
    m.observe_at("alice --works_at--> acme", 100);
    m.maintain(100);
    m.observe_at("alice --works_at--> gigant", 200); // supersession
    m.observe_at("alice --likes--> khq\nalice --likes--> zph", 300); // escalation
    let derived = m.maintain(300);
    assert!(derived > 0);
    let before_ctx = m.context(&["alice"], 300);

    m.save(path).unwrap();
    let mut m2 = AgentMemory::load(MockExtractor::new(0.9), path).unwrap();

    // derived relations rebuilt; answers identical
    assert_eq!(
        m2.ask("reports_to(\"alice\", Y)").unwrap(),
        vec!["Y=bob".to_string(), "Y=carol".to_string()]
    );
    assert_eq!(
        m2.ask("current(\"alice\", \"works_at\", O)").unwrap(),
        vec!["O=gigant".to_string()]
    );
    // episodes + escalations survive
    assert_eq!(m2.episodes().len(), 4);
    assert_eq!(m2.escalations().len(), 1);
    // context assembly identical apart from the news section: a freshly
    // loaded memory has nothing "new since load", by design
    let strip_news = |s: &str| -> String {
        match s.find("== memory (distilled") {
            Some(i) => s[i..].to_string(),
            None => s.to_string(),
        }
    };
    assert_eq!(
        strip_news(&m2.context(&["alice"], 300)),
        strip_news(&before_ctx)
    );
    // why() still walks to the re-asserted base facts
    let w = m2.why("current(alice, works_at, gigant)");
    assert!(w.contains("asserted (base fact)"), "{w}");
    // and the loaded memory keeps working incrementally
    m2.observe_at("bob --works_at--> initech", 400);
    m2.maintain(400);
    assert_eq!(
        m2.ask("current(\"bob\", \"works_at\", O)").unwrap(),
        vec!["O=initech".to_string()]
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn what_if_lookahead_leaves_store_unchanged() {
    let mut m = mem("reports_to(X,Y) :- current(X,\"manager\",Y).\n\
                    trans: reports_to(X,Z) :- reports_to(X,Y), reports_to(Y,Z).");
    m.observe_at("a --manager--> b", 100);
    m.maintain(100);
    assert_eq!(
        m.ask("reports_to(\"a\", Y)").unwrap(),
        vec!["Y=b".to_string()]
    );

    // what would follow if c managed a?
    let (rows, added) = m
        .what_if("c --manager--> a", "reports_to(\"c\", Y)")
        .unwrap();
    let mut got: Vec<&str> = rows.iter().map(|r| r.strip_prefix("Y=").unwrap()).collect();
    got.sort();
    assert_eq!(got, vec!["a", "b"], "hypothetical closure c->a->b");
    assert!(added >= 2, "assumption would add facts: {added}");

    // store untouched: no c facts, no news pollution
    assert!(m.ask("reports_to(\"c\", Y)").unwrap().is_empty());
    assert!(m.engine.changes_from(m.engine.epoch()).is_empty());
    let ctx = m.context(&["a"], 200);
    assert!(!ctx.contains("reports_to(c"), "{ctx}");
    // and the same query repeated is stable
    let (rows2, _) = m
        .what_if("c --manager--> a", "reports_to(\"c\", Y)")
        .unwrap();
    assert_eq!(rows2.len(), 2);

    // committing the real episode makes it true
    m.observe_at("c --manager--> a", 200);
    m.maintain(200);
    assert_eq!(m.ask("reports_to(\"c\", Y)").unwrap().len(), 2);
}

#[test]
fn parse_protocol_reported_gives_drop_reasons() {
    use lemmalog::agent::parse_protocol_reported;
    let (facts, dropped) = parse_protocol_reported(
        "alice --works_at[0.8]--> acme\n\
         speaker --works_at--> acme\n\
         - acme --has_products--> products? maybe not because generic\n\
         this line has no arrow structure",
        0.9,
    );
    assert_eq!(facts.len(), 1);
    assert_eq!(dropped.len(), 3);
    let reasons: Vec<&str> = dropped.iter().map(|(_, r)| r.as_str()).collect();
    assert!(
        reasons.iter().any(|r| r.contains("role word")),
        "{reasons:?}"
    );
    assert!(
        reasons
            .iter()
            .any(|r| r.contains("prose") || r.contains("punctuation")),
        "{reasons:?}"
    );
    assert!(
        reasons.iter().any(|r| r.contains("no `--rel-->`")),
        "{reasons:?}"
    );
}

#[test]
fn observe_extracted_applies_policy_and_reports_drops() {
    let mut m = mem("");
    let (report, dropped) =
        m.observe_extracted("alice --works_at--> acme\nspeaker --works_at--> acme", 100);
    assert_eq!(report.added, 1, "only the clean fact asserted");
    assert_eq!(dropped.len(), 1);
    assert!(dropped[0].1.contains("role word"));
    m.maintain(100);
    assert_eq!(
        m.ask("current(\"alice\", \"works_at\", O)").unwrap(),
        vec!["O=acme".to_string()]
    );
}

/// Regression: moving the clock does not refresh temporal views on its own.
///
/// Seminaive evaluation fires a rule only for facts in the epoch delta, so a
/// `now(T)` view like `current/3` keeps answering as of the clock that was
/// set when each edge arrived. Production hit this from the other side: a
/// backdated batch rewound the clock, and every fact whose VF was later
/// silently stopped projecting — still in `edge`, absent from `current`, and
/// no amount of re-running maintenance brought it back. `invalidate_derived`
/// is the documented escape hatch, and the MCP read paths call it via
/// `sync_clock`.
#[test]
fn seminaive_does_not_refresh_now_rules_without_invalidation() {
    let mut m = mem("");
    // valid from t=2000, but derived while the clock sits at 1000
    m.observe_extracted("late --relates_to--> ticket", 2000);
    m.maintain(1000);
    assert!(
        m.ask("current(\"late\", R, O)").unwrap().is_empty(),
        "VF=2000 must not project at clock 1000"
    );
    // the edge is resident either way
    assert_eq!(m.ask("edge(\"late\", R, O, VF, VT, TS)").unwrap().len(), 1);

    // Pins current design, not a requirement: advancing the clock alone
    // leaves the view stale because there is no delta to fire on. If
    // `set_now` ever learns to invalidate when clock-dependent rules exist,
    // delete this assertion — the two around it are the regression guard.
    m.maintain(3000);
    assert!(
        m.ask("current(\"late\", R, O)").unwrap().is_empty(),
        "clock advance alone does not refresh — this is why invalidate_derived exists"
    );

    // invalidation re-seeds the delta and the view catches up
    m.engine.invalidate_derived();
    m.maintain(3000);
    assert_eq!(
        m.ask("current(\"late\", R, O)").unwrap().len(),
        1,
        "after invalidation the fact projects at clock 3000"
    );
}
#[test]
fn source_references_are_valid_entities() {
    use lemmalog::agent::parse_protocol_reported;
    let (facts, dropped) = parse_protocol_reported(
        "sketch_mode --located--> src/features/sketchMode/bind.ts\n\
         entity_token_problem --defined_at--> src/agent.rs:92\n\
         parser --tracked_by--> JordyZomer/lemmalog#12\n\
         sketch_mode --depends_on--> engine_scene",
        1.0,
    );
    assert_eq!(facts.len(), 4, "dropped: {dropped:?}");
    assert!(dropped.is_empty(), "{dropped:?}");
    assert_eq!(facts[1].obj, "src/agent.rs:92");
}

/// The SAME list is asserted by the Python export filter
/// (`test_secret_shapes_are_refused_and_values_kept` in ~/.hermes/scripts/tests/): one rule, two
/// implementations, pinned by one list so they cannot drift apart unnoticed.
const SECRET_CASES: [&str; 9] = [
    "sk-A1b2C3d4E5f6G7h8I9j0",
    "ghp_aB1cD2eF3gH4iJ5kL6mN7oP8qR9sT0uV1wX2",
    "xoxb-123456789012-abcdefghij",
    "Zx9Qw8Er7Ty6Ui5Op4As3Df2Gh1Jk0Lm",
    "https://user:pw@example.test/x",
    "postgres://app:secret@db.internal:5432/app",
    "token=Xk9mP2qR7sT4vW8y",
    "password=hunter2",
    "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0",
];
const VALUE_CASES: [&str; 6] = [
    "9f2c1b7a4e6d8035af1c9b2e7d4f6018ab3c5e7d",       // hex sha: a fact shape, never a secret
    "https://example.test/x",                          // url without credentials
    "ssh://git@github.com/org/repo.git",               // user, no password
    "running",
    "~/.hermes/skills/software-development/lemmalog",
    "2026-11-22T12:00:00Z",
];

#[test]
fn secret_shape_parity_cases() {
    for s in SECRET_CASES {
        assert!(lemmalog::agent::secret_shaped(s), "must be refused: {s}");
    }
    for s in VALUE_CASES {
        assert!(!lemmalog::agent::secret_shaped(s), "must be kept: {s}");
    }
}

#[test]
fn observe_refuses_a_credential_and_never_echoes_it() {
    let (facts, dropped) = lemmalog::agent::parse_protocol_reported(
        "svc --key--> ghp_aB1cD2eF3gH4iJ5kL6mN7oP8qR9sT0uV1wX2\nsvc --status--> up\n",
        0.9,
    );
    assert_eq!(facts.len(), 1, "only the ordinary fact is stored: {facts:?}");
    assert_eq!(facts[0].obj, "up");
    assert_eq!(dropped.len(), 1);
    let (line, reason) = &dropped[0];
    assert!(reason.contains("credential"), "{reason}");
    assert!(
        !line.contains("ghp_"),
        "the drop report must not carry the credential: {line}"
    );
    assert!(line.contains("<redacted>"), "{line}");
}

#[test]
fn why_distinguishes_a_variable_from_an_unknown_entity() {
    let mut m = mem("");
    m.observe("a --status--> one");
    // `current/3` is DERIVED: without a maintenance pass the fact reads as "[unknown fact]",
    // which is correct engine behaviour and what the CLI does on load.
    m.maintain(100);
    // an entity the store has never seen is not a "variable"
    let absent = m.why("current(a, status, absentvalue)");
    assert!(absent.contains("not in the store"), "{absent}");
    assert!(
        !absent.contains("contains variables"),
        "a ground goal with an unknown entity is not a variable problem: {absent}"
    );
    // a real variable still says so
    let var = m.why("current(A, status, one)");
    assert!(var.contains("contains variables"), "{var}");
    // and a real fact still explains itself
    let real = m.why("current(a, status, one)");
    assert!(real.contains("via rule/current"), "{real}");
}

#[test]
fn two_facts_on_one_line_are_refused_not_merged() {
    use lemmalog::agent::parse_protocol_reported;
    // Measured 2026-11-22: this stored ONE fact whose object was `four e --status--> five`.
    let (facts, dropped) = parse_protocol_reported("d --status--> four e --status--> five", 1.0);
    assert!(facts.is_empty(), "must not merge: {facts:?}");
    assert_eq!(dropped.len(), 1);
    assert!(
        dropped[0].1.contains("one fact per line"),
        "{:?}",
        dropped[0].1
    );
    // `--` inside a value is fine: only a second `-->` makes the line ambiguous
    let (ok, _) = parse_protocol_reported("cli --note--> use --force and --verbose", 1.0);
    assert_eq!(ok.len(), 1);
    assert_eq!(ok[0].obj, "use --force and --verbose");
}

#[test]
fn punctuation_with_spaces_is_still_prose() {
    use lemmalog::agent::parse_protocol_reported;
    let (facts, dropped) = parse_protocol_reported(
        "thing --located--> see src/agent.rs, around line 118\n\
         thing --noted--> Yes. Confirmed.",
        1.0,
    );
    assert!(facts.is_empty(), "{facts:?}");
    assert_eq!(dropped.len(), 2);
    assert!(
        dropped.iter().all(|(_, r)| r.contains("prose")),
        "{dropped:?}"
    );
}

#[test]
fn snapshot_preserves_rules_installed_after_construction() {
    let dir = std::env::temp_dir().join("lemmalog-test-batches");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("mem.snapshot");
    let path = path.to_str().unwrap();

    // Nothing in the constructor: this is how the MCP server runs.
    let mut m = mem("");
    m.observe_at("alice --manager--> bob\nbob --manager--> carol", 100);
    let b = m
        .install_rules(
            "reports_to(X, Y) :- current(X, \"manager\", Y).\n\
             reports_to(X, Z) :- reports_to(X, Y), reports_to(Y, Z).",
        )
        .unwrap();
    m.maintain(100);
    assert_eq!(m.ask("reports_to(\"alice\", Y)").unwrap().len(), 2);

    m.save(path).unwrap();
    let mut m2 = AgentMemory::load(MockExtractor::new(0.9), path).unwrap();

    assert_eq!(
        m2.ask("reports_to(\"alice\", Y)").unwrap(),
        vec!["Y=bob".to_string(), "Y=carol".to_string()],
        "an installed rule batch must survive a reload"
    );
    // batch identity survives, so uninstall still targets the same batch
    let ids: Vec<String> = m2.rule_batches().into_iter().map(|(i, _)| i).collect();
    assert!(ids.contains(&b), "batch {b} missing from {ids:?}");
    assert!(m2.uninstall_rules(&b));
    m2.maintain(200);
    assert!(m2.ask("reports_to(\"alice\", Y)").unwrap().is_empty());
}

#[test]
fn batch_ids_never_collide_across_uninstall() {
    let mut m = AgentMemory::<MockExtractor>::new(MockExtractor::new(0.9), "").unwrap();
    // agent installs two batches, uninstalls the first, installs another:
    // with ids derived from rule_batches.len() the new batch would reuse
    // b2 while the original b2 is still live, and uninstall("b2") would
    // then target whichever batch position() finds first
    let b1 = m
        .install_rules("r1(X, Y) :- current(X, \"knows\", Y).")
        .unwrap();
    let b2 = m
        .install_rules("r2(X, Y) :- current(X, \"likes\", Y).")
        .unwrap();
    assert!(m.uninstall_rules(&b1));
    let b3 = m
        .install_rules("r3(X, Y) :- current(X, \"owns\", Y).")
        .unwrap();
    assert_ne!(b2, b3, "new batch id must not collide with a live batch");
    // uninstall by id after the churn removes exactly the batch asked for
    assert!(m.uninstall_rules(&b2));
    let remaining: Vec<String> = m.rule_batches().into_iter().map(|(id, _)| id).collect();
    assert!(!remaining.contains(&b2), "{remaining:?}");
    assert!(remaining.contains(&b3), "{remaining:?}");
}

#[test]
fn bare_numbers_through_the_protocol_become_integers() {
    let mut m = AgentMemory::<MockExtractor>::new(MockExtractor::new(0.9), "").unwrap();
    m.observe_extracted(
        "launch --monthly_cost--> 120\nlaunch --name--> Apollo\nlaunch --monthly_cost--> 60\n",
        100,
    );
    m.maintain(100);
    // sum aggregation and comparison only work over integers — symbol
    // "120" would derive nothing
    m.install_rules("total_cost(S, sum(N)) :- current(S, \"monthly_cost\", N).\nbig_spender(S) :- total_cost(S, C), C >= 150.")
        .unwrap();
    m.maintain(100);
    assert_eq!(
        m.ask("total_cost(\"launch\", T)").unwrap(),
        vec!["T=180".to_string()],
        "digit objects aggregate as integers"
    );
    assert_eq!(m.ask("big_spender(S)").unwrap().len(), 1);
}

#[test]
fn retract_propagates_through_derived_closures() {
    let mut m = AgentMemory::<MockExtractor>::new(MockExtractor::new(0.9), "").unwrap();
    m.observe_extracted(
        "alice --manager--> bob\nbob --manager--> carol\ncarol --manager--> dana",
        100,
    );
    m.install_rules(
        "reports_to(X,Y) :- current(X,\"manager\",Y).\ntrans: reports_to(X,Z) :- reports_to(X,Y), reports_to(Y,Z).",
    )
    .unwrap();
    m.maintain(100);
    // alice reaches bob, carol, dana through the closure
    assert_eq!(m.ask("reports_to(\"alice\", X)").unwrap().len(), 3);
    // the middle edge was wrong — retract it and the closure must repair
    let (done, missing, died) = m.retract_facts("bob --manager--> carol");
    assert!(missing.is_empty(), "{missing:?}");
    assert_eq!(done.len(), 1);
    assert!(
        died.iter().any(|l| l.contains("reports_to")),
        "consequence report lists dead derivations: {died:?}"
    );
    let remaining = m.ask("reports_to(\"alice\", X)").unwrap();
    assert_eq!(remaining.len(), 1, "closure repaired: {remaining:?}");
    // a second retract of the same fact is a loud not-found, not a silent no-op
    let (_, missing2, _) = m.retract_facts("bob --manager--> carol");
    assert_eq!(missing2.len(), 1);
}

#[test]
fn rich_context_carries_attribution_and_latest_values() {
    let mut m = AgentMemory::<MockExtractor>::new(MockExtractor::new(0.9), "").unwrap();
    m.observe_extracted(
        "caroline --received_necklace_from--> grandma\nmelanie --paints--> landscapes",
        100,
    );
    m.observe_extracted("melanie --paints--> portraits", 200);
    m.maintain(200);
    let ctx = m.context_for_query_rich("What was grandma's gift to Melanie?", 400);
    assert!(ctx.contains("ATTRIBUTION"), "{ctx}");
    assert!(
        ctx.to_lowercase().contains("no topic facts for: melanie"),
        "{ctx}"
    );
    let ctx2 = m.context_for_query_rich("What is the latest amount Melanie charges?", 400);
    assert!(ctx2.contains("CURRENT STATE"), "{ctx2}");
    assert!(
        ctx2.contains("superseded: landscapes") || ctx2.contains("paints"),
        "{ctx2}"
    );
}

#[test]
fn kernel_code_facts_surive_strict_validation() {
    // the VR session dropped ~15 lines to token constraints: C syntax
    // in entities must pass as long as it is a single token
    let facts = "vme --field_type--> struct vm_map_entry\n\
                 pmap_list --head--> *pmap\n\
                 flags --requires--> MAP_FIXED|MAP_ANON\n\
                 entry --links--> entry->links\n\
                 vm_map --lookup--> vm_map_lookup_entry()\n\
                 page --addr_flags--> &vm_page[0]";
    let _m = AgentMemory::<MockExtractor>::new(MockExtractor::new(0.9), "").unwrap();
    let parsed = lemmalog::agent::parse_protocol_strict(facts, 0.9);
    assert_eq!(parsed.len(), 6, "all C-syntax facts parse: {parsed:?}");
    // leaked deliberation still dies: spaces + punctuation
    let bad = "x --note--> see the function at vm_map.c, around line 118";
    assert_eq!(lemmalog::agent::parse_protocol_strict(bad, 0.9).len(), 0);
}

#[test]
fn functional_relations_supersede_and_multi_accumulate_silently() {
    let mut m = AgentMemory::<MockExtractor>::new(MockExtractor::new(0.9), "").unwrap();
    // status is functional: a new status must REPLACE, not join, the old
    m.observe_extracted("hyp_1 --status--> proposed", 100);
    let (r1, _) = m.observe_extracted("hyp_1 --status--> supported", 200);
    m.maintain(200);
    assert_eq!(r1.escalations.len(), 0, "no conflict noise on status");
    assert_eq!(r1.updated, 1, "supersede counts as update");
    let open: Vec<_> = m
        .engine
        .query("edge", &[None, None, None, None, None, None])
        .into_iter()
        .map(|(k, _)| k)
        .filter(|k| {
            m.engine.interner.display(&k[1]) == "status"
                && matches!(k[4].as_int(), Some(vt) if vt == i64::MAX)
        })
        .collect();
    assert_eq!(open.len(), 1, "exactly one open status: {open:?}");
    // evidence is multi-valued: every add accumulates without escalation.
    // Evidence objects take the protocol shapes: a bare source reference
    // (space-free) or a punctuation-free phrase — spaced prose with
    // reference punctuation stays dropped (deliberation guard)
    let (r2, _) = m.observe_extracted(
        "hyp_1 --evidence--> vhost.c:3052\nhyp_1 --evidence--> regression test fails",
        300,
    );
    assert_eq!(r2.escalations.len(), 0, "evidence adds silently");
    m.maintain(300);
    assert_eq!(
        m.ask("current(\"hyp_1\", \"evidence\", E)").unwrap().len(),
        2
    );
}

#[test]
fn quoted_objects_parse_to_clean_symbols() {
    // issue report: models habitually quote multi-word claims; the
    // protocol had no quoting, so '"mean"' landed as a symbol WITH
    // literal quotes and quoted spaced phrases died as prose
    let facts = "hyp_1 --hypothesis--> \"mean field swaps mu and nu\"\n\
                 \"Caroline\" --paints--> lake sunrise\n\
                 hyp_1 --status--> 'supported'";
    let parsed = lemmalog::agent::parse_protocol_strict(facts, 0.9);
    assert_eq!(parsed.len(), 3, "{parsed:?}");
    assert_eq!(parsed[0].obj, "mean field swaps mu and nu");
    assert_eq!(parsed[1].subj, "Caroline");
    assert!(!parsed[0].obj.contains('"'));
    // unbalanced quotes stay what they are and die as prose
    let bad = "hyp_1 --hypothesis--> \"unclosed claim about things";
    assert!(lemmalog::agent::parse_protocol_strict(bad, 0.9).is_empty());
}

#[test]
fn underscore_prefixed_variables_parse_as_variables() {
    // issue #6: `_Y` (Prolog named don't-care) parsed as a constant, so
    // rules using it silently derived nothing; `_foo` stays a constant
    let mut m = AgentMemory::<MockExtractor>::new(MockExtractor::new(0.9), "").unwrap();
    m.observe_extracted("n0 --requires--> m0\nn1 --requires--> m1", 100);
    m.maintain(100);
    m.install_rules("interior(X) :- current(X, \"requires\", _Y).").unwrap();
    m.maintain(100);
    assert_eq!(m.ask("interior(X)").unwrap().len(), 2, "_Y is a variable");
    m.install_rules("tagged(X) :- current(X, \"requires\", _foo).").unwrap();
    m.maintain(100);
    // `_foo` is a constant that matches nothing: still derives nothing
    assert_eq!(m.ask("tagged(X)").unwrap().len(), 0, "_foo stays a constant");
}

#[test]
fn installing_a_shadow_rule_warns_about_union() {
    // issue #7: installing a "corrected" rule without uninstalling the old
    // one keeps both active (union); the response must say so
    let mut m = AgentMemory::<MockExtractor>::new(MockExtractor::new(0.9), "").unwrap();
    m.observe_extracted("n0 --requires--> m0", 100);
    m.maintain(100);
    let b1 = m.install_rules("dep(A, B) :- current(A, \"requires\", B).").unwrap();
    assert!(m.batch_conflicts(&b1).is_empty(), "first install is clean");
    let b2 = m
        .install_rules("dep(A, B) :- current(A, \"requires\", B), current(A, \"keep\", yes).")
        .unwrap();
    let warns = m.batch_conflicts(&b2);
    assert_eq!(warns.len(), 1, "{warns:?}");
    assert!(warns[0].contains("dep is also defined by batch(es) b"), "{warns:?}");
    assert!(warns[0].contains("UNION"), "{warns:?}");
}

#[test]
fn bitemporal_stamp_and_belief_query() {
    // the review's central finding: asserted_at was always a copy of
    // valid_from, so "what did the store believe at T" was unanswerable
    // while the docs claimed bitemporal. Now the write boundary stamps
    // asserted_at from the wall clock, and believed/4 answers point-in-
    // time knowledge — including "valid before known"
    let mut m = AgentMemory::<MockExtractor>::new(MockExtractor::new(0.9), "").unwrap();
    // backdated observe: the fact was true at ts=1000, learned now
    m.observe_extracted("alice --works_at--> acme", 1000);
    m.maintain(1000);
    let rows = m.engine.relation_keys("edge");
    let (vf, at) = match &rows[0][..] {
        [.., Value::Int(vf), _, Value::Int(at)] => (*vf, *at),
        _ => panic!("edge arity"),
    };
    assert_eq!(vf, 1000, "valid_from is the backdated ts");
    assert!(at > 1_700_000_000, "asserted_at is wall time (unix seconds), got {at}");
    assert!(at > vf, "the two clocks differ on a backdated write");
    // point-in-time belief via ask_deep with T bound
    let known_now = m
        .ask_deep("believed(\"alice\", \"works_at\", O, 9999999999)")
        .unwrap();
    assert_eq!(known_now.len(), 1, "known after asserted_at");
    // the money shot: between valid_from and asserted_at the fact was
    // TRUE but NOT YET KNOWN — believed must exclude it
    let mid = (vf + at) / 2;
    let mid_belief = m
        .ask_deep(&format!("believed(\"alice\", \"works_at\", O, {mid})"))
        .unwrap();
    assert!(mid_belief.is_empty(), "valid before known: {mid_belief:?}");
    // and before valid_from it was neither true nor known
    let early = m
        .ask_deep("believed(\"alice\", \"works_at\", O, 999)")
        .unwrap();
    assert!(early.is_empty());
}

#[test]
fn exclusive_matches_the_relation_name_exactly() {
    // chk-n5va.162 suspected `exclusive()` matched relation names by prefix, so
    // declaring plan_spring_2027 exclusive collapsed a 3-course plan. The lookup
    // is an exact key on the `exclusive` relation; the prefix matching in that
    // bead was the CALLER's (the MemDerive harness declared every plan_<term>
    // relation exclusive). See FORK-NOTES.md, "Encoding a multi-valued fact
    // that also needs revision".
    let mut m = mem("exclusive(\"plan_spring_2027\").");
    m.maintain(1); // materialize the rule's base fact before any update decision
    m.observe(
        "milo --plan_spring_2027--> crt_330\n\
         milo --plan_spring_2027--> gls_220\n\
         milo --plan_spring_2027_v2--> tde_380\n\
         milo --plan_spring_2027_v2--> hum_120",
    );
    m.maintain(100);
    // declared exclusive: one open object, the later assertion replaces
    assert_eq!(
        m.ask("current(\"milo\", \"plan_spring_2027\", O)")
            .unwrap()
            .len(),
        1
    );
    // a look-alike name that merely shares the prefix is untouched
    assert_eq!(
        m.ask("current(\"milo\", \"plan_spring_2027_v2\", O)")
            .unwrap()
            .len(),
        2,
        "exclusivity must not leak to a relation that shares the prefix"
    );
}

#[test]
fn declaring_the_prefix_alone_does_not_make_a_relation_exclusive() {
    // the other direction of the same guarantee: `exclusive("plan")` is a
    // declaration about a relation literally named `plan`
    let mut m = mem("exclusive(\"plan\").");
    m.maintain(1);
    m.observe("milo --plan_spring_2027--> crt_330\nmilo --plan_spring_2027--> gls_220");
    m.maintain(100);
    assert_eq!(
        m.ask("current(\"milo\", \"plan_spring_2027\", O)")
            .unwrap()
            .len(),
        2,
        "a set-valued relation stays set-valued"
    );
}

/// Regression, br-gx5.13. A fact whose `ts` is older than an open edge's
/// `valid_from` used to close that edge anyway. The result was an interval
/// valid never (`VF=2000, VT=1500`), a silently destroyed newer value, and
/// `updated=1` in the report — and `retract` could not undo it, because it
/// only matches open rows.
#[test]
fn out_of_order_fact_never_closes_a_newer_edge() {
    let mut m = mem("");
    // session A: the store learns v2.0.0, valid from ts=2000
    assert_eq!(m.observe_at("svc --version--> 2.0.0", 2000).added, 1);
    m.maintain(2000);

    // session B, older than A, arrives afterwards in wall time
    let r = m.observe_at("svc --version--> 1.5.0", 1500);
    assert_eq!(
        r.updated, 0,
        "a backdated fact must not supersede the newer one: {r:?}"
    );
    m.maintain(2000);

    // current() keeps answering the newer value, and only it
    assert_eq!(
        m.ask("current(\"svc\", \"version\", O)").unwrap(),
        vec!["O=2.0.0".to_string()],
        "the newer value must stay the current one"
    );

    // no edge in the store may be valid never
    let rows = m
        .engine
        .query("edge", &[None, None, None, None, None, None]);
    let inverted: Vec<(i64, i64)> = rows
        .iter()
        .filter_map(|(k, _)| match (k[3].as_int(), k[4].as_int()) {
            (Some(vf), Some(vt)) if vf > vt => Some((vf, vt)),
            _ => None,
        })
        .collect();
    assert!(
        inverted.is_empty(),
        "edge rows whose valid_from is after their valid_to: {inverted:?}"
    );
    let open = rows
        .iter()
        .filter(|(k, _)| matches!(k[4].as_int(), Some(vt) if vt == i64::MAX))
        .count();
    assert_eq!(open, 1, "exactly one open edge for the slot");
}

/// The other half of br-gx5.13: a backdated fact is not discarded, it is
/// recorded as the closed interval it really was, the newer edge is left
/// alone, and the slot keeps accepting later values normally.
#[test]
fn backdated_fact_is_recorded_as_closed_history() {
    let mut m = mem("");
    m.observe_at("svc --version--> 2.0.0", 2000);
    m.maintain(2000);

    let r = m.observe_at("svc --version--> 1.5.0", 1500);
    assert_eq!(
        (r.updated, r.historical),
        (0, 1),
        "a backdated fact is reported, not applied as an update: {r:?}"
    );
    m.maintain(2000);

    // history is kept as [1500, 2000); the open edge is untouched
    let mut rows = slot_edges(&m);
    rows.sort();
    assert_eq!(
        rows,
        vec![
            ("1.5.0".to_string(), 1500, 2000),
            ("2.0.0".to_string(), 2000, i64::MAX),
        ],
        "the store now knows when 2.0.0 took over"
    );

    // the guard does not wedge the slot: a later value supersedes normally
    let r = m.observe_at("svc --version--> 3.0.0", 3000);
    assert_eq!((r.updated, r.historical), (1, 0), "{r:?}");
    m.maintain(3000);
    assert_eq!(
        m.ask("current(\"svc\", \"version\", O)").unwrap(),
        vec!["O=3.0.0".to_string()]
    );

    // and an EQUAL ts is not "backdated": it supersedes (VF == VT is empty,
    // not inverted) — the guard must not over-refuse
    let r = m.observe_at("svc --version--> 3.1.0", 3000);
    assert_eq!((r.updated, r.historical), (1, 0), "{r:?}");
    m.maintain(3000);
    assert_eq!(
        m.ask("current(\"svc\", \"version\", O)").unwrap(),
        vec!["O=3.1.0".to_string()]
    );
    let inverted = slot_edges(&m).into_iter().filter(|&(_, vf, vt)| vf > vt).count();
    assert_eq!(inverted, 0);
}

/// `(object, valid_from, valid_to)` for every `edge` row in the store —
/// regression tests read the raw intervals, not `current/3`, because the bug
/// they guard is precisely a row `current/3` cannot show.
fn slot_edges(m: &lemmalog::AgentMemory<MockExtractor>) -> Vec<(String, i64, i64)> {
    m.engine
        .query("edge", &[None, None, None, None, None, None])
        .iter()
        .map(|(k, _)| {
            (
                m.engine.interner.display(&k[2]),
                k[3].as_int().unwrap_or(-1),
                k[4].as_int().unwrap_or(-1),
            )
        })
        .collect()
}

/// Stores written before the guard can already hold two open rows for one
/// slot. A backdated fact must close only the rows that started at or before
/// it, and must leave the newer open row exactly as it was.
#[test]
fn backdated_fact_closes_only_older_open_rows() {
    fn plant(m: &mut lemmalog::AgentMemory<MockExtractor>, obj: &str, vf: i64) {
        let s = m.engine.sym("svc");
        let p = m.engine.sym("version");
        let o = m.engine.sym(obj);
        m.engine.declare(
            "edge",
            &[
                s,
                p,
                o,
                Value::Int(vf),
                Value::Int(i64::MAX),
                Value::Int(1),
            ],
            lemmalog::Ann::unit(),
        );
    }

    let mut m = mem("");
    plant(&mut m, "1.0.0", 1000);
    plant(&mut m, "2.0.0", 2000);

    let r = m.observe_at("svc --version--> 1.5.0", 1500);
    assert_eq!((r.updated, r.historical), (0, 1), "{r:?}");
    m.maintain(2000);

    let mut rows = slot_edges(&m);
    rows.sort();
    assert_eq!(
        rows,
        vec![
            ("1.0.0".to_string(), 1000, 1500),
            ("1.5.0".to_string(), 1500, 2000),
            ("2.0.0".to_string(), 2000, i64::MAX),
        ],
        "only the row the fact supersedes is closed"
    );
    assert_eq!(
        m.ask("current(\"svc\", \"version\", O)").unwrap(),
        vec!["O=2.0.0".to_string()]
    );
}
