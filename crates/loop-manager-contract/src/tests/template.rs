use crate::template::{Filter, Inputs, SyntaxError, Template, Unresolved, event_view};
use serde_json::{Value, json};

const PROMPT: &[&str] = &["inputs", "issue", "rig", "run", "project"];

fn ready_event() -> Value {
    json!({
        "schema": 1, "seq": 7, "ts": "2027-01-15T08:00:00.000Z", "type": "issue.ready",
        "project": "web", "rig": null, "run": null, "correlation": null,
        "data": { "issue": { "id": "bd-1", "epic": "bd-0", "labels": ["auto", "ui"], "priority": 2 } }
    })
}

fn filter(table: Value) -> Result<Filter, SyntaxError> {
    Filter::parse(table.as_object().unwrap())
}

#[test]
fn the_event_view_is_flat() {
    let view = event_view(&ready_event());
    assert_eq!(view["seq"], json!(7));
    assert_eq!(view["type"], json!("issue.ready"));
    assert_eq!(view["issue"]["epic"], json!("bd-0"));
    assert!(view.get("data").is_none() && view.get("schema").is_none());
}

#[test]
fn where_matches_scalars_arrays_and_requires_every_entry() {
    let view = event_view(&ready_event());
    let yes = |t: Value| filter(t).unwrap().matches(&view);
    assert!(yes(json!({})), "an empty filter matches everything");
    assert!(
        yes(json!({ "issue.labels": "auto" })),
        "an array value contains the scalar"
    );
    assert!(!yes(json!({ "issue.labels": "backend" })));
    assert!(
        yes(json!({ "issue.labels": ["backend", "ui"] })),
        "contains any element"
    );
    assert!(yes(json!({ "issue.priority": [1, 2], "project": "web" })));
    assert!(
        !yes(json!({ "issue.priority": 2, "project": "api" })),
        "AND of all entries"
    );
    assert!(!yes(json!({ "rig": "main" })), "null does not match");
    assert!(!yes(json!({ "issue.owner": "me" })), "absent does not match");
    assert!(yes(json!({ "issue.priority": 2 })));
}

#[test]
fn where_rejects_what_it_cannot_match() {
    assert!(filter(json!({ "issue.labels": { "any": "x" } })).is_err());
    assert!(filter(json!({ "issue.labels": [] })).is_err());
    assert!(filter(json!({ "issue.priority": 1.5 })).is_err());
    assert!(filter(json!({ "issue..labels": "x" })).is_err());
    assert_eq!(
        filter(json!({ "issue.epic": "x" }))
            .unwrap()
            .paths()
            .collect::<Vec<_>>(),
        ["issue.epic"]
    );
}

#[test]
fn a_prompt_template_substitutes_scalars_writes_structures_as_toon_and_fails_on_missing_values() {
    let context = json!({
        "issue": { "id": "bd-1", "priority": 2, "labels": ["auto", "ui"] },
        "rig": { "path": "/src" },
        "inputs": {}
    });
    let t = Template::parse("Fix {{issue.id}} (p{{ issue.priority }}) in {{rig.path}}.", PROMPT).unwrap();
    assert_eq!(t.render(&context).unwrap(), "Fix bd-1 (p2) in /src.");
    assert_eq!(
        t.paths().collect::<Vec<_>>(),
        ["issue.id", "issue.priority", "rig.path"]
    );
    let missing = Template::parse("{{ inputs.goal }}", PROMPT).unwrap();
    assert_eq!(
        missing.render(&context),
        Err(Unresolved {
            path: "inputs.goal".into()
        })
    );
    let object = Template::parse("{{ rig }}", PROMPT).unwrap();
    assert_eq!(object.render(&context).unwrap(), "path: /src");
    let labels = Template::parse("Labels {{ issue.labels }}", PROMPT).unwrap();
    assert_eq!(labels.render(&context).unwrap(), "Labels [2]: auto,ui");
}

#[test]
fn template_syntax_is_checked_at_load() {
    let escaped = Template::parse(r#"literal {{"{{"}} and }} stay"#, PROMPT).unwrap();
    assert_eq!(escaped.render(&json!({})).unwrap(), "literal {{ and }} stay");
    assert!(Template::parse("{{ issue.id", PROMPT).is_err(), "unclosed");
    assert!(Template::parse("{{ }}", PROMPT).is_err(), "empty");
    assert!(Template::parse("{{ issue.id | upper }}", PROMPT).is_err(), "no filters");
    assert!(Template::parse("{{ event.issue }}", PROMPT).is_err(), "unknown root");
}

#[test]
fn inputs_keep_json_types_map_recursively_and_report_unresolved_paths() {
    let spec = json!({
        "issue": { "next_ready": true, "epic": "{{event.issue.epic}}" },
        "labels": "{{ event.issue.labels }}",
        "note": "from {{event.type}} #{{event.seq}}",
        "list": ["{{event.issue.priority}}", 3],
        "all": "labels {{ event.issue.labels }}"
    });
    let inputs = Inputs::parse(&spec, true).unwrap();
    let view = event_view(&ready_event());
    assert_eq!(
        inputs.resolve(Some(&view)).unwrap(),
        json!({
            "issue": { "next_ready": true, "epic": "bd-0" },
            "labels": ["auto", "ui"],
            "note": "from issue.ready #7",
            "list": [2, 3],
            "all": "labels [2]: auto,ui"
        })
    );
    let other =
        event_view(&json!({ "seq": 8, "type": "issue.ready", "data": { "issue": { "id": "bd-2", "labels": [] } } }));
    assert_eq!(
        inputs.resolve(Some(&other)),
        Err(Unresolved {
            path: "event.issue.epic".into()
        })
    );
}

#[test]
fn schedule_inputs_have_no_placeholders() {
    assert!(Inputs::parse(&json!({ "epic": "{{ event.issue.epic }}" }), false).is_err());
    let fixed = Inputs::parse(&json!({ "scope": "all", "depth": 1 }), false).unwrap();
    assert_eq!(fixed.resolve(None).unwrap(), json!({ "scope": "all", "depth": 1 }));
    assert!(
        Inputs::parse(&json!({ "x": "{{ issue.id }}" }), true).is_err(),
        "inputs start at `event`"
    );
}

#[test]
fn a_loaded_where_on_the_run_view_matches_a_journal_envelope() {
    use crate::config::Source;
    let trigger = "name = \"t\"\non = \"run.passed\"\nwhere = { \"run.formula\" = \"implement\" }\nrun = \"audit\"";
    let project = super::config::BASE[3].1;
    let project = format!(
        "{}[[trigger]]\n{trigger}\n",
        &project[..project.find("[[trigger]]").unwrap()]
    );
    let config = super::config::load_with(&[("shop/loop-manager.toml", project.as_str())]).unwrap();
    let Source::Event { filter, .. } = &config.projects[0].packs[0].triggers[0].source else {
        panic!("an event trigger");
    };
    let passed = |formula: &str| {
        let body = json!({
            "type": "run.passed", "project": "shop", "rig": "backend", "run": "r1", "correlation": null,
            "data": { "run": { "id": "r1", "formula": formula }, "outcome": { "result": "pass" } }
        });
        crate::envelope(3, 0, &body.to_string())
    };
    assert!(filter.matches(&event_view(&passed("implement"))));
    assert!(!filter.matches(&event_view(&passed("audit"))));
    assert_eq!(event_view(&passed("implement"))["run"]["id"], json!("r1"));
}

#[test]
fn event_paths_follow_the_payload_schemas() {
    use crate::config::schema::event_path;
    assert_eq!(event_path("issue.ready", "issue.labels", false), Some(true));
    assert_eq!(event_path("issue.ready", "issue.labels.x", false), Some(false));
    assert_eq!(event_path("issue.ready", "seq", false), Some(true));
    assert_eq!(event_path("issue.ready", "seq.x", false), Some(false));
    assert_eq!(event_path("issue.labeled", "label", false), Some(true));
    assert_eq!(event_path("issue.labeled", "ready", false), Some(true));
    assert_eq!(event_path("issue.labeled", "issue.id", false), Some(true));
    assert_eq!(event_path("issue.labeled", "changed", false), Some(false));
    assert_eq!(event_path("issue.ready", "label", false), Some(false));
    assert_eq!(event_path("run.passed", "run", false), Some(true));
    assert_eq!(event_path("run.passed", "run.formula", false), Some(true));
    assert_eq!(event_path("run.passed", "outcome.data.x.y", false), Some(true));
    assert_eq!(event_path("gate.opened", "rig", false), Some(true));
    assert_eq!(event_path("gate.opened", "gate", false), Some(false));
    assert_eq!(event_path("no.such", "seq", false), None);
    assert_eq!(
        event_path("run.passed", "run", true),
        Some(false),
        "an object never matches"
    );
    assert_eq!(event_path("run.passed", "run.formula", true), Some(true));
    assert_eq!(
        event_path("run.state_changed", "run", true),
        Some(true),
        "the envelope run id"
    );
    assert_eq!(event_path("run.state_changed", "run.formula", false), Some(false));
    assert_eq!(event_path("run.awaiting", "deadline", true), Some(true));
    assert_eq!(event_path("run.awaiting", "run.formula", true), Some(true));
    assert_eq!(event_path("run.step_skipped", "step", true), Some(true));
    assert_eq!(event_path("run.step_skipped", "run.formula", true), Some(true));
}
