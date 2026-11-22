//! Criterion 13: an ACP harness runs an adapter process per session, so three codex runs (two of one provider, one
//! of another) get three instances; none starts before a run is queued for it, and each stops as `idle` right after
//! its run, with no idle wait.

use super::codex_agent;
use crate::{Env, daemon::Daemon};
use loop_manager_core::parse_rfc3339_ms;
use loop_manager_testkit::openai::{Provider, Reply};
use serde_json::Value;
use std::{collections::HashSet, sync::Arc, time::Duration};

const WITHIN: Duration = Duration::from_secs(60);
/// How soon after its run an instance is stopped: the stop grace (10 s) and a margin, well under any idle wait.
const STOP: Duration = Duration::from_secs(15);

pub(crate) async fn run(env: Arc<Env>) -> String {
    let finish = format!("{} finish pass --summary built", env.loop_manager.display());
    let shared = Provider::start(vec![Reply::Task(finish.clone())]).await;
    let other = Provider::start(vec![Reply::Task(finish)]).await;
    let d = Daemon::start(&env, |root| {
        codex_agent(root, "dev", &shared.base_url());
        codex_agent(root, "ops", &shared.base_url());
        codex_agent(root, "qa", &other.base_url());
    })
    .await;
    let of_codex = |kind: &str, (k, e): &(String, Value)| k == kind && e["data"]["instance"]["harness"] == "codex";
    // Lazy: the daemon idles a while first, and no instance starts before a run needs one.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let runs = [d.create("dev").await, d.create("ops").await, d.create("qa").await];
    for run in &runs {
        d.until_state(run, "passed", WITHIN).await;
    }
    let seen = d
        .journal_until(STOP + WITHIN, |seen| {
            seen.iter().filter(|e| of_codex("instance.stopped", e)).count() >= runs.len()
                && seen.iter().filter(|e| k_is(e, "run.passed")).count() >= runs.len()
        })
        .await;

    // The k-th instance starts only after the k-th run was queued.
    let (mut queued, mut starting) = (0, 0);
    for e in &seen {
        queued += usize::from(k_is(e, "run.queued"));
        if of_codex("instance.starting", e) {
            starting += 1;
            assert!(starting <= queued, "an instance before its run: {seen:?}");
        }
    }
    let ready: Vec<&Value> = seen
        .iter()
        .filter(|e| of_codex("instance.ready", e))
        .map(|(_, e)| e)
        .collect();
    let ids: HashSet<&str> = ready
        .iter()
        .filter_map(|e| e["data"]["instance"]["id"].as_str())
        .collect();
    let pids: HashSet<i64> = ready
        .iter()
        .filter_map(|e| e["data"]["instance"]["pid"].as_i64())
        .collect();
    assert_eq!(ready.len(), runs.len(), "an instance per run: {ready:?}");
    assert_eq!(
        (ids.len(), pids.len()),
        (runs.len(), runs.len()),
        "a process each: {ready:?}"
    );

    let stopped: Vec<&Value> = seen
        .iter()
        .filter(|e| of_codex("instance.stopped", e))
        .map(|(_, e)| e)
        .collect();
    assert!(stopped.iter().all(|e| e["data"]["reason"] == "idle"), "{stopped:?}");
    // Sorted, each stop lies within `STOP` of the end of the run it served, whichever that run is.
    let ts = |e: &Value| parse_rfc3339_ms(e["ts"].as_str().unwrap()).unwrap();
    let mut stops: Vec<i64> = stopped.iter().map(|e| ts(e)).collect();
    let mut passes: Vec<i64> = seen
        .iter()
        .filter(|e| k_is(e, "run.passed"))
        .map(|(_, e)| ts(e))
        .collect();
    stops.sort_unstable();
    passes.sort_unstable();
    let stop = i64::try_from(STOP.as_millis()).unwrap();
    for (s, p) in stops.iter().zip(&passes) {
        assert!(
            *s <= p + stop,
            "an instance outlived its run by over {STOP:?}: {stops:?} vs {passes:?}"
        );
    }
    format!(
        "3 runs on 3 instances ({}); each stopped as idle within {STOP:?} of its run",
        ids.into_iter().collect::<Vec<_>>().join(", ")
    )
}

fn k_is((k, _): &(String, Value), kind: &str) -> bool {
    k == kind
}
