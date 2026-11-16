//! Yes/no questions (chk-n5va.161): a fully-ground goal answered `true`/`false`.
//!
//! Before this, `ask` reported a holding ground goal as one row with no
//! bindings — rendered as the empty string, which the CLI printed as a blank
//! line and the MCP tool as an empty result. Exactly what a goal that binds
//! nothing returned. `Engine::holds` is the verdict surface; `ask` keeps its
//! bindings-only contract.

use lemmalog::{AgentMemory, MockExtractor};

fn memory() -> AgentMemory<MockExtractor> {
    let mut m = AgentMemory::new(MockExtractor::new(0.9), "").unwrap();
    m.observe("milo --advisor--> ferrin");
    m.maintain(100);
    m
}

#[test]
fn ground_atom_that_holds_is_true() {
    let m = memory();
    assert_eq!(
        m.holds("current(\"milo\", \"advisor\", \"ferrin\")").unwrap(),
        Some(true)
    );
}

#[test]
fn ground_atom_that_does_not_hold_is_false() {
    let m = memory();
    // a different object for a relation the store does hold
    assert_eq!(
        m.holds("current(\"milo\", \"advisor\", \"other\")").unwrap(),
        Some(false)
    );
    // a relation the store never received: closed world -> false
    assert_eq!(
        m.holds("current(\"milo\", \"never_asserted\", \"x\")").unwrap(),
        Some(false)
    );
    // an unknown predicate: false, not an error
    assert_eq!(m.holds("no_such_predicate(\"milo\")").unwrap(), Some(false));
}

#[test]
fn a_goal_with_variables_gets_bindings_not_a_verdict() {
    let m = memory();
    assert_eq!(m.holds("current(\"milo\", \"advisor\", A)").unwrap(), None);
    assert_eq!(
        m.ask("current(\"milo\", \"advisor\", A)").unwrap(),
        vec!["A=ferrin".to_string()],
        "the bindings path is unchanged"
    );
    // a wildcard is a variable too: it asks to see rows, not a verdict
    assert_eq!(m.holds("current(\"milo\", \"advisor\", _)").unwrap(), None);
}

#[test]
fn ask_reports_a_holding_ground_goal_as_one_row_with_no_bindings() {
    // The shape `holds` exists to replace: one row, no text. Pinned so nobody
    // "fixes" ask into returning the string "true" and breaks row counting.
    let m = memory();
    assert_eq!(
        m.ask("current(\"milo\", \"advisor\", \"ferrin\")").unwrap(),
        vec![String::new()]
    );
    assert!(m
        .ask("current(\"milo\", \"advisor\", \"other\")")
        .unwrap()
        .is_empty());
}

#[test]
fn a_malformed_goal_is_still_a_parse_error() {
    let m = memory();
    assert!(m.holds("current(\"milo\", \"advisor\"").is_err());
}
