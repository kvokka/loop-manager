//! Criterion 16 on the real br: the daemon claims a run's issue and holds the claim through the death of the step
//! (a `kill -9` of the daemon mid-step) and its resumed attempt; `passed` closes the issue, `failed` returns it; five
//! requests at once for the next ready issue of an epic of three give each issue to exactly one run; an issue whose runs
//! ended `failed` `issue_failed_runs` times is no longer given out; every step gets the tracker's path in
//! `BEADS_DIR`; br holds no record of loop-manager's own beyond claims and closes.

use super::{br, exec_formula, tracker};
use crate::{
    Env,
    daemon::{Daemon, write},
};
use serde_json::{Value, json};
use std::{collections::HashSet, path::Path, sync::Arc, time::Duration};

const WITHIN: Duration = Duration::from_secs(60);

/// The issue `id` as br shows it.
fn show(root: &Path, id: &str) -> Value {
    let shown: Value = serde_json::from_str(&br(root, &["show", id, "--json"])).unwrap();
    shown[0].clone()
}

/// Waits until br shows the issue `id` with `status` and `assignee`: br is written after the run's transaction.
async fn until_shown(root: &Path, id: &str, status: &str, assignee: &Value) {
    let deadline = std::time::Instant::now() + WITHIN;
    loop {
        let shown = show(root, id);
        if shown["status"] == status && shown["assignee"] == *assignee {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{id} is not {status} by {assignee}: {shown}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Starts `formula` on the task input `issue`; the reply.
async fn start(d: &Daemon, formula: &str, issue: Value) -> Value {
    let body = json!({ "project": "shop", "rig": "web", "formula": formula, "inputs": { "issue": issue } });
    d.post("/v1/runs", body).await.unwrap()
}

pub(crate) async fn run(env: Arc<Env>) -> String {
    let mut issues: Vec<String> = Vec::new();
    let mut d = Daemon::start(&env, |root| {
        write(
            root.join("config/loop-manager/config.toml"),
            "projects = [\"shop\"]\nissue_failed_runs = 2\n",
        );
        let input = "issue = { type = \"task\", required = true }";
        exec_formula(
            root,
            "hold",
            input,
            "\"sh\", \"-c\", \"printf %s \\\"$BEADS_DIR\\\"; sleep 1\"",
        );
        exec_formula(root, "break", input, "\"sh\", \"-c\", \"exit 3\"");
        tracker(root);
        issues.push(br(root, &["create", "Alone", "--silent"]));
        let many = br(root, &["create", "Many", "-t", "epic", "--silent"]);
        for n in 1..=3 {
            issues.push(br(
                root,
                &["create", &format!("Part {n}"), "--parent", &many, "--silent"],
            ));
        }
        let once = br(root, &["create", "Once", "-t", "epic", "--silent"]);
        issues.push(br(root, &["create", "Flaky", "--parent", &once, "--silent"]));
        issues.extend([many, once]);
    })
    .await;
    let (alone, parts, flaky, many, once) = (&issues[0], &issues[1..4], &issues[4], &issues[5], &issues[6]);
    let root = d.dir.path().to_owned();
    let tracker_dir = root.join("config/loop-manager/shop/.beads");
    // The first read of the tracker fills the ready set the daemon picks from.
    let leaves: HashSet<&str> = issues[..5].iter().map(String::as_str).collect();
    let ready = |seen: &[(String, Value)]| {
        let ids = seen.iter().filter(|(k, _)| k == "issue.ready");
        ids.filter_map(|(_, e)| e["data"]["issue"]["id"].as_str())
            .filter(|id| leaves.contains(id))
            .count()
    };
    d.journal_until(WITHIN, |seen| ready(seen) == leaves.len()).await;

    // The claim through the death of the step and the resumed attempt; `passed` closes.
    let held = start(&d, "hold", json!(alone)).await["run"]
        .as_str()
        .unwrap()
        .to_owned();
    d.until_state(&held, "running", WITHIN).await;
    let owner = format!("loop-manager:{held}");
    assert_eq!(
        show(&root, alone)["assignee"],
        owner.as_str(),
        "claimed as the run prepares"
    );
    d.restart().await;
    assert_eq!(
        show(&root, alone)["assignee"],
        owner.as_str(),
        "held through the restart"
    );
    let view = d.until_state(&held, "passed", WITHIN).await;
    assert_eq!(
        view["attempts"].as_array().map(Vec::len),
        Some(2),
        "resumed after the crash: {view}"
    );
    until_shown(&root, alone, "closed", &json!(owner)).await;

    check_concurrent_claims(&d, many, parts).await;
    check_failed_issue_limit(&d, once, flaky).await;

    // Every step had the tracker's path; br holds nothing of loop-manager's but claims and closes.
    let journal = d
        .journal_until(WITHIN, |seen| {
            seen.iter().filter(|(k, _)| k == "run.passed").count() >= 4
        })
        .await;
    for (_, e) in journal.iter().filter(|(k, _)| k == "run.passed") {
        let stdout = &e["data"]["outcome"]["data"]["stdout"];
        assert_eq!(stdout.as_str().map(Path::new), Some(tracker_dir.as_path()), "{e}");
    }
    let listed: Value = serde_json::from_str(&br(&root, &["list", "--all", "--json"])).unwrap();
    let all = listed["issues"].as_array().expect("issues");
    assert_eq!(all.len(), issues.len(), "no issue of loop-manager's own: {listed}");
    for issue in all.iter().map(|b| show(&root, b["id"].as_str().unwrap())) {
        assert_eq!(issue["created_by"], "alice", "{issue}");
        let labels = issue["labels"].as_array().map_or(0, Vec::len);
        let comments = issue["comments"].as_array().map_or(0, Vec::len);
        assert_eq!(
            (labels, comments),
            (0, 0),
            "no label or comment of loop-manager's: {issue}"
        );
    }
    "claim held through a crash; 5 requests for an epic of 3: 3 runs, 2 nothing_ready; out after 2 failed runs".into()
}

async fn check_concurrent_claims(d: &Daemon, many: &str, parts: &[String]) {
    // Five requests at once for the next ready issue of an epic of three.
    let next = json!({ "next_ready": true, "epic": many });
    let replies = futures_join(d, &next).await;
    let runs: Vec<&str> = replies.iter().filter_map(|r| r["run"].as_str()).collect();
    let nothing = replies.iter().filter(|r| r["nothing_ready"] == true).count();
    assert_eq!((runs.len(), nothing), (3, 2), "{replies:?}");
    let mut given = HashSet::new();
    for run in &runs {
        let view = d.until_state(run, "passed", WITHIN).await;
        assert!(
            given.insert(view["issue"].as_str().unwrap().to_owned()),
            "an issue given twice: {view}"
        );
    }
    assert_eq!(
        given,
        parts.iter().cloned().collect(),
        "each issue of the epic to one run"
    );
}

async fn check_failed_issue_limit(d: &Daemon, once: &str, flaky: &str) {
    // `failed` returns the issue, and after `issue_failed_runs` failed runs it is no longer given out.
    for n in 1..=2 {
        let run = start(d, "break", json!({ "next_ready": true, "epic": once })).await;
        let run = run["run"]
            .as_str()
            .unwrap_or_else(|| panic!("failure {n}: {run}"))
            .to_owned();
        d.until_state(&run, "failed", WITHIN).await;
        until_shown(d.dir.path(), flaky, "open", &Value::Null).await;
        // Given out again only once the daemon has read it back into the ready set.
        let again = |seen: &[(String, Value)]| {
            let ready = seen
                .iter()
                .filter(|(k, e)| k == "issue.ready" && e["data"]["issue"]["id"] == flaky);
            ready.count() > n
        };
        d.journal_until(WITHIN, again).await;
    }
    let refused = start(d, "break", json!({ "next_ready": true, "epic": once })).await;
    assert_eq!(refused["nothing_ready"], true, "{refused}");
}

/// Five requests for `next` at once.
async fn futures_join(d: &Daemon, next: &Value) -> Vec<Value> {
    let one = || start(d, "hold", next.clone());
    let (a, b, c, e, f) = tokio::join!(one(), one(), one(), one(), one());
    vec![a, b, c, e, f]
}
