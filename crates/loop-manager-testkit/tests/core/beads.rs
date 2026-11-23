//! Bead ownership on the scripted driver and the tracker in memory (docs/design/tracker-br.md, invariants 2–5;
//! criteria 16 and 5a, core part).

use crate::common;

use common::{
    beads::{FIX, next_ready, slow_turn},
    hollow_turn, last, world,
};
use loop_manager_core::driver::Normalized;
use loop_manager_core::run::{Msg, Verdict};
use loop_manager_testkit::Step;
use serde_json::{Value, json};
use std::time::Duration;

#[tokio::test(start_paused = true)]
async fn five_requests_for_the_next_ready_bead_take_five_beads_and_the_sixth_finds_nothing() {
    let w = world(FIX);
    let beads: Vec<String> = (0..5).map(|_| w.tracker.add(&[], None)).collect();
    w.changed().await;
    let replies = tokio::join!(
        w.fix(next_ready()),
        w.fix(next_ready()),
        w.fix(next_ready()),
        w.fix(next_ready()),
        w.fix(next_ready()),
    );
    let runs: Vec<String> = [replies.0, replies.1, replies.2, replies.3, replies.4]
        .into_iter()
        .map(|r| r.unwrap()["run"].as_str().unwrap().to_owned())
        .collect();
    let mut taken: Vec<String> = Vec::new();
    for run in &runs {
        let events = w.events(run, Duration::ZERO).await;
        taken.push(last(&events, "run.queued")["run"]["bead"].as_str().unwrap().to_owned());
    }
    taken.sort();
    assert_eq!(taken, beads, "every bead to exactly one run");
    assert_eq!(w.fix(next_ready()).await.unwrap(), json!({ "nothing_ready": true }));
    let second = w.fix(json!(beads[0])).await.unwrap_err().to_string();
    assert!(second.contains("is held by `run_"), "{second}");
}

#[tokio::test(start_paused = true)]
async fn a_run_claims_its_bead_shows_it_to_the_agent_and_closes_it_when_it_passes() {
    let w = world(FIX);
    let bead = w.tracker.add(&[], None);
    w.driver.script(slow_turn());
    let run = w.fix(json!(bead)).await.unwrap()["run"].as_str().unwrap().to_owned();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        w.tracker.bead(&bead).unwrap().assignee,
        Some(format!("loop-manager:{run}"))
    );
    let (env, prompts) = w.driver.sent(&run, 1);
    assert_eq!(prompts, [format!("Fix Task {bead}.")]);
    let env = env.unwrap();
    assert_eq!(env["TRACKER_ACTOR"], format!("loop-manager:{run}:1"));
    assert_eq!(env["LOOP_MANAGER_BEAD"], bead);
    w.finish_bead(&run, Verdict::Pass, "fixed").await;
    tokio::time::sleep(Duration::from_secs(20)).await;
    let events = w.events(&run, Duration::ZERO).await;
    assert_eq!(last(&events, "run.passed")["run"]["bead"], json!(bead));
    assert_eq!(w.tracker.bead(&bead).unwrap().status, "closed");
    w.changed().await;
    assert_eq!(w.fix(next_ready()).await.unwrap(), json!({ "nothing_ready": true }));
}

#[tokio::test(start_paused = true)]
async fn failed_runs_release_the_bead_and_the_third_closes_its_gate_until_the_operator_opens_it() {
    let w = world(FIX);
    let bead = w.tracker.add(&[], None);
    for _ in 0..3 {
        w.driver.script(hollow_turn());
        let run = w.fix(json!(bead)).await.unwrap()["run"].as_str().unwrap().to_owned();
        assert_eq!(w.states(&run).await.last().unwrap(), "failed");
        let released = w.tracker.bead(&bead).unwrap();
        assert_eq!((released.assignee, released.status.as_str()), (None, "open"));
    }
    let closed = w.all("gate.closed");
    let key = format!("bead:shop/{bead}");
    assert_eq!(closed.len(), 1, "{closed:?}");
    assert_eq!(
        (
            closed[0]["gate"]["key"].as_str(),
            closed[0]["gate"]["closed_by"].as_str()
        ),
        (Some(key.as_str()), Some("core"))
    );
    w.changed().await;
    assert_eq!(
        w.fix(next_ready()).await.unwrap(),
        json!({ "nothing_ready": true }),
        "a gated bead is skipped"
    );
    let waiting = w.fix(json!(bead)).await.unwrap()["run"].as_str().unwrap().to_owned();
    let events = w.events(&waiting, Duration::ZERO).await;
    assert_eq!(last(&events, "run.queued")["waiting_for"], "gate");
    w.driver.script(hollow_turn());
    w.core.send(None, Msg::OpenGate { key }).await.unwrap();
    assert_eq!(w.states(&waiting).await.last().unwrap(), "failed");
    assert_eq!(w.all("gate.closed").len(), 1, "the operator's opening reset the count");
}

#[tokio::test(start_paused = true)]
async fn a_bead_taken_over_by_hand_mid_run_is_a_divergence_and_stays_untouched() {
    let w = world(FIX);
    let bead = w.tracker.add(&[], None);
    w.driver.script(slow_turn());
    let run = w.run_on(json!(bead)).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    w.tracker.assign(&bead, "bob");
    w.finish_bead(&run, Verdict::Pass, "fixed").await;
    tokio::time::sleep(Duration::from_secs(20)).await;
    let diverged = w.all("resync.diverged");
    assert_eq!(
        (diverged.len(), &diverged[0]["observed"], &diverged[0]["expected"]),
        (1, &json!("bob"), &json!(format!("loop-manager:{run}")))
    );
    let held = w.tracker.bead(&bead).unwrap();
    assert_eq!(
        (held.status.as_str(), held.assignee.as_deref()),
        ("in_progress", Some("bob"))
    );
}

#[tokio::test(start_paused = true)]
async fn a_bead_someone_else_holds_fails_the_run_as_bead_claimed_and_stays_theirs() {
    let w = world(FIX);
    let bead = w.tracker.add(&[], None);
    w.tracker.assign(&bead, "alice");
    let run = w.fix(json!(bead)).await.unwrap()["run"].as_str().unwrap().to_owned();
    let events = w.events(&run, Duration::from_secs(1)).await;
    let failed = last(&events, "run.failed");
    assert_eq!(
        (failed["class"].as_str(), failed["reason"].as_str()),
        (Some("infrastructure"), Some("bead_claimed"))
    );
    assert_eq!(w.tracker.bead(&bead).unwrap().assignee.as_deref(), Some("alice"));
    assert!(w.all("resync.diverged").is_empty(), "nothing of alice's was released");
    // One of another rig the core knows is refused by its id and stays unclaimed.
    let seen = w.tracker.add_in("w", &[], None);
    w.changed().await;
    let refused = w.fix_on(Some("root"), json!(seen)).await.unwrap_err().to_string();
    assert!(refused.contains("belongs to another rig than `root`"), "{refused}");
    assert_eq!(w.assignee(&seen), None);
    // One the core has not seen yet is the tracker's answer to the task: its claim fails the run and is released.
    let unseen = w.tracker.add_in("w", &[], None);
    let run = w.fix_on(Some("root"), json!(unseen)).await.unwrap()["run"]
        .as_str()
        .unwrap()
        .to_owned();
    let events = w.events(&run, Duration::from_secs(1)).await;
    assert_eq!(last(&events, "run.failed")["reason"], "bead_of_another_rig");
    assert_eq!(w.assignee(&unseen), None);
}

#[tokio::test(start_paused = true)]
async fn an_unavailable_tracker_is_reported_and_the_claim_retried_until_it_answers() {
    let w = world(FIX);
    let bead = w.tracker.add(&[], None);
    w.changed().await;
    w.tracker.set_down(true);
    w.driver.script(slow_turn());
    let run = w.fix(json!(bead)).await.unwrap()["run"].as_str().unwrap().to_owned();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(w.states(&run).await.last().unwrap(), "preparing");
    let all = w.all("tracker.sync_failed");
    let failed: Vec<&Value> = all.iter().filter(|f| f["op"] == "claim").collect();
    assert!(failed.len() >= 2, "{all:?}");
    assert_eq!(
        (failed[0]["op"].as_str(), failed[0]["bead"].as_str()),
        (Some("claim"), Some(bead.as_str()))
    );
    assert!(failed[0]["next_retry_at"].is_string());
    w.tracker.set_down(false);
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert!(w.states(&run).await.contains(&"running".to_owned()));
}

#[tokio::test(start_paused = true)]
async fn the_record_of_an_attempt_on_an_unavailable_tracker_is_reported_and_retried_until_it_lands() {
    let pair = "name = \"pair\"\n[inputs]\nbead = { type = \"task\", required = true }\n\
        [[step]]\nkind = \"ai\"\nagent = \"fixer\"\n[[step]]\nkind = \"ai\"\nagent = \"fixer\"\n";
    let w = world(&[FIX, &[("shop/formulas/pair.toml", pair)]].concat());
    let bead = w.tracker.add(&[], None);
    w.driver.script(slow_turn());
    // Step two works on while the tracker is down: its run ends with no release.
    let mut long = hollow_turn();
    long.insert(1, Step::Wait(Duration::from_secs(3600)));
    w.driver.script(long);
    let run = w.start_bead("pair", Some("web"), json!(bead)).await.unwrap()["run"]
        .as_str()
        .unwrap()
        .to_owned();
    tokio::time::sleep(Duration::from_secs(1)).await;
    // The end of step one is the only tracker call while the tracker is down.
    w.tracker.set_down(true);
    w.finish_attempt(&run, 1, Verdict::Pass, "fixed").await;
    tokio::time::sleep(Duration::from_secs(20)).await;
    let all = w.all("tracker.sync_failed");
    assert!(
        all.len() >= 2 && all.iter().all(|f| f["op"] == "attempt_ended"),
        "{all:?}"
    );
    assert_eq!(all[0]["bead"], json!(bead));
    assert!(all[0]["next_retry_at"].is_string(), "{all:?}");
    assert!(w.tracker.attempts().is_empty());
    w.tracker.set_down(false);
    tokio::time::sleep(Duration::from_secs(60)).await;
    let records = w.tracker.attempts();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(
        (&records[0].2["attempt"], &records[0].2["outcome"]),
        (&json!(1), &json!("pass"))
    );
}

#[tokio::test(start_paused = true)]
async fn a_trigger_on_the_next_ready_bead_takes_each_bead_once_then_skips_with_nothing_ready() {
    let take = "[rigs.web]\npath = \"web\"\n[rigs.api]\npath = \"api\"\n\n[[trigger]]\nname = \"take\"\nevery = \"1h\"\nrun = \"fix\"\nrig = \
    \"web\"\n\
        inputs = { bead = { next_ready = true } }\n";
    let w = world(&[FIX, &[("shop/loop-manager.toml", take), ("shop/api/.keep", "")]].concat());
    let beads = [
        w.tracker.add(&[], None),
        w.tracker.add_in("a", &[], None),
        w.tracker.add_in("w", &[], None),
    ];
    w.changed().await;
    let fire = || Msg::Fire {
        project: "shop".into(),
        rig: "web".into(),
        trigger: "take".into(),
    };
    let mut taken = Vec::new();
    for _ in 0..2 {
        let run = w.core.send(None, fire()).await.unwrap()["run"]
            .as_str()
            .unwrap()
            .to_owned();
        let events = w.events(&run, Duration::ZERO).await;
        taken.push(last(&events, "run.queued")["run"]["bead"].as_str().unwrap().to_owned());
    }
    assert_eq!(
        taken,
        [beads[0].clone(), beads[2].clone()],
        "a bead of another rig is not taken"
    );
    let third = w.core.send(None, fire()).await.unwrap();
    assert_eq!(third["skipped"], "nothing_ready");
    // Named by id, a bead of another rig is refused at once.
    let refused = w.fix(json!(beads[1])).await.unwrap_err().to_string();
    assert!(refused.contains("another rig"), "{refused}");
}

#[tokio::test(start_paused = true)]
async fn the_next_ready_bead_in_an_epic_skips_beads_of_other_epics() {
    let w = world(FIX);
    w.tracker.add(&[], Some("e-other"));
    w.tracker.add(&[], None);
    let mine = w.tracker.add(&[], Some("e-1"));
    w.changed().await;
    let in_epic = || json!({ "next_ready": true, "epic": "e-1" });
    let run = w.run_on(in_epic()).await;
    let events = w.events(&run, Duration::ZERO).await;
    assert_eq!(last(&events, "run.queued")["run"]["bead"], json!(mine));
    assert_eq!(w.fix(in_epic()).await.unwrap(), json!({ "nothing_ready": true }));
}

#[tokio::test(start_paused = true)]
async fn an_agent_failure_whose_summary_starts_with_bead_still_releases_the_bead() {
    let w = world(FIX);
    let bead = w.tracker.add(&[], None);
    w.driver.script(slow_turn());
    let run = w.run_on(json!(bead)).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    w.finish_bead(&run, Verdict::Fail, "bead_claimed").await;
    tokio::time::sleep(Duration::from_secs(20)).await;
    assert_eq!(w.states(&run).await.last().unwrap(), "failed");
    assert_eq!(w.assignee(&bead), None, "the claim of a failed run goes back");
}

#[tokio::test(start_paused = true)]
async fn claims_that_answer_after_their_runs_failed_neither_count_nor_reach_the_next_run() {
    let w = world(FIX);
    let bead = w.tracker.add(&[], None);
    w.tracker.set_down(true);
    for _ in 0..3 {
        let run = w.run_on(json!(bead)).await;
        tokio::time::sleep(Duration::from_secs(121)).await;
        let events = w.events(&run, Duration::ZERO).await;
        assert_eq!(last(&events, "run.failed")["reason"], "tracker_unavailable");
    }
    assert!(
        w.all("gate.closed").is_empty(),
        "a claim never held is not a failure on the bead"
    );
    w.tracker.set_down(false);
    // The queue drains at its next retry (at most the 5 min cap): each late claim, then its release.
    tokio::time::sleep(Duration::from_secs(301)).await;
    assert_eq!(w.assignee(&bead), None);
    w.driver.script(slow_turn());
    let run = w.run_on(json!(bead)).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(w.states(&run).await.contains(&"running".to_owned()));
    assert_eq!(
        w.assignee(&bead),
        Some(format!("loop-manager:{run}")),
        "the stale claims were released before"
    );
    assert_eq!(w.driver.sent(&run, 1).1, [format!("Fix Task {bead}.")]);
}

#[tokio::test(start_paused = true)]
async fn a_start_without_a_rig_takes_it_from_the_prefix_of_the_bead_id_or_runs_in_the_root() {
    let w = world(FIX);
    let [web, none, labelled] = [
        w.tracker.add_in("w", &[], None),
        w.tracker.add(&[], None),
        w.tracker.add(&["rig:web"], None),
    ];
    let workdir = async |bead: &str| {
        let run = w.fix_on(None, json!(bead)).await.unwrap()["run"]
            .as_str()
            .unwrap()
            .to_owned();
        let events = w.events(&run, Duration::ZERO).await;
        last(&events, "run.queued")["run"]["workdir"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    // The id gives the rig before the ready list is read; a bead of no rig's prefix runs in the root
    // (docs/design/config.md).
    assert!(workdir(&web).await.ends_with("/shop/web"));
    assert!(workdir(&none).await.ends_with("/shop"));
    // A `rig:` label no longer places a bead: its start is refused with the reason.
    w.changed().await;
    let refused = w.fix_on(None, json!(labelled)).await.unwrap_err().to_string();
    assert!(
        refused.contains("has a `rig:` label: the rig of a bead is the prefix of its id"),
        "{refused}"
    );
}

/// The labels of a bead name the agent of every ai step of its run and the harness it is for (docs/design/config.md); a
/// pick passes over a bead its run cannot take (docs/design/tracker-br.md).
#[tokio::test(start_paused = true)]
async fn a_bead_names_the_agent_of_its_run_and_the_harness_it_is_for() {
    let pair = "name = \"pair\"\n[inputs]\nbead = { type = \"task\", required = true }\n\
        [[step]]\nname = \"one\"\nkind = \"ai\"\nagent = \"fixer\"\n\
        [[step]]\nname = \"two\"\nkind = \"ai\"\nagent = \"fixer\"\n";
    let extra = [
        ("shop/agents/qa/agent.toml", "harness = \"scripted\"\n"),
        ("shop/agents/qa/prompt.template.md", "Fix {{ bead.title }}."),
        ("shop/formulas/pair.toml", pair),
    ];
    let w = world(&[FIX, &extra].concat());
    let [codex, twice, qa] = [
        w.tracker.add(&["harness:codex"], None),
        w.tracker.add(&["agent:qa", "agent:dev"], None),
        w.tracker.add(&["agent:qa", "harness:scripted"], None),
    ];
    w.changed().await;
    let refused = async |bead: &str| w.fix(json!(bead)).await.unwrap_err().to_string();
    let why = refused(&codex).await;
    assert!(
        why.contains("labelled `harness:codex`: the config gives `scripted` to `shop.fixer` on `web`"),
        "{why}"
    );
    assert!(refused(&twice).await.contains("has several `agent:` labels"));
    w.driver.script(slow_turn());
    w.driver.script(slow_turn());
    let picked = w.start_bead("pair", Some("web"), next_ready()).await.unwrap();
    let passed_over = picked["passed_over"].as_object().unwrap();
    assert_eq!(passed_over.keys().collect::<Vec<_>>(), [&codex, &twice], "{picked}");
    let run = picked["run"].as_str().unwrap().to_owned();
    for attempt in [1, 2] {
        for _ in 0..60 {
            if w.driver.token(&run, attempt).is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        w.finish_attempt(&run, attempt, Verdict::Pass, "done").await;
    }
    tokio::time::sleep(Duration::from_secs(20)).await;
    let events = w.events(&run, Duration::ZERO).await;
    assert_eq!(last(&events, "run.queued")["run"]["bead"], qa);
    assert_eq!(last(&events, "run.queued")["run"]["agent"], "shop.qa");
    assert_eq!(last(&events, "run.passed")["run"]["agent"], "shop.qa", "step two");
}

#[tokio::test(start_paused = true)]
async fn each_ended_attempt_of_a_run_with_a_bead_is_recorded_on_it_once_and_a_run_without_one_records_none() {
    let w = world(FIX);
    let bead = w.tracker.add(&[], None);
    w.driver.script(hollow_turn());
    let failed = w.run_on(json!(bead)).await;
    let states = w.states(&failed).await;
    assert_eq!(states.last().unwrap(), "failed");
    let tried = states.iter().filter(|s| *s == "starting").count();
    w.driver.script(slow_turn());
    let passed = w.run_on(json!(bead)).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    w.finish_bead(&passed, Verdict::Pass, "fixed").await;
    w.driver.script(hollow_turn());
    let plain = w.create().await;
    assert_eq!(w.states(&plain).await.last().unwrap(), "failed");
    tokio::time::sleep(Duration::from_secs(20)).await;
    let records = w.tracker.attempts();
    let of = |run: &str| -> Vec<(String, Value)> {
        let mine = records.iter().filter(|(_, _, e)| e["run"] == run);
        (mine.map(|(actor, id, e)| (actor.clone(), json!([id, e["attempt"], e["outcome"]])))).collect()
    };
    let failures = of(&failed);
    assert_eq!(failures.len(), tried, "one record per attempt: {records:?}");
    for (n, (actor, record)) in failures.iter().enumerate() {
        assert_eq!(actor, &format!("loop-manager:{failed}:{}", n + 1));
        assert_eq!(record, &json!([bead, n + 1, null]), "an attempt with no outcome");
    }
    let pass = &records.iter().find(|(_, _, e)| e["run"] == passed.as_str()).unwrap().2;
    assert_eq!(
        of(&passed),
        [(format!("loop-manager:{passed}:1"), json!([bead, 1, "pass"]))]
    );
    assert_eq!(
        (
            &pass["summary"],
            &pass["step"],
            &pass["agent"],
            &pass["harness"],
            &pass["round"]
        ),
        (
            &json!("fixed"),
            &json!("1"),
            &json!("shop.fixer"),
            &json!("scripted"),
            &json!(1)
        ),
        "{pass}"
    );
    assert!(of(&plain).is_empty(), "a run without a bead records nothing");
}

/// The record of an attempt holds its model, session, tokens and failure, and its session has the tracker variables of
/// its step: the agent, the harness, and the model only when the config sets one (docs/design/contracts.md 5.1).
#[tokio::test(start_paused = true)]
async fn an_attempt_record_holds_its_model_session_tokens_and_failure_and_its_session_the_steps_agent() {
    let pair = "name = \"pair\"\n[inputs]\nbead = { type = \"task\", required = true }\n\
        [[step]]\nkind = \"ai\"\nagent = \"fixer\"\n[[step]]\nkind = \"ai\"\nagent = \"checker\"\n";
    let files = [
        (
            "shop/agents/fixer/agent.toml",
            "harness = \"scripted\"\nmodel = \"m1\"\n",
        ),
        ("shop/agents/fixer/prompt.template.md", "Fix {{ bead.title }}."),
        ("shop/agents/checker/agent.toml", "harness = \"scripted\"\n"),
        ("shop/agents/checker/prompt.template.md", "Check {{ bead.title }}."),
        ("shop/formulas/pair.toml", pair),
    ];
    let w = world(&files);
    let bead = w.tracker.add(&[], None);
    let mut first = slow_turn();
    first.insert(
        1,
        Step::Emit(Normalized::Usage {
            input_tokens: 100,
            output_tokens: 20,
            rate_limit: None,
        }),
    );
    w.driver.script(first);
    w.driver.script(vec![Step::Exit {
        code: None,
        signal: Some(9),
    }]);
    let run = w.start_bead("pair", Some("web"), json!(bead)).await.unwrap()["run"]
        .as_str()
        .unwrap()
        .to_owned();
    tokio::time::sleep(Duration::from_secs(1)).await;
    w.finish_attempt(&run, 1, Verdict::Pass, "fixed").await;
    tokio::time::sleep(Duration::from_secs(20)).await;
    let env = |attempt| {
        let env = w.driver.sent(&run, attempt).0.unwrap();
        let vars = ["TRACKER_AGENT", "TRACKER_HARNESS", "TRACKER_MODEL"].map(|k| env.get(k).cloned());
        vars.map(|v| v.map(Value::from).unwrap_or_default())
    };
    assert_eq!(env(1), [json!("shop.fixer"), json!("scripted"), json!("m1")]);
    assert_eq!(
        env(2),
        [json!("shop.checker"), json!("scripted"), Value::Null],
        "no model, no variable"
    );
    let records = w.tracker.attempts();
    let record = |n: u32| {
        &records
            .iter()
            .find(|(_, _, e)| e["run"] == run.as_str() && e["attempt"] == n)
            .unwrap()
            .2
    };
    let reader = loop_manager_core::run::Reader::open(&w.dir.path().join("state.db")).unwrap();
    let view = reader.run(&run).unwrap().unwrap();
    let (fixed, crashed) = (record(1), record(2));
    let fields = |e: &Value| {
        json!([
            e["agent"],
            e["model"],
            e["input_tokens"],
            e["output_tokens"],
            e["failure"]
        ])
    };
    assert_eq!(fields(fixed), json!(["shop.fixer", "m1", 100, 20, null]), "{fixed}");
    assert_eq!(fixed["session"], view["attempts"][0]["session"]);
    assert!(fixed["session"].is_object(), "{fixed}");
    assert_eq!(
        (&crashed["agent"], &crashed["model"]),
        (&json!("shop.checker"), &Value::Null),
        "{crashed}"
    );
    assert_eq!(crashed["failure"]["kind"], "harness_crash", "{crashed}");
    assert_eq!(crashed["session"], view["attempts"][1]["session"]);
}
