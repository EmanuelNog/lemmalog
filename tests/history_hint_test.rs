//! FORK ADDITION: a `current(...)` answer of `false` must say whether the value
//! was deliberately closed (a tombstone) or merely superseded (a closed edge),
//! so an agent knows to look further instead of reading absence as never-known.
//! Counts only: the closed VALUES stay out of the answer, because keeping a
//! retired value out of the agent's context is the point of closing it.

use lemmalog::{AgentMemory, MockExtractor};

fn mem() -> AgentMemory<MockExtractor> {
    AgentMemory::new(MockExtractor::new(0.9), "").unwrap()
}

#[test]
fn a_live_value_gets_no_history_hint() {
    let mut m = mem();
    m.observe_at("svc --version--> 2.0.0", 1000);
    m.maintain(1000);
    assert!(
        m.history_hint("current(\"svc\", version, \"2.0.0\")")
            .is_none(),
        "a live value is not an absence; there is nothing to explain"
    );
}

#[test]
fn a_tombstoned_fact_reports_the_tombstone_and_the_closed_edge() {
    let mut m = mem();
    m.observe_at("svc --version--> 2.0.0", 1000);
    m.maintain(1000);
    let (closed, missing, _died) = m.close_facts("svc --version--> 2.0.0");
    assert_eq!(closed.len(), 1);
    assert!(missing.is_empty());

    let hint = m
        .history_hint("current(\"svc\", version, \"2.0.0\")")
        .expect("a closed fact must explain the absence");
    assert!(hint.contains("no current value"), "{hint}");
    assert!(hint.contains("1 tombstone"), "{hint}");
    assert!(hint.contains("1 closed edge"), "{hint}");
    assert!(hint.contains("newest close 1000"), "{hint}");
    assert!(
        hint.contains("tombstone("),
        "the hint must name the follow-up query: {hint}"
    );
}

#[test]
fn a_superseded_fact_reports_history_without_a_tombstone() {
    let mut m = mem();
    m.observe_at("svc --version--> 2.0.0", 1000);
    m.observe_at("svc --version--> 2.1.0", 2000); // supersession: no tombstone
    m.maintain(2000);
    let hint = m
        .history_hint("current(\"svc\", version, \"2.0.0\")")
        .expect("a superseded value is history too");
    assert!(hint.contains("0 tombstone"), "{hint}");
    assert!(hint.contains("1 closed edge"), "{hint}");
    assert!(hint.contains("newest close 2000"), "{hint}");
}

#[test]
fn a_slot_with_no_live_value_counts_its_history() {
    let mut m = mem();
    m.observe_at("svc --version--> 2.0.0", 1000);
    m.maintain(1000);
    m.close_facts("svc --version--> 2.0.0");
    let hint = m
        .history_hint("current(\"svc\", version, O)")
        .expect("an empty slot must say whether it has history");
    assert!(hint.contains("no current value"), "{hint}");
    assert!(hint.contains("1 tombstone"), "{hint}");
    assert!(hint.contains("1 closed edge"), "{hint}");
}

#[test]
fn a_never_asserted_fact_says_there_is_no_recorded_history() {
    let mut m = mem();
    m.observe_at("svc --version--> 2.0.0", 1000);
    m.maintain(1000);
    let hint = m
        .history_hint("current(\"svc\", version, \"9.9.9\")")
        .expect("absence must still answer");
    assert!(hint.contains("no current value"), "{hint}");
    assert!(hint.contains("no recorded history"), "{hint}");
}

#[test]
fn a_non_current_goal_gets_no_history_hint() {
    let mut m = mem();
    m.observe_at("svc --version--> 2.0.0", 1000);
    m.maintain(1000);
    assert!(m.history_hint("version(svc, 2.0.0)").is_none());
}
