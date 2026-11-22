use super::*;
use crate::journal::parse_rfc3339_ms;
use serde_json::json;

/// 2026-11-22T10:00:00Z plus `secs`.
fn at(secs: i64) -> i64 {
    parse_rfc3339_ms("2026-11-22T10:00:00Z").unwrap() + secs * 1000
}

fn run(id: &str, bead: Option<&str>, created: i64) -> WorkRun {
    WorkRun {
        id: id.into(),
        rig: "rig".into(),
        formula: "work".into(),
        bead: bead.map(Into::into),
        bead_title: None,
        bead_parent: None,
        parent_run: None,
        parent_round: None,
        round: 1,
        live: false,
        created_ms: at(created),
    }
}

fn child(id: &str, bead: Option<&str>, parent: &str, round: u32, created: i64) -> WorkRun {
    WorkRun {
        parent_run: Some(parent.into()),
        parent_round: Some(round),
        ..run(id, bead, created)
    }
}

fn attempt(run: &str, n: u32, round: u32, session: &str, started: i64) -> WorkAttempt {
    WorkAttempt {
        run: run.into(),
        n,
        round,
        step: "work".into(),
        harness: "claude".into(),
        session: SessionRef(json!({ "session_id": session })),
        workdir: Some(format!("/w/{run}")),
        started_ms: at(started),
        ended_ms: Some(at(started + 30)),
        prompts: vec![format!("{run}-p{n}")],
    }
}

/// A tracker map from `(id, title, parent)` rows.
fn map(rows: &[(&str, &str, Option<&str>)]) -> BTreeMap<String, BeadRef> {
    rows.iter()
        .map(|(id, title, parent)| {
            (
                (*id).into(),
                BeadRef {
                    title: (*title).into(),
                    parent: parent.map(Into::into),
                },
            )
        })
        .collect()
}

/// `(id, parent, title)` of every node, in order.
fn shape(nodes: &[Node]) -> Vec<(&str, Option<&str>, &str)> {
    nodes
        .iter()
        .map(|n| (n.id.as_str(), n.parent.as_deref(), n.title.as_str()))
        .collect()
}

fn epic() -> BTreeMap<String, BeadRef> {
    map(&[
        ("E", "Epic", None),
        ("M", "Middle", Some("E")),
        ("S", "Sibling", Some("E")),
        ("A", "Leaf", Some("M")),
        ("B", "Other", Some("E")),
    ])
}

#[test]
fn tasks_direct_launch_hangs_under_the_ancestor_chain() {
    let runs = [run("run_1", Some("A"), 0)];
    let attempts = [attempt("run_1", 1, 1, "s1", 10)];
    let nodes = tree("E", &runs, &attempts, &epic());
    assert_eq!(
        shape(&nodes),
        [
            ("b:M", None, "M Middle"),
            ("b:A", Some("b:M"), "A Leaf"),
            ("r:run_1.1", Some("b:A"), "Round 1"),
            ("t:run_1.1", Some("r:run_1.1"), "work (claude)"),
        ]
    );
    assert_eq!(nodes[0].kind, Kind::Bead);
    assert!(nodes.iter().all(|n| n.at_ms == at(10)));
    let thread = nodes[3].thread.as_ref().unwrap();
    assert_eq!(thread.prompts, ["run_1-p1"]);
    assert_eq!(thread.workdir.as_deref(), Some("/w/run_1"));
    assert!(tree("M", &runs, &attempts, &epic()).is_empty());
}

#[test]
fn tasks_drain_child_on_another_bead_goes_under_its_round() {
    let mut drain = run("run_1", Some("E"), 0);
    drain.round = 2;
    let runs = [drain, child("run_2", Some("A"), "run_1", 2, 20)];
    let attempts = [
        attempt("run_1", 1, 1, "plan", 10),
        attempt("run_2", 1, 1, "s1", 30),
        attempt("run_2", 2, 2, "s2", 90),
    ];
    assert_eq!(
        shape(&tree("E", &runs, &attempts, &epic())),
        [
            ("r:run_1.1", None, "Round 1"),
            ("t:run_1.1", Some("r:run_1.1"), "work (claude)"),
            ("r:run_1.2", None, "Round 2"),
            ("b:run_1.2:M", Some("r:run_1.2"), "M Middle"),
            ("b:run_1.2:A", Some("b:run_1.2:M"), "A Leaf"),
            ("r:run_2.1", Some("b:run_1.2:A"), "Round 1"),
            ("t:run_2.1", Some("r:run_2.1"), "work (claude)"),
            ("r:run_2.2", Some("b:run_1.2:A"), "Round 2"),
            ("t:run_2.2", Some("r:run_2.2"), "work (claude)"),
        ]
    );
}

#[test]
fn tasks_same_bead_child_nests_its_rounds() {
    let runs = [run("run_1", Some("B"), 0), child("run_2", Some("B"), "run_1", 1, 15)];
    let attempts = [attempt("run_1", 1, 1, "s1", 10), attempt("run_2", 1, 1, "s2", 20)];
    assert_eq!(
        shape(&tree("E", &runs, &attempts, &epic())),
        [
            ("b:B", None, "B Other"),
            ("r:run_1.1", Some("b:B"), "Round 1"),
            ("t:run_1.1", Some("r:run_1.1"), "work (claude)"),
            ("r:run_2.1", Some("r:run_1.1"), "Round 1"),
            ("t:run_2.1", Some("r:run_2.1"), "work (claude)"),
        ]
    );
}

#[test]
fn tasks_beadless_child_inherits_its_parents_bead() {
    let runs = [run("run_1", Some("E"), 0), child("run_2", None, "run_1", 1, 15)];
    let attempts = [attempt("run_2", 1, 1, "s1", 20)];
    assert_eq!(
        shape(&tree("E", &runs, &attempts, &epic())),
        [
            ("r:run_1.1", None, "Round 1"),
            ("r:run_2.1", Some("r:run_1.1"), "Round 1"),
            ("t:run_2.1", Some("r:run_2.1"), "work (claude)"),
        ]
    );
    assert_eq!(tasks(&runs, &attempts, &epic()).len(), 1);
}

#[test]
fn tasks_repeated_runs_on_one_bead_tell_their_rounds_apart() {
    let runs = [run("run_1", Some("B"), 0), run("run_2", Some("B"), 3600)];
    let attempts = [attempt("run_1", 1, 1, "s1", 10), attempt("run_2", 1, 1, "s2", 3610)];
    let one = tree("E", &runs[..1], &attempts[..1], &epic());
    let both = tree("E", &runs, &attempts, &epic());
    assert_eq!(
        shape(&both),
        [
            ("b:B", None, "B Other"),
            ("r:run_1.1", Some("b:B"), "Round 1 · 2026-11-22 10:00 · run_1"),
            ("t:run_1.1", Some("r:run_1.1"), "work (claude)"),
            ("r:run_2.1", Some("b:B"), "Round 1 · 2026-11-22 11:00 · run_2"),
            ("t:run_2.1", Some("r:run_2.1"), "work (claude)"),
        ]
    );
    assert_eq!(one[1].title, "Round 1");
    let ids = |nodes: &[Node]| nodes.iter().map(|n| n.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids(&one), ids(&both)[..3]);
}

#[test]
fn tasks_round_without_session_appears_once_a_child_has_one() {
    let mut drain = run("run_1", Some("E"), 0);
    drain.live = true;
    let runs = [drain, child("run_2", Some("B"), "run_1", 1, 10)];
    assert!(tree("E", &runs, &[], &epic()).is_empty());
    let attempts = [attempt("run_2", 1, 1, "s1", 20)];
    let nodes = tree("E", &runs, &attempts, &epic());
    assert_eq!(
        shape(&nodes),
        [
            ("r:run_1.1", None, "Round 1"),
            ("b:run_1.1:B", Some("r:run_1.1"), "B Other"),
            ("r:run_2.1", Some("b:run_1.1:B"), "Round 1"),
            ("t:run_2.1", Some("r:run_2.1"), "work (claude)"),
        ]
    );
    assert_eq!(
        nodes.iter().map(|n| n.live).collect::<Vec<_>>(),
        [true, false, false, false]
    );
}

#[test]
fn tasks_exec_only_run_makes_a_task_without_nodes() {
    let runs = [run("run_1", Some("A"), 0)];
    assert_eq!(
        tasks(&runs, &[], &epic()),
        [Task {
            bead: "E".into(),
            title: "Epic".into(),
            updated_ms: at(0),
            live: false
        }]
    );
    assert!(tree("E", &runs, &[], &epic()).is_empty());
}

#[test]
fn tasks_beadless_run_is_omitted() {
    let runs = [run("run_1", None, 0), child("run_2", Some("B"), "run_1", 1, 10)];
    let attempts = [attempt("run_1", 1, 1, "s1", 5), attempt("run_2", 1, 1, "s2", 20)];
    let beads = beads(&runs);
    assert_eq!(
        tasks(&runs, &attempts, &beads)
            .iter()
            .map(|t| t.bead.as_str())
            .collect::<Vec<_>>(),
        ["B"]
    );
    assert_eq!(
        shape(&tree("B", &runs, &attempts, &beads)),
        [
            ("r:run_2.1", None, "Round 1"),
            ("t:run_2.1", Some("r:run_2.1"), "work (claude)")
        ]
    );
}

#[test]
fn tasks_resume_continues_a_thread_and_a_new_session_starts_one() {
    let runs = [run("run_1", Some("E"), 0)];
    let mut attempts = [
        attempt("run_1", 1, 1, "s1", 10),
        attempt("run_1", 2, 1, "s1", 60),
        attempt("run_1", 3, 1, "s2", 120),
    ];
    attempts[2].ended_ms = None;
    let nodes = tree("E", &runs, &attempts, &epic());
    assert_eq!(
        shape(&nodes),
        [
            ("r:run_1.1", None, "Round 1"),
            ("t:run_1.1", Some("r:run_1.1"), "work (claude) · 10:00:10"),
            ("t:run_1.3", Some("r:run_1.1"), "work (claude) · 10:02:00"),
        ]
    );
    assert_eq!(nodes[1].thread.as_ref().unwrap().prompts, ["run_1-p1", "run_1-p2"]);
    assert_eq!((nodes[1].live, nodes[2].live), (false, true));
}

#[test]
fn tasks_siblings_go_by_start_then_id() {
    let runs = [
        run("run_1", Some("E"), 0),
        child("run_2", Some("A"), "run_1", 1, 10),
        child("run_3", Some("B"), "run_1", 1, 20),
    ];
    let attempts = [attempt("run_2", 1, 1, "s1", 300), attempt("run_3", 1, 1, "s2", 200)];
    let nodes = tree("E", &runs, &attempts, &epic());
    let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "r:run_1.1",
            "b:run_1.1:B",
            "r:run_3.1",
            "t:run_3.1",
            "b:run_1.1:M",
            "b:run_1.1:A",
            "r:run_2.1",
            "t:run_2.1"
        ]
    );
    assert_eq!(nodes[0].at_ms, at(200));
}

#[test]
fn tasks_rebuild_from_the_same_rows_is_equal() {
    let runs = [
        run("run_1", Some("E"), 0),
        child("run_2", Some("A"), "run_1", 1, 10),
        child("run_3", Some("A"), "run_1", 1, 20),
        run("run_4", Some("B"), 30),
    ];
    let attempts = [
        attempt("run_2", 1, 1, "s1", 40),
        attempt("run_3", 1, 1, "s2", 40),
        attempt("run_4", 1, 1, "s3", 40),
        attempt("run_4", 2, 1, "s4", 40),
    ];
    let first = tree("E", &runs, &attempts, &epic());
    assert_eq!(first.len(), 11);
    assert_eq!(first, tree("E", &runs, &attempts, &epic()));
}

#[test]
fn tasks_go_by_last_activity_and_report_live() {
    let mut late = run("run_2", Some("F"), 50);
    late.live = true;
    let runs = [run("run_1", Some("E"), 100), late];
    let attempts = [attempt("run_2", 1, 1, "s1", 470)];
    let beads = map(&[("E", "Epic", None), ("F", "Feature", None)]);
    assert_eq!(
        tasks(&runs, &attempts, &beads),
        [
            Task {
                bead: "F".into(),
                title: "Feature".into(),
                updated_ms: at(500),
                live: true
            },
            Task {
                bead: "E".into(),
                title: "Epic".into(),
                updated_ms: at(100),
                live: false
            },
        ]
    );
}

#[test]
fn tasks_beads_take_the_newest_run() {
    let mut old = run("run_1", Some("A"), 0);
    old.bead_title = Some("Old".into());
    let mut new = run("run_2", Some("A"), 10);
    (new.bead_title, new.bead_parent) = (Some("New".into()), Some("E".into()));
    assert_eq!(beads(&[old, new]), map(&[("A", "New", Some("E"))]));
}

#[test]
fn tasks_only_the_current_round_of_a_live_run_is_live() {
    let runs = [WorkRun {
        live: true,
        round: 2,
        ..run("run_1", Some("E"), 0)
    }];
    let attempts = [attempt("run_1", 1, 1, "s1", 10), attempt("run_1", 2, 2, "s2", 60)];
    let nodes = tree("E", &runs, &attempts, &epic());
    let live: Vec<_> = nodes.iter().map(|n| (n.id.as_str(), n.live)).collect();
    assert_eq!(
        live,
        [
            ("r:run_1.1", false),
            ("t:run_1.1", false),
            ("r:run_1.2", true),
            ("t:run_1.2", false),
        ]
    );
}
