//! Criterion 38, through the CLI of a real daemon on the `acp-agent` fixture and an exec step, no model. An exec step
//! stopped by the operator gets the outcome `fail`, which waits for `run continue`; a command of another attempt is
//! `stale_attempt` and changes nothing. An `ai` step taken over keeps its turn going; an instruction said twice under
//! one key is queued once, and Continue refuses to hand the run back while it waits; it reaches the agent at the turn
//! end, the agent declares a pass in manual mode, and the run moves only after Continue: it passes once, in the same
//! attempt, the instruction echoed once. The rest of the matrix lives in cargo (the check of row 38).

use super::{exec_formula, fake_agent};
use crate::{Env, daemon::Daemon};
use serde_json::json;
use std::{fs, sync::Arc, time::Duration};

const WITHIN: Duration = Duration::from_secs(60);

/// `loop-manager run <args> --attempt <attempt>` on `d`: its answer, asserted to be `want`.
fn run_cli(d: &Daemon, args: &[&str], attempt: u64, want: &str) {
    let attempt = attempt.to_string();
    let (ok, answer) = d.cli_json(&[&["run"], args, &["--attempt", &attempt]].concat());
    assert_eq!(answer["answer"], want, "run {args:?}: {answer}");
    assert_eq!(ok, want == "accepted", "run {args:?}: {answer}");
}

/// The records of the history of `run` so far, as text.
fn history(d: &Daemon, run: &str) -> String {
    fs::read_to_string(d.path(&format!("state/loop-manager/history/{run}.jsonl"))).unwrap_or_default()
}

async fn exec_stop(d: &Daemon) {
    let run = d.create("slow").await;
    let deadline = tokio::time::Instant::now() + WITHIN;
    // The command runs once its output comes: a Stop before its start would find nothing to stop.
    while !history(d, &run).contains("exec_output") {
        assert!(tokio::time::Instant::now() < deadline, "the command did not start");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    run_cli(d, &["stop", &run], 1, "accepted");
    let view = d
        .until_view(&run, "no outcome", WITHIN, |v| v["manual"]["deferred"] == "outcome")
        .await;
    assert_eq!(
        (&view["state"], &view["outcome"]["result"], &view["outcome"]["summary"]),
        (&json!("running"), &json!("fail"), &json!("stopped by the operator")),
        "{view}"
    );
    run_cli(d, &["continue", &run], 2, "stale_attempt");
    assert_eq!(d.get(&format!("/v1/runs/{run}")).await["control"], "manual");
    run_cli(d, &["continue", &run], 1, "accepted");
    let view = d.until_state(&run, "failed", WITHIN).await;
    assert_eq!(view["attempts"].as_array().map(Vec::len), Some(1), "{view}");
}

async fn ai_manual(d: &Daemon, go: &std::path::Path) {
    let run = d.create("build").await;
    d.until_view(&run, "no session", WITHIN, |v| v["attempts"][0]["session"].is_object())
        .await;
    for _ in 0..2 {
        run_cli(d, &["takeover", &run, "--key", "t1"], 1, "accepted");
    }
    run_cli(d, &["say", &run, "check the docs"], 2, "stale_attempt");
    for _ in 0..2 {
        run_cli(d, &["say", &run, "check the docs", "--key", "s1"], 1, "accepted");
    }
    let view = d.get(&format!("/v1/runs/{run}")).await;
    let unsent = view["manual"]["unsent"].as_array().cloned().unwrap_or_default();
    assert_eq!(unsent.len(), 1, "{view}");
    assert_eq!(unsent[0]["state"], "queued", "{view}");
    run_cli(d, &["continue", &run], 1, "unsent_input");

    // The first turn ends without an outcome; the instruction goes at its end, and its turn declares a pass.
    fs::write(go, "").unwrap();
    let view = d
        .until_view(&run, "no outcome settled", WITHIN, |v| {
            v["manual"]["deferred"] == "settled" && v["outcome"]["result"] == "pass"
        })
        .await;
    assert_eq!(
        (&view["state"], &view["control"]),
        (&json!("running"), &json!("manual")),
        "{view}"
    );
    run_cli(d, &["continue", &run], 1, "accepted");
    let view = d.until_state(&run, "passed", WITHIN).await;
    assert_eq!(view["attempt"], 1, "{view}");
    let seen = d
        .journal_until(WITHIN, |seen| {
            seen.iter().any(|(k, e)| k == "run.passed" && e["run"] == run.as_str())
        })
        .await;
    let of_run = |kind: &str| {
        (seen.iter())
            .filter(|(k, e)| k == kind && e["run"] == run.as_str())
            .count()
    };
    assert_eq!(
        (of_run("run.taken_over"), of_run("run.finishing"), of_run("run.passed")),
        (1, 1, 1),
        "{seen:?}"
    );
    assert_eq!(history(d, &run).matches("Echo: check the docs").count(), 1);
}

pub(crate) async fn run(env: Arc<Env>) -> String {
    let gate = tempfile::tempdir().unwrap();
    let (go, seen) = (gate.path().join("go"), gate.path().join("seen"));
    let command = format!(
        "while [ ! -e {go} ]; do sleep 0.05; done; if [ -e {seen} ]; then {lm} finish pass --summary built; fi; \
         touch {seen}",
        go = go.display(),
        seen = seen.display(),
        lm = env.loop_manager.display()
    );
    let d = Daemon::start(&env, |root| {
        fake_agent(&env, root, "", "", &command);
        exec_formula(root, "slow", "", "\"sh\", \"-c\", \"echo up; exec sleep 600\"");
    })
    .await;
    tokio::join!(exec_stop(&d), ai_manual(&d, &go));
    d.until_no_steps(Duration::from_secs(15)).await;
    String::new()
}
