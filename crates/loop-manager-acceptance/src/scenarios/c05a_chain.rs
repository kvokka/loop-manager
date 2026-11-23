//! Criterion 5a: a chain without a model. An exec planner creates an epic of three issues with br; a project trigger
//! on `issue.ready` starts the executor on the next ready issue of that epic; every issue ends closed by its run, and
//! all four runs share the planner's correlation: br attributes the planner's writes to its attempt by `BD_ACTOR`.

use super::{br, exec_formula, tracker};
use crate::{
    Env,
    daemon::{Daemon, write},
};
use serde_json::Value;
use std::{collections::HashSet, sync::Arc, time::Duration};

const WITHIN: Duration = Duration::from_secs(60);

const TRIGGER: &str = "[[trigger]]\nname = \"next\"\non = \"issue.ready\"\nwhere = { \"issue.labels\" = \"auto\" }\n\
                       run = \"work\"\nrig = \"web\"\ninputs = { issue = { next_ready = true, epic = \"{{ \
                       event.issue.epic }}\" } }\n";

pub(crate) async fn run(env: Arc<Env>) -> String {
    let d = Daemon::start(&env, |root| {
        write(
            root.join("config/loop-manager/shop/loop-manager.toml"),
            format!("[rigs.web]\npath = \"web\"\n{TRIGGER}"),
        );
        let plan = "set -e; epic=$(br create Epic -t epic --silent); \
                    for n in 1 2 3; do br create \"Task $n\" --parent \"$epic\" -l auto --silent; done";
        exec_formula(
            root,
            "plan",
            "",
            &format!("\"sh\", \"-c\", {}", serde_json::to_string(plan).unwrap()),
        );
        exec_formula(root, "work", "issue = { type = \"task\", required = true }", "\"true\"");
        tracker(root);
        br(root, &["create", "Seed", "--silent"]);
    })
    .await;
    // The first read of a tracker is its history, not news: plan only after it (the seed turns ready).
    d.journal_until(WITHIN, |seen| seen.iter().any(|(k, _)| k == "issue.ready"))
        .await;
    let planner = d.create("plan").await;
    let count = |seen: &[(String, Value)], kind: &str| seen.iter().filter(|(k, _)| k == kind).count();
    // An issue is closed in br after its run passed (an effect of the terminal transaction).
    let journal = d
        .journal_until(WITHIN, |seen| {
            count(seen, "run.passed") == 4 && count(seen, "issue.closed") == 3
        })
        .await;

    let correlation = journal
        .iter()
        .find(|(_, e)| e["run"] == planner.as_str())
        .expect("the planner")
        .1["correlation"]
        .clone();
    assert!(correlation.is_string(), "{journal:?}");
    let runs: Vec<&Value> = journal
        .iter()
        .filter(|(k, _)| k == "run.passed")
        .map(|(_, e)| e)
        .collect();
    for run in &runs {
        assert_eq!(run["correlation"], correlation, "one chain: {run}");
    }
    let mut issues = HashSet::new();
    for run in runs.iter().filter(|e| e["run"] != planner.as_str()) {
        let view = d.get(&format!("/v1/runs/{}", run["run"].as_str().unwrap())).await;
        assert_eq!(view["formula"], "shop.work", "{view}");
        assert!(
            issues.insert(view["issue"].as_str().expect("an issue").to_owned()),
            "an issue taken twice: {view}"
        );
    }
    let listed: Value = serde_json::from_str(&br(d.dir.path(), &["list", "--all", "--json", "-l", "auto"])).unwrap();
    let epic = listed["issues"].as_array().expect("issues");
    assert_eq!(epic.len(), 3, "{listed}");
    for issue in epic {
        assert_eq!(issue["status"], "closed", "{issue}");
        assert!(
            issues.contains(issue["id"].as_str().unwrap()),
            "{issue} closed by a run of the chain"
        );
    }
    format!("4 runs in one correlation {correlation}")
}
