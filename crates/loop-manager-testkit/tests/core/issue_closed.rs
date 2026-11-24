//! A run whose issue is closed by another actor ends `cancelled` with reason `issue_closed`, at whatever step
//! (docs/design/contracts.md 6.2); its release leaves the issue closed, so no trigger takes it again.

use crate::common;

use common::{World, world};
use loop_manager_core::Journal;
use serde_json::{Value, json};
use std::time::Duration;

/// `web` lets one writer work at once, and every ready issue starts `hold` on it, as devloop's `epic-ready` does.
const FILES: &[(&str, &str)] = &[
    (
        "shop/loop-manager.toml",
        "[rigs.web]\npath = \"web\"\nmax_writers = 1\n\n[[trigger]]\nname = \"epic-ready\"\non = \"issue.ready\"\n\
         run = \"hold\"\nrig = \"web\"\ninputs = { issue = \"{{ event.issue.id }}\" }\n",
    ),
    // A writer by its exec step, which waits for the owner first.
    (
        "shop/formulas/hold.toml",
        "name = \"hold\"\n[inputs]\nissue = { type = \"task\", required = true }\n\
         [[step]]\nname = \"code_owner\"\nkind = \"await\"\n\
         [[step]]\nname = \"ci\"\nkind = \"exec\"\ncommand = [\"true\"]\n",
    ),
];

impl World {
    /// The events `kind` of the journal, whole.
    fn journal_of(&self, kind: &str) -> Vec<Value> {
        let all = Journal::new(&self.journal).read_from(1, 100_000).unwrap();
        all.into_iter().filter(|e| e["type"] == kind).collect()
    }

    /// The run that started on `issue`.
    fn run_of(&self, issue: &str) -> String {
        let queued = self.journal_of("run.queued");
        let on = queued.iter().find(|e| e["data"]["run"]["issue"] == issue);
        on.unwrap_or_else(|| panic!("no run on {issue}: {queued:?}"))["run"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

#[tokio::test(start_paused = true)]
async fn an_issue_closed_by_another_actor_cancels_its_waiting_run_and_frees_its_writer_slot() {
    let w = world(FILES);
    let (first, second) = (w.tracker.add(&["auto"], None), w.tracker.add(&["auto"], None));
    w.changed().await;
    let held = w.run_of(&first);
    w.until(Some(&held), "run.awaiting", 0).await;
    let queued = w.run_of(&second);
    let waits = w.journal_of("run.queued");
    let last = waits.iter().rfind(|e| e["run"] == queued.as_str()).unwrap();
    assert_eq!(last["data"]["waiting_for"], "writers", "{last:?}");

    // The run's own attempt closing its issue is its own close, not this case.
    w.tracker.close_as(&first, &format!("loop-manager:{held}:1"));
    w.changed().await;
    assert!(
        w.journal_of("run.cancelled").is_empty(),
        "{:?}",
        w.journal_of("run.cancelled")
    );
    w.tracker.set_status(&first, "open");
    w.changed().await;

    // The owner closes the epic by hand while its run waits on the await step.
    w.tracker.close_as(&first, "human");
    w.changed().await;
    let cancelled = w.until(Some(&held), "run.cancelled", 0).await;
    assert_eq!(cancelled["reason"], "issue_closed", "{cancelled:?}");
    w.until(Some(&queued), "run.awaiting", 0).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    w.changed().await;
    let issue = w.tracker.issue(&first).unwrap();
    assert_eq!((issue.status.as_str(), issue.assignee), ("closed", None));
    let fired: Vec<Value> = (w.journal_of("trigger.fired").into_iter())
        .filter(|e| e["data"]["inputs"]["issue"] == json!(first))
        .collect();
    assert_eq!(fired.len(), 1, "no trigger takes the closed issue again: {fired:?}");
}

#[tokio::test(start_paused = true)]
async fn an_issue_whose_status_another_actor_sets_to_closed_cancels_its_waiting_run() {
    let w = world(FILES);
    let issue = w.tracker.add(&["auto"], None);
    w.changed().await;
    let held = w.run_of(&issue);
    w.until(Some(&held), "run.awaiting", 0).await;

    // `br update -s closed`: an update of the status, not a close.
    w.tracker.set_status(&issue, "closed");
    w.changed().await;
    let cancelled = w.until(Some(&held), "run.cancelled", 0).await;
    assert_eq!(cancelled["reason"], "issue_closed", "{cancelled:?}");
}

#[tokio::test(start_paused = true)]
async fn the_runs_own_close_read_after_a_status_write_of_another_actor_leaves_it_running() {
    let w = world(FILES);
    let issue = w.tracker.add(&["auto"], None);
    w.changed().await;
    let held = w.run_of(&issue);
    w.until(Some(&held), "run.awaiting", 0).await;

    // One read takes both writes, each with the issue as read: closed.
    w.tracker.set_status(&issue, "in_progress");
    w.tracker.close_as(&issue, &format!("loop-manager:{held}:1"));
    w.changed().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        w.journal_of("run.cancelled").is_empty(),
        "{:?}",
        w.journal_of("run.cancelled")
    );
}

#[tokio::test(start_paused = true)]
async fn a_write_of_another_actor_without_status_read_after_the_runs_own_close_leaves_it_running() {
    let w = world(FILES);
    let issue = w.tracker.add(&["auto"], None);
    w.changed().await;
    let held = w.run_of(&issue);
    w.until(Some(&held), "run.awaiting", 0).await;

    // One read takes both writes, each with the issue as read: closed. The label does not tell who closed it.
    w.tracker.close_as(&issue, &format!("loop-manager:{held}:1"));
    w.tracker.label(&issue, "x");
    w.changed().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        w.journal_of("run.cancelled").is_empty(),
        "{:?}",
        w.journal_of("run.cancelled")
    );
}
