//! Triggers checked against the events and the formulas they name (docs/design/config.md).

use super::config::{BASE, fails, load_with};

/// The project file with its trigger replaced by `trigger`.
fn project_trigger(trigger: &str) -> String {
    let start = BASE[3].1.find("[[trigger]]").unwrap();
    format!("{}[[trigger]]\n{trigger}\n", &BASE[3].1[..start])
}

#[test]
fn triggers_are_checked_against_events_and_formulas() {
    let cases = [
        (
            "name = \"t\"\non = \"issue.ready\"\nwhere = { \"issue.nope\" = 1 }\nrun = \"audit\"",
            "has no `issue.nope`",
        ),
        (
            "name = \"t\"\non = \"run.passed\"\nwhere = { run = \"r1\" }\nrun = \"audit\"",
            "has no `run`",
        ),
        (
            "name = \"t\"\non = \"issue.gone\"\nrun = \"audit\"",
            "unknown event type `issue.gone`",
        ),
        (
            "name = \"t\"\nevery = \"1h\"\nrun = \"implement\"\ninputs = { issue = \"{{ event.issue.id }}\" }",
            "no event",
        ),
        (
            "name = \"t\"\non = \"issue.ready\"\nrun = \"implement\"\ninputs = { issue = \"{{ event.issue.nope }}\" }",
            "no `issue.nope`",
        ),
        (
            "name = \"t\"\non = \"issue.ready\"\nrun = \"implement\"\ninputs = { issue = \"x\", extra = 1 }",
            "no input `extra`",
        ),
        (
            "name = \"t\"\non = \"issue.ready\"\nrun = \"implement\"",
            "required input `issue`",
        ),
        (
            "name = \"t\"\non = \"issue.ready\"\ncron = \"0 3 * * *\"\nrun = \"audit\"",
            "exactly one of",
        ),
        ("name = \"t\"\ncron = \"0 3 * *\"\nrun = \"audit\"", "five cron fields"),
        ("name = \"t\"\nevery = \"3d\"\nrun = \"audit\"", "unit is one of"),
        (
            "name = \"t\"\ncron = \"0 3 * * *\"\nwhere = { project = \"x\" }\nrun = \"audit\"",
            "`where` needs `on`",
        ),
        (
            "name = \"t\"\ncron = \"0 3 * * *\"\nrun = \"audit\"\nrig = \"frontend\"",
            "no rig `frontend`",
        ),
        (
            "name = \"t\"\ncron = \"0 3 * * *\"\nrun = \"nothing\"",
            "no formula `nothing`",
        ),
    ];
    for (trigger, needle) in cases {
        fails(&[("shop/loop-manager.toml", project_trigger(trigger).as_str())], needle);
    }
    let with_rig = BASE[5]
        .1
        .replace("run = \"audit\"", "run = \"audit\"\nrig = \"backend\"");
    fails(&[("kit/pack.toml", with_rig.as_str())], "gets its rig from the project");
}

#[test]
fn cron_reads_the_system_zone_unless_timezone_names_one() {
    let with = |zone: &str| {
        BASE[0]
            .1
            .replace("projects", &format!("timezone = \"{zone}\"\nprojects"))
    };
    let config = load_with(&[("config.toml", with("Europe/Berlin").as_str())]).unwrap();
    assert_eq!(config.timezone.iana_name(), Some("Europe/Berlin"));
    fails(&[("config.toml", with("Mars/Olympus").as_str())], "`timezone`");
    assert_eq!(load_with(BASE).unwrap().timezone, jiff::tz::TimeZone::system());
}
