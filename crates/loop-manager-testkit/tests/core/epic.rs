//! A long epic (docs/design/config.md, decision 18): each issue is blocked by the one before it, a trigger on
//! `issue.ready` takes the next, and the chain runs to the end however long it is. Exec steps, so real time.

use crate::common;

use loop_manager_core::{
    BoxFuture, Clock, Journal, config, open,
    run::{self, ExecHooks, Msg},
    tracker::{Change, Issue, Lineage, NewIssue, Tracker, TrackerError, Wait},
};
use loop_manager_testkit::MemoryTracker;
use serde_json::Value;
use std::{fs, sync::Arc, time::Duration};

const ISSUES: usize = 15;

/// The memory tracker with a chain of blockers: `b-<n>` is ready only once `b-<n-1>` is closed.
#[derive(Clone)]
struct Chain(MemoryTracker);

impl Tracker for Chain {
    fn claim<'a>(&'a self, owner: &'a str, id: &'a str) -> BoxFuture<'a, Result<Issue, TrackerError>> {
        self.0.claim(owner, id)
    }
    fn release<'a>(&'a self, owner: &'a str, id: &'a str) -> BoxFuture<'a, Result<(), TrackerError>> {
        self.0.release(owner, id)
    }
    fn close<'a>(&'a self, owner: &'a str, id: &'a str, reason: &'a str) -> BoxFuture<'a, Result<(), TrackerError>> {
        self.0.close(owner, id, reason)
    }
    fn close_by<'a>(&'a self, actor: &'a str, id: &'a str, reason: &'a str) -> BoxFuture<'a, Result<(), TrackerError>> {
        self.0.close_by(actor, id, reason)
    }
    fn ready(&self) -> BoxFuture<'_, Result<Vec<Issue>, TrackerError>> {
        Box::pin(async move {
            let unblocked = |b: &Issue| {
                let n: usize = b.id.trim_start_matches("b-").parse().unwrap();
                n == 1
                    || self
                        .0
                        .issue(&format!("b-{}", n - 1))
                        .is_some_and(|p| p.status == "closed")
            };
            Ok(self.0.ready().await?.into_iter().filter(unblocked).collect())
        })
    }
    fn under<'a>(&'a self, root: &'a str) -> BoxFuture<'a, Result<Vec<String>, TrackerError>> {
        self.0.under(root)
    }
    fn subtree<'a>(&'a self, root: &'a str) -> BoxFuture<'a, Result<Vec<Issue>, TrackerError>> {
        self.0.subtree(root)
    }
    fn waiting<'a>(&'a self, root: &'a str) -> BoxFuture<'a, Result<Vec<Wait>, TrackerError>> {
        self.0.waiting(root)
    }
    fn changes(&self, after: i64) -> BoxFuture<'_, Result<Vec<Change>, TrackerError>> {
        self.0.changes(after)
    }
    fn create<'a>(
        &'a self,
        actor: &'a str,
        key: &'a str,
        title: &'a str,
        text: &'a str,
    ) -> BoxFuture<'a, Result<Issue, TrackerError>> {
        self.0.create(actor, key, title, text)
    }
    fn file<'a>(
        &'a self,
        actor: &'a str,
        key: Option<&'a str>,
        new: &'a NewIssue,
    ) -> BoxFuture<'a, Result<Issue, TrackerError>> {
        self.0.file(actor, key, new)
    }
    fn lineage<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Lineage, TrackerError>> {
        self.0.lineage(id)
    }

    fn attempt_ended<'a>(
        &'a self,
        actor: &'a str,
        id: &'a str,
        entry: &'a serde_json::Value,
    ) -> BoxFuture<'a, Result<(), TrackerError>> {
        self.0.attempt_ended(actor, id, entry)
    }

    fn comment<'a>(&'a self, actor: &'a str, id: &'a str, text: &'a str) -> BoxFuture<'a, Result<(), TrackerError>> {
        Tracker::comment(&self.0, actor, id, text)
    }
}

#[tokio::test]
async fn an_epic_of_fifteen_issues_each_blocked_by_the_last_runs_to_its_end() {
    let dir = tempfile::tempdir().unwrap();
    let take = "[rigs.web]\npath = \"web\"\n[[trigger]]\nname = \"take\"\non = \"issue.ready\"\nrun = \"close\"\n\
                rig = \"web\"\ninputs = { issue = \"{{ event.issue.id }}\" }\n";
    let close = "name = \"close\"\n[inputs]\nissue = { type = \"task\", required = true }\n\
                 [[step]]\nkind = \"exec\"\ncommand = [\"true\"]\ntimeout = \"5s\"\n";
    let files = common::FILES
        .iter()
        .copied()
        .filter(|(p, _)| *p != "shop/loop-manager.toml");
    for (path, text) in files.chain([("shop/loop-manager.toml", take), ("shop/formulas/close.toml", close)]) {
        let path = dir.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let tracker = Chain(MemoryTracker::new());
    for _ in 0..ISSUES {
        tracker.0.add(&[], None);
    }
    let mut config = config::load(&dir.path().join("config.toml")).unwrap();
    for p in &mut config.projects {
        p.tracker.present = true;
    }
    let journal = dir.path().join("journal");
    let (core, _task) = run::start(
        open(&dir.path().join("state.db")).unwrap(),
        Clock::managed(0),
        Arc::new(config),
        Vec::new(),
        [("shop".to_owned(), Arc::new(tracker.clone()) as Arc<dyn Tracker>)].into(),
        Arc::new(ExecHooks::default()),
        Journal::new(&journal),
    );

    let of = |all: &[Value], kind: &str| all.iter().filter(|e| e["type"] == kind).cloned().collect::<Vec<_>>();
    let started = std::time::Instant::now();
    let all = loop {
        // No watcher on a tracker in memory: the test says it changed.
        core.send(None, Msg::TrackerChanged("shop".into())).await.unwrap();
        let all = Journal::new(&journal).read_from(1, 100_000).unwrap_or_default();
        let over = of(&all, "run.passed").len() == ISSUES || !of(&all, "trigger.skipped").is_empty();
        if over || started.elapsed() > Duration::from_secs(30) {
            break all;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let skipped = of(&all, "trigger.skipped");
    assert!(skipped.is_empty(), "{skipped:?}");
    assert_eq!(of(&all, "run.passed").len(), ISSUES);
    assert!((1..=ISSUES).all(|n| tracker.0.issue(&format!("b-{n}")).unwrap().status == "closed"));
    // One chain: every issue after the first was freed by the run before it.
    let queued = of(&all, "run.queued");
    assert!(queued.iter().all(|e| e["correlation"] == queued[0]["correlation"]));
    assert!(queued.iter().all(|e| e["data"]["run"]["depth"] == 1), "{queued:?}");
}
