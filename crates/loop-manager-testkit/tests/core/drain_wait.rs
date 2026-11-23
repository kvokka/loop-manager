//! What a `drain` step gives and what it waits for (docs/design/config.md "Formula", decision 159): a run of a rig
//! gives only the issues of its rig under its root, a run of the root those of every rig; the step does not end while
//! an issue of its own waits on a foreign blocker or a `blocked` status set by hand, up to the step's `timeout`.

use crate::common;

use common::{World, drain::SWEEP, issues::slow_turn, world};
use loop_manager_core::run::Verdict;
use serde_json::{Value, json};
use std::{collections::BTreeSet, time::Duration};

impl World {
    /// Starts `sweep` on `root` on rig `rig`; returns its run.
    async fn sweep_on(&self, root: &str, rig: &str) -> String {
        for _ in 0..8 {
            self.driver.script(slow_turn());
        }
        let reply = self.start_issue("sweep", Some(rig), json!(root)).await.unwrap();
        reply["run"].as_str().unwrap().to_owned()
    }

    /// The outcome of the first end of a step of `run`, if it ended.
    fn step_end(&self, run: &str) -> Option<Value> {
        let all = self.all("run.step_finished");
        all.into_iter()
            .find(|d| d["run"]["id"] == run)
            .map(|d| d["outcome"].clone())
    }
}

/// An issue of another rig under the root is foreign to a run of a rig: not given, and not refused either.
#[tokio::test(start_paused = true)]
async fn a_rig_run_gives_only_the_issues_of_its_rig() {
    let w = world(SWEEP);
    let root = w.tracker.add(&[], None);
    let mine = w.tracker.add_in("w", &[], Some(&root));
    let other = w.tracker.add_in("a", &[], Some(&root));
    let run = w.sweep_on(&root, "web").await;
    let outcome = w.settle(&run, &[], &[], &mut BTreeSet::new()).await;
    assert_eq!(outcome["result"], "pass", "{outcome}");
    assert_eq!(outcome["data"]["closed"], json!([mine]));
    assert_eq!(outcome["data"]["refused"], json!({}));
    assert!(w.kids(&run).iter().all(|k| k.1 != other));
    assert_eq!(w.tracker.issue(&other).unwrap().status, "open");
}

/// An issue of its own blocked by a foreign issue holds the step; the foreign issue closed by someone else wakes it, and
/// the issue is given.
#[tokio::test(start_paused = true)]
async fn an_issue_blocked_by_a_foreign_issue_holds_the_step_until_it_closes() {
    let w = world(SWEEP);
    let root = w.tracker.add(&[], None);
    let mine = w.tracker.add_in("w", &[], Some(&root));
    let foreign = w.tracker.add_in("a", &[], Some(&root));
    w.tracker.depend(&mine, &foreign, "blocks");
    let run = w.sweep_on(&root, "web").await;
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(w.step_end(&run), None, "the step waits");
    assert!(w.kids(&run).is_empty());
    w.tracker.set_status(&foreign, "closed");
    w.changed().await;
    let outcome = w.settle(&run, &[], &[], &mut BTreeSet::new()).await;
    assert_eq!(outcome["data"]["closed"], json!([mine]), "{outcome}");
}

/// A blocker of its own that failed does not hold the step: the issue behind it is left as before.
#[tokio::test(start_paused = true)]
async fn a_failed_issue_of_its_own_does_not_hold_the_step() {
    let w = world(SWEEP);
    let root = w.tracker.add(&[], None);
    let first = w.tracker.add_in("w", &[], Some(&root));
    let then = w.tracker.add_in("w", &[], Some(&root));
    w.tracker.depend(&then, &first, "blocks");
    let run = w.sweep_on(&root, "web").await;
    let outcome = w.settle(&run, &[first.as_str()], &[], &mut BTreeSet::new()).await;
    assert_eq!(outcome["data"]["failed"], json!([first]), "{outcome}");
    assert_eq!(outcome["data"]["closed"], json!([]));
}

/// A `blocked` status set by hand on an issue of its own is a lock: the step waits until it is lifted.
#[tokio::test(start_paused = true)]
async fn an_issue_blocked_by_hand_holds_the_step() {
    let w = world(SWEEP);
    let root = w.tracker.add(&[], None);
    let locked = w.tracker.add_in("w", &[], Some(&root));
    w.tracker.set_status(&locked, "blocked");
    let run = w.sweep_on(&root, "web").await;
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(w.step_end(&run), None, "the step waits");
    w.tracker.set_status(&locked, "open");
    w.changed().await;
    let outcome = w.settle(&run, &[], &[], &mut BTreeSet::new()).await;
    assert_eq!(outcome["data"]["closed"], json!([locked]), "{outcome}");
}

/// A run of the root gives the issues of every rig under it, a blocker under the root among them, and waits only for
/// a blocker outside its root.
#[tokio::test(start_paused = true)]
async fn a_root_run_gives_every_rig_and_waits_only_for_issues_outside_its_root() {
    let w = world(SWEEP);
    let root = w.tracker.add(&[], None);
    let api = w.tracker.add_in("a", &[], Some(&root));
    let web = w.tracker.add_in("w", &[], Some(&root));
    w.tracker.depend(&web, &api, "blocks");
    let outside = w.tracker.add_in("a", &[], None);
    let late = w.tracker.add_in("w", &[], Some(&root));
    w.tracker.depend(&late, &outside, "waits-for");
    let run = w.sweep(&root).await;
    let mut done = BTreeSet::new();
    for _ in 0..120 {
        for (id, ..) in w.kids(&run) {
            if !done.contains(&id) && w.driver.token(&id, 1).is_some() {
                w.end_attempt(&id, 1, Verdict::Pass).await;
                done.insert(id);
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(w.step_end(&run), None, "the step waits for the issue outside the root");
    let kids: Vec<String> = w.kids(&run).into_iter().map(|k| k.1).collect();
    assert_eq!(kids, [api.clone(), web.clone()]);
    w.tracker.set_status(&outside, "closed");
    w.changed().await;
    let outcome = w.settle(&run, &[], &[], &mut done).await;
    assert_eq!(outcome["data"]["closed"], json!([api, web, late]), "{outcome}");
}

/// The step's `timeout` ends the wait: the attempt fails as a runaway turn.
#[tokio::test(start_paused = true)]
async fn the_step_timeout_ends_the_wait() {
    let mut files = SWEEP.to_vec();
    let sweep = files.iter_mut().find(|f| f.0 == "shop/formulas/sweep.toml").unwrap();
    sweep.1 = "name = \"sweep\"\n[inputs]\nissue = { type = \"task\", required = true }\n\
        [[step]]\nname = \"all\"\nkind = \"drain\"\nformula = \"work\"\ntimeout = \"10m\"\n";
    let w = world(&files);
    let root = w.tracker.add(&[], None);
    let mine = w.tracker.add_in("w", &[], Some(&root));
    w.tracker.depend(&mine, &w.tracker.add_in("a", &[], None), "blocks");
    let run = w.sweep_on(&root, "web").await;
    tokio::time::sleep(Duration::from_secs(9 * 60)).await;
    assert!(w.all("run.attempt_failed").iter().all(|d| d["run"]["id"] != run));
    tokio::time::sleep(Duration::from_secs(2 * 60)).await;
    let failed = w.all("run.attempt_failed");
    let failed = failed.iter().find(|d| d["run"]["id"] == run).expect("the wait ended");
    assert_eq!(failed["failure"]["kind"], "runaway_turn", "{failed}");
}

/// An issue of its own this entry already gave, which failed and now waits on a foreign blocker, does not hold the
/// step: it ends with the issue in `failed`, not at the step's `timeout`.
#[tokio::test(start_paused = true)]
async fn a_given_issue_that_failed_and_waits_on_a_foreign_blocker_does_not_hold_the_step() {
    let mut files = SWEEP.to_vec();
    let sweep = files.iter_mut().find(|f| f.0 == "shop/formulas/sweep.toml").unwrap();
    sweep.1 = "name = \"sweep\"\n[inputs]\nissue = { type = \"task\", required = true }\n\
        [[step]]\nname = \"all\"\nkind = \"drain\"\nformula = \"work\"\ntimeout = \"10m\"\n\
        [[step]]\nname = \"wrap\"\nkind = \"ai\"\nagent = \"scribe\"\n";
    let w = world(&files);
    let root = w.tracker.add(&[], None);
    let mine = w.tracker.add_in("w", &[], Some(&root));
    let foreign = w.tracker.add_in("a", &[], Some(&root));
    let run = w.sweep_on(&root, "web").await;
    // The child works on the issue when the foreign blocker comes.
    let stop = [mine.as_str()];
    assert_eq!(w.settle(&run, &[], &stop, &mut BTreeSet::new()).await, Value::Null);
    w.tracker.depend(&mine, &foreign, "blocks");
    let outcome = w.settle(&run, &stop, &[], &mut BTreeSet::new()).await;
    assert_eq!(outcome["data"]["failed"], json!([mine]), "{outcome}");
    assert_eq!(outcome["data"]["closed"], json!([]));
    assert!(w.all("run.attempt_failed").iter().all(|d| d["run"]["id"] != run));
}
