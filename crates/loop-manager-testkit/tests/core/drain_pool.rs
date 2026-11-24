//! `pool` and `on_item_failure` of a drain step (docs/design/config.md, criterion 30): at most `pool` issues in work at
//! once, 1 unless set; after a failed issue `soft_stop` gives no new issue and lets the started end, `hard_stop` cancels
//! them, and the step fails, a restart in between giving no new issue either.

use crate::common;

use common::{POLICY, World, drain::SWEEP, issues::slow_turn, world};
use loop_manager_core::run::{Decision, Verdict};
use serde_json::{Value, json};
use std::{collections::BTreeSet, time::Duration};

/// Formulas `one` (no `pool`), `two`, `soft`, `hard` and `again` (`pool = 2`; `soft` with `max_attempts = 2`), each a
/// lone drain step into runs of `work`.
fn files(config: &str) -> Vec<(String, String)> {
    let flow = |name: &str, keys: &str| {
        let text = format!(
            "name = \"{name}\"\n[inputs]\nissue = {{ type = \"task\", required = true }}\n\
             [[step]]\nname = \"all\"\nkind = \"drain\"\nformula = \"work\"\n{keys}"
        );
        (format!("shop/formulas/{name}.toml"), text)
    };
    let mut files: Vec<(String, String)> = SWEEP.iter().map(|(p, t)| ((*p).into(), (*t).into())).collect();
    files[0].1 = format!("{}{config}", files[0].1);
    files.extend([
        flow("one", ""),
        flow("two", "pool = 2\n"),
        flow("soft", "pool = 2\non_item_failure = \"soft_stop\"\n"),
        flow("hard", "pool = 2\non_item_failure = \"hard_stop\"\n"),
        flow("again", "pool = 2\non_item_failure = \"soft_stop\"\nmax_attempts = 2\n"),
    ]);
    files
}

/// A world with the formulas of [`files`], five ready issues under a root, and `formula` started on the root.
async fn start(formula: &str, config: &str, extra: &[(&str, &str)]) -> (World, String, Vec<String>) {
    let files = files(config);
    let mut all: Vec<(&str, &str)> = files.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    all.extend_from_slice(extra);
    let w = world(&all);
    let root = w.tracker.add(&[], None);
    let issues: Vec<String> = (0..5).map(|_| w.tracker.add_in("w", &[], Some(&root))).collect();
    for _ in 0..8 {
        w.driver.script(slow_turn());
    }
    let reply = w.start_issue(formula, Some("web"), json!(root)).await.unwrap();
    let run = reply["run"].as_str().unwrap().to_owned();
    (w, run, issues)
}

fn states(w: &World, run: &str) -> Vec<String> {
    w.kids(run).into_iter().map(|k| k.2).collect()
}

/// Waits for `run` to fail and returns the data of its `run.failed`.
async fn failed(w: &World, run: &str) -> Value {
    for _ in 0..60 {
        if !w.all("run.failed").iter().all(|e| e["run"]["id"] != run) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    w.until(Some(run), "run.failed", 0).await
}

/// Ends each child of `run` once its session has started — `fail` on the issues in `failing` — until `run` passed;
/// returns the data of its `run.passed`.
async fn passed(w: &World, run: &str, failing: &[&str], done: &mut BTreeSet<String>) -> Value {
    for _ in 0..120 {
        if !w.all("run.passed").iter().all(|e| e["run"]["id"] != run) {
            break;
        }
        for (id, issue, ..) in w.kids(run) {
            if !done.contains(&id) && w.driver.token(&id, 1).is_some() {
                let verdict = if failing.contains(&issue.as_str()) {
                    Verdict::Fail
                } else {
                    Verdict::Pass
                };
                w.end_attempt(&id, 1, verdict).await;
                done.insert(id);
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    w.until(Some(run), "run.passed", 0).await
}

#[tokio::test(start_paused = true)]
async fn without_pool_one_issue_is_in_work_at_a_time() {
    let (w, run, issues) = start("one", "", &[]).await;
    let mut done = BTreeSet::new();
    w.settle(&run, &[], &[issues[0].as_str()], &mut done).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(states(&w, &run), ["running"]);
    let outcome = &passed(&w, &run, &[], &mut done).await["outcome"];
    assert_eq!(
        outcome["data"],
        json!({ "closed": issues, "failed": [], "refused": {} })
    );
}

#[tokio::test(start_paused = true)]
async fn pool_two_keeps_two_of_five_in_work_and_a_failed_issue_does_not_stop_the_rest() {
    let (w, run, issues) = start("two", "", &[]).await;
    let mut done = BTreeSet::new();
    w.settle(&run, &[], &[issues[0].as_str()], &mut done).await;
    // Another read of the tracker: the second holds its place of the pool queued (`max_runs = 1` runs one at a time).
    w.changed().await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(states(&w, &run), ["running", "queued"]);
    let outcome = &passed(&w, &run, &[issues[0].as_str()], &mut done).await["outcome"];
    assert_eq!(
        outcome["data"],
        json!({ "closed": issues[1..], "failed": [issues[0]], "refused": {} })
    );
    let given: Vec<String> = w.kids(&run).into_iter().map(|k| k.1).collect();
    assert_eq!(given, issues, "in the order they are ready");
}

#[tokio::test(start_paused = true)]
async fn soft_stop_gives_no_new_issue_and_lets_the_started_end() {
    let (w, run, issues) = start("soft", "", &[]).await;
    let mut done = BTreeSet::new();
    w.settle(&run, &[issues[0].as_str()], &[issues[1].as_str()], &mut done)
        .await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(states(&w, &run), ["failed", "running"]);
    assert!(w.all("run.failed").iter().all(|e| e["run"]["id"] != run.as_str()));
    let second = w.kids(&run)[1].0.clone();
    w.end_attempt(&second, 1, Verdict::Pass).await;
    let failed = failed(&w, &run).await;
    assert_eq!((failed["class"].as_str(), states(&w, &run).len()), (Some("agent"), 2));
    let reason = failed["reason"].as_str().unwrap();
    assert!(reason.starts_with(&format!("stopped on {}", issues[0])), "{reason}");
    assert_eq!(
        failed["outcome"]["data"],
        json!({ "closed": [issues[1]], "failed": [issues[0]], "refused": {},
            "stopped_on": [issues[0]], "not_started": issues[2..] })
    );
}

#[tokio::test(start_paused = true)]
async fn hard_stop_cancels_the_started_issues() {
    let (w, run, issues) = start("hard", "", &[]).await;
    let (first, second) = (issues[0].as_str(), issues[1].as_str());
    // The first fails as soon as it runs; the second never ends on its own.
    for _ in 0..60 {
        let kids = w.kids(&run);
        if let Some(kid) = kids.iter().find(|k| k.1 == first && k.2 == "running") {
            w.end_attempt(&kid.0, 1, Verdict::Fail).await;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let failed = failed(&w, &run).await;
    assert_eq!(states(&w, &run), ["failed", "cancelled"]);
    let cancelled = w.until(Some(&w.kids(&run)[1].0), "run.cancelled", 0).await;
    assert_eq!(cancelled["reason"], "drain_hard_stop");
    assert_eq!(
        failed["outcome"]["data"],
        json!({ "closed": [], "failed": [first, second], "refused": {},
            "stopped_on": [first], "not_started": issues[2..] })
    );
}

#[tokio::test(start_paused = true)]
async fn a_restart_mid_stop_gives_no_new_issue() {
    let config = "[settings]\naddons = [{ name = \"policy\" }]\n[[addon]]\npath = \"policy\"\n";
    let (mut w, run, issues) = start("soft", config, &[POLICY[1]]).await;
    let mut done = BTreeSet::new();
    w.settle(&run, &[issues[0].as_str()], &[issues[1].as_str()], &mut done)
        .await;
    w.restart().await;
    for _ in 0..8 {
        w.driver.script(slow_turn());
    }
    let kid = w.kids(&run)[1].0.clone();
    let fail = Decision::Fail {
        reason: "lost".into(),
        gate: None,
    };
    w.decide(&kid, "run.attempt_failed", 0, fail).await;
    let resume = Decision::Resume {
        after: Duration::ZERO,
        gate: None,
    };
    w.decide(&run, "run.attempt_failed", 0, resume).await;
    let failed = failed(&w, &run).await;
    assert_eq!(states(&w, &run), ["failed", "failed"]);
    assert_eq!(failed["outcome"]["data"]["not_started"], json!(issues[2..]));
}

#[tokio::test(start_paused = true)]
async fn a_repeat_after_a_stop_is_a_new_entry() {
    let (w, run, issues) = start("again", "", &[]).await;
    let mut done = BTreeSet::new();
    w.settle(&run, &[issues[0].as_str()], &[issues[1].as_str()], &mut done)
        .await;
    // The entry is told apart by milliseconds: the repeat must not start in the millisecond of the first children.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let second = w.kids(&run)[1].0.clone();
    w.end_attempt(&second, 1, Verdict::Pass).await;
    done.insert(second);
    // `max_attempts = 2`: the step runs again and gives every issue not closed, the failed one too.
    passed(&w, &run, &[], &mut done).await;
    let given: Vec<String> = w.kids(&run).into_iter().map(|k| k.1).collect();
    assert_eq!(given[2..], [issues[0].as_str(), &issues[2], &issues[3], &issues[4]]);
}
