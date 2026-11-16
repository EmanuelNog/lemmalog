use lemmalog::{Ann, Engine, Value};

fn syms(e: &mut Engine, xs: &[&str]) -> Vec<Value> {
    xs.iter().map(|x| e.sym(x)).collect()
}

#[test]
fn count_min_max_sum() {
    let mut e = Engine::new();
    e.install_program(
        "kit_count(P, count(K)) :- bought(P, K).\n\
         kit_max(P, max(N)) :- rating(P, N).\n\
         kit_min(P, min(N)) :- rating(P, N).\n\
         total(P, sum(N)) :- rating(P, N).",
    )
    .unwrap();
    let rows = [
        ("alice", "f15"),
        ("alice", "spitfire"),
        ("alice", "tiger"),
        ("bob", "camaro"),
    ];
    for (p, k) in rows {
        let v = syms(&mut e, &[p, k]);
        e.declare("bought", &v, Ann::unit());
    }
    for (p, n) in [("alice", 5), ("alice", 3), ("alice", 4), ("bob", 2)] {
        let p = e.sym(p);
        e.declare("rating", &[p, Value::Int(n)], Ann::unit());
    }
    e.run();
    let alice = e.sym("alice");
    let c = e.query("kit_count", &[Some(alice), None]);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].0[1], Value::Int(3), "count distinct kits");
    assert_eq!(
        e.query("kit_max", &[Some(alice), None])[0].0[1],
        Value::Int(5)
    );
    assert_eq!(
        e.query("kit_min", &[Some(alice), None])[0].0[1],
        Value::Int(3)
    );
    assert_eq!(
        e.query("total", &[Some(alice), None])[0].0[1],
        Value::Int(12)
    );
    // bob's group separate
    let bob = e.sym("bob");
    assert_eq!(
        e.query("kit_count", &[Some(bob), None])[0].0[1],
        Value::Int(1)
    );
}

#[test]
fn count_grows_and_propagates() {
    let mut e = Engine::new();
    e.install_program(
        "kit_count(P, count(K)) :- bought(P, K).\n\
         big_spender(P) :- kit_count(P, N), N >= 3.",
    )
    .unwrap();
    for k in ["a", "b"] {
        let v = syms(&mut e, &["alice", k]);
        e.declare("bought", &v, Ann::unit());
    }
    e.run();
    assert_eq!(e.query("kit_count", &[]).len(), 1);
    assert_eq!(e.query("kit_count", &[None, None])[0].0[1], Value::Int(2));
    assert_eq!(e.query("big_spender", &[]).len(), 0);

    // third kit arrives in a later epoch: count 2 -> 3 flips the reader
    let v = syms(&mut e, &["alice", "c"]);
    e.declare("bought", &v, Ann::unit());
    e.run();
    let rows = e.query("kit_count", &[]);
    assert_eq!(rows.len(), 1, "no stale count row");
    assert_eq!(rows[0].0[1], Value::Int(3));
    assert_eq!(
        e.query("big_spender", &[]).len(),
        1,
        "value change propagates"
    );
}

#[test]
fn retraction_shrinks_count() {
    let mut e = Engine::new();
    e.install_program("kit_count(P, count(K)) :- bought(P, K).")
        .unwrap();
    let kits: Vec<Vec<Value>> = ["a", "b", "c"]
        .iter()
        .map(|k| syms(&mut e, &["alice", k]))
        .collect();
    for k in &kits {
        e.declare("bought", k, Ann::unit());
    }
    e.run();
    assert_eq!(e.query("kit_count", &[])[0].0[1], Value::Int(3));
    assert!(e.retract("bought", &kits[0]));
    e.run();
    let rows = e.query("kit_count", &[]);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].0[1],
        Value::Int(2),
        "count recomputed after retraction"
    );
}

#[test]
fn multiple_aggregate_columns() {
    let mut e = Engine::new();
    e.install_program("stats(P, count(K), max(R)) :- bought(P, K, R).")
        .unwrap();
    for (k, r) in [("a", 3), ("b", 7), ("c", 5)] {
        let alice = e.sym("alice");
        let v = vec![alice, e.sym(k), Value::Int(r)];
        e.declare("bought", &v, Ann::unit());
    }
    e.run();
    let rows = e.query("stats", &[]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0[1], Value::Int(3));
    assert_eq!(rows[0].0[2], Value::Int(7));
}

#[test]
fn rejects_bad_aggregation_programs() {
    // aggregate in a body atom
    let mut e = Engine::new();
    assert!(e.install_program("p(X) :- q(X), r(count(X)).").is_err());
    // recursion through the aggregated head
    let mut e = Engine::new();
    assert!(
        e.install_program("p(X, count(Y)) :- p(X, Y), q(X, Y).")
            .is_err(),
        "aggregation over its own head must be rejected"
    );
    // mixed definition
    let mut e = Engine::new();
    assert!(e
        .install_program("p(X, count(Y)) :- q(X, Y).\np(X, Y) :- q(X, Y).")
        .is_err());
    // unbound group variable
    let mut e = Engine::new();
    assert!(e.install_program("p(Z, count(Y)) :- q(X, Y).").is_err());
}

#[test]
fn aggregate_witness_in_why() {
    let mut e = Engine::new();
    e.install_program("kits: kit_count(P, count(K)) :- bought(P, K).")
        .unwrap();
    let v = syms(&mut e, &["alice", "spitfire"]);
    e.declare("bought", &v, Ann::unit());
    e.run();
    let alice = e.sym("alice");
    let out = e.why("kit_count", &[alice, Value::Int(1)]);
    assert!(out.contains("via kits"), "{out}");
}

// ---------------------------------------------------------------- aggregate
// multiplicity (chk-n5va.160)
//
// What count()/sum() mean here, stated once so the tests below read as
// instances of one rule (see `lower_agg_clause` and README "Aggregation"):
//
//   The fold's unit is one SOURCE ROW: a row of the leftmost positive body
//   atom that binds the aggregated value. sum() adds that column over the
//   group's distinct source rows; count() counts them. So a value repeated by
//   two different rows counts twice (two 11-credit courses are 22 credits),
//   while the same row reached through two join paths counts once (two
//   sections of one 3-credit course are 3 credits).
//
// Folding the projected (group, value) tuple instead — what the engine did
// before this — merges equal values and under-counts; that is the MemDerive
// failure (24 credits totalling 76 answered sum=7, count=2).

#[test]
fn sum_folds_one_row_per_source_row_not_per_distinct_value() {
    let mut e = Engine::new();
    e.install_program(
        "credits(P, sum(N)) :- credit(P, C, N).\n\
         n_credits(P, count(N)) :- credit(P, C, N).\n\
         top(P, max(N)) :- credit(P, C, N).",
    )
    .unwrap();
    for (c, n) in [("math", 11), ("phys", 11), ("chem", 4)] {
        let alice = e.sym("alice");
        let v = vec![alice, e.sym(c), Value::Int(n)];
        e.declare("credit", &v, Ann::unit());
    }
    e.run();
    let alice = e.sym("alice");
    assert_eq!(
        e.query("credits", &[Some(alice), None])[0].0[1],
        Value::Int(26),
        "11 + 11 + 4: the repeated value is two rows, not one"
    );
    assert_eq!(
        e.query("n_credits", &[Some(alice), None])[0].0[1],
        Value::Int(3),
        "count counts rows, not distinct values"
    );
    assert_eq!(e.query("top", &[Some(alice), None])[0].0[1], Value::Int(11));
}

#[test]
fn sum_over_a_category_counts_equal_credits_per_course() {
    // The MemDerive shape verbatim (q013): the value comes from a view keyed
    // by course, the category comes from a join, and two Foundation courses
    // carry the same credit value. Answered 7 before the fix (4+3); the four
    // Foundation courses here are worth 14.
    let mut e = Engine::new();
    e.install_program(
        "credited(S, C, N) :- passed(S, C), course_credits(C, N).\n\
         category_credits(S, Cat, sum(N)) :- credited(S, C, N), category_of(C, Cat).",
    )
    .unwrap();
    let alice = e.sym("alice");
    let foundation = e.sym("Foundation");
    for (c, n) in [("crt-101", 4), ("crt-115", 4), ("hum-120", 3), ("tde-380", 3)] {
        let c = e.sym(c);
        e.declare("passed", &[alice, c], Ann::unit());
        e.declare("course_credits", &[c, Value::Int(n)], Ann::unit());
        e.declare("category_of", &[c, foundation], Ann::unit());
    }
    e.run();
    assert_eq!(
        e.query("category_credits", &[Some(alice), Some(foundation), None])[0].0[2],
        Value::Int(14),
        "4 + 4 + 3 + 3"
    );
}

#[test]
fn sum_counts_a_repeated_identical_row_once() {
    // A set relation: asserting the same fact twice is one row, so the fold
    // must not see it twice.
    let mut e = Engine::new();
    e.install_program("total(P, sum(N)) :- spend(P, N).").unwrap();
    let alice = e.sym("alice");
    let row = vec![alice, Value::Int(8)];
    e.declare("spend", &row, Ann::unit());
    e.declare("spend", &row, Ann::base(0.5, ["ep2"]));
    e.run();
    assert_eq!(e.query("total", &[])[0].0[1], Value::Int(8));
}

#[test]
fn sum_does_not_double_count_a_row_duplicated_by_a_one_to_many_join() {
    // The case that made set semantics look attractive: a second section of
    // the same course doubles the body's solutions, but the row the aggregate
    // folds (course(math, 3)) is one row. 3, not 6.
    let mut e = Engine::new();
    e.install_program(
        "credits(P, sum(N)) :- enrol(P, S), section(S, C), course(C, N).\n\
         n_courses(P, count(N)) :- enrol(P, S), section(S, C), course(C, N).\n\
         n_sections(P, count(S)) :- enrol(P, S), section(S, C), course(C, N).",
    )
    .unwrap();
    let alice = e.sym("alice");
    let math = e.sym("math");
    for s in ["s1", "s2"] {
        let s = e.sym(s);
        e.declare("enrol", &[alice, s], Ann::unit());
        e.declare("section", &[s, math], Ann::unit());
    }
    e.declare("course", &[math, Value::Int(3)], Ann::unit());
    e.run();
    assert_eq!(
        e.query("credits", &[])[0].0[1],
        Value::Int(3),
        "one course row, however many sections join to it"
    );
    assert_eq!(
        e.query("n_courses", &[])[0].0[1],
        Value::Int(1),
        "count counts source rows, not body solutions"
    );
    assert_eq!(
        e.query("n_sections", &[])[0].0[1],
        Value::Int(2),
        "aggregating over the join column still sees both sections"
    );
}
