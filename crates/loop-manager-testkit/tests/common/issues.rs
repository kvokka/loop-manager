//! Helpers of the issue tests: a formula on one issue, its runs and the issue events in the journal.

use super::{World, hollow_turn};
use loop_manager_core::run::{Create, Msg, Outcome, Verdict};
use loop_manager_testkit::Step;
use serde_json::{Value, json};
use std::time::Duration;

pub(crate) const FIX: &[(&str, &str)] = &[
    ("shop/agents/fixer/agent.toml", "harness = \"scripted\"\n"),
    ("shop/agents/fixer/prompt.template.md", "Fix {{ issue.title }}."),
    (
        "shop/formulas/fix.toml",
        "name = \"fix\"\n[inputs]\nissue = { type = \"task\", required = true }\n\
         [[step]]\nkind = \"ai\"\nagent = \"fixer\"\n",
    ),
];

impl World {
    pub(crate) async fn fix(&self, issue: Value) -> Result<Value, loop_manager_core::Error> {
        self.fix_on(Some("web"), issue).await
    }

    /// A start of `fix` on `issue` on `rig`; `None` names no rig.
    pub(crate) async fn fix_on(&self, rig: Option<&str>, issue: Value) -> Result<Value, loop_manager_core::Error> {
        self.start_issue("fix", rig, issue).await
    }

    /// A start of `formula` with the input `issue` on `rig`.
    pub(crate) async fn start_issue(
        &self,
        formula: &str,
        rig: Option<&str>,
        issue: Value,
    ) -> Result<Value, loop_manager_core::Error> {
        let create = Create {
            project: "shop".into(),
            rig: rig.map(Into::into),
            formula: formula.into(),
            inputs: [("issue".to_owned(), issue)].into_iter().collect(),
            correlation: None,
            depth: 0,
            parent_run: None,
            token: None,
            continue_from: None,
        };
        self.core.send(None, Msg::Create(create)).await
    }

    /// A run of `fix` on `issue`, by its id.
    pub(crate) async fn run_on(&self, issue: Value) -> String {
        let reply = self.fix(issue).await.unwrap();
        reply["run"].as_str().unwrap().to_owned()
    }

    pub(crate) fn assignee(&self, issue: &str) -> Option<String> {
        self.tracker.issue(issue).unwrap().assignee
    }

    pub(crate) async fn finish_issue(&self, run: &str, result: Verdict, summary: &str) {
        self.finish_attempt(run, 1, result, summary).await;
    }

    pub(crate) async fn finish_attempt(&self, run: &str, attempt: u32, result: Verdict, summary: &str) {
        let token = self.driver.token(run, attempt).unwrap();
        let outcome = Outcome {
            result,
            summary: summary.into(),
            data: None,
        };
        let msg = Msg::Finish {
            run: run.into(),
            token,
            outcome,
        };
        self.core.send(None, msg).await.unwrap();
    }

    /// The tracker says it changed; the core reads its ready set again.
    pub(crate) async fn changed(&self) {
        let msg = Msg::TrackerChanged("shop".into());
        self.core.send(None, msg).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub(crate) fn next_ready() -> Value {
    json!({ "next_ready": true })
}

/// A turn that ends 10 s after the prompt: time for the finisher.
pub(crate) fn slow_turn() -> Vec<Step> {
    let mut steps = hollow_turn();
    steps.insert(1, Step::Wait(Duration::from_secs(10)));
    steps
}
