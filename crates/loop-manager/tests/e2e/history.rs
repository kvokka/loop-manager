//! The history of a run over the API (docs/design/event-model.md, "History"; docs/api/frontends.md): the stream from a
//! cursor across a disconnect and a daemon restart, `caught_up`, a record cut to its limit and a run over
//! `[history] max_run_bytes`. The `acp-agent` fixture stands in for a harness. No model.

use crate::common::{self, Daemon, WAIT};
use loop_manager_front::client::{ClientError, Events};
use serde_json::{Value, json};
use std::{fs, path::Path, time::Instant};

/// The next event of `events` as `(event, data)`; `None` once the stream ended.
async fn next(events: &mut Events) -> Option<(String, Value)> {
    let event = tokio::time::timeout(WAIT, events.next())
        .await
        .expect("the stream went quiet");
    event
        .unwrap()
        .map(|e| (e.event, serde_json::from_str(&e.data).unwrap()))
}

/// The history stream of `run` from `from`.
async fn open(d: &Daemon, run: &str, from: u64) -> Events {
    d.client
        .stream(&format!("/v1/runs/{run}/history?from={from}"))
        .await
        .unwrap()
}

/// The records of `run` from `from` to the end of the stream; there is no `caught_up` before the run's end.
async fn to_end(d: &Daemon, run: &str, from: u64) -> Vec<Value> {
    let mut events = open(d, run, from).await;
    let mut records = Vec::new();
    while let Some((event, data)) = next(&mut events).await {
        if event != "caught_up" {
            assert_eq!(event, data["kind"], "{data}");
            records.push(data);
        }
    }
    records
}

/// The records of a live `run` from the first until `caught_up`, and its data.
async fn written(d: &Daemon, run: &str) -> (Vec<Value>, Value) {
    let mut events = open(d, run, 1).await;
    let mut records = Vec::new();
    loop {
        let (event, data) = next(&mut events).await.expect("the stream of a live run ended");
        if event == "caught_up" {
            return (records, data);
        }
        records.push(data);
    }
}

fn numbers(records: &[Value]) -> Vec<u64> {
    records.iter().map(|r| r["n"].as_u64().unwrap()).collect()
}

fn kinds(records: &[Value]) -> Vec<&str> {
    records.iter().map(|r| r["kind"].as_str().unwrap()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_history_resumes_from_its_cursor_after_a_disconnect_and_goes_on_after_a_daemon_restart() {
    let mut d = Daemon::start(|_| {}).await;
    match d.client.stream("/v1/runs/run_404/history").await {
        Err(ClientError::Api { status: 404, error }) => assert_eq!(error["code"], "not_found"),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("a history of no run"),
    }
    let run = d.create("slow", None).await;
    d.until_state(&run, "running").await;
    // What is written so far, then `caught_up` with the last `n`; the reader goes away there. The records follow the
    // commit of the state, so the reader comes again until the step's attempt is among them.
    let deadline = Instant::now() + WAIT;
    let before = loop {
        let (before, caught_up) = written(&d, &run).await;
        assert_eq!(caught_up, json!({ "n": before.len() }));
        if before.len() >= 2 {
            break before;
        }
        assert!(Instant::now() < deadline, "{before:#?}");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(kinds(&before), ["step", "attempt"], "{before:#?}");
    assert_eq!(
        (
            &before[0]["step"],
            &before[0]["step_kind"],
            &before[0]["index"],
            &before[0]["round"]
        ),
        (&json!("1"), &json!("exec"), &json!(0), &json!(1)),
        "{before:#?}"
    );
    assert_eq!(numbers(&before), [1, 2]);
    // The stream reopened from its first record sends what the first one did.
    let mut again = open(&d, &run, 1).await;
    for record in &before {
        assert_eq!(next(&mut again).await.unwrap().1, *record);
    }
    drop(again);

    assert!(d.terminate().success());
    d.child = common::spawn(d.dir.path());
    let deadline = Instant::now() + WAIT;
    while d.client.request("GET", "/v1/daemon", None, None).await.is_err() {
        assert!(Instant::now() < deadline, "the daemon did not come back");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // Ended by hand if the restart has not ended it already.
    let _ = d.post(&format!("/v1/runs/{run}/cancel"), json!({}), None).await;
    let after = to_end(&d, &run, 3).await;
    let all: Vec<Value> = before.iter().chain(&after).cloned().collect();
    assert_eq!(numbers(&all), (1..=all.len() as u64).collect::<Vec<_>>(), "{all:#?}");
    assert_eq!(kinds(&after).first(), Some(&"attempt_end"), "{after:#?}");
    assert_eq!(kinds(&after).last(), Some(&"run"), "{after:#?}");
    // A reader past the run's end gets nothing more, and the stream ends.
    assert!(to_end(&d, &run, all.len() as u64 + 1).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_over_its_history_bytes_gets_one_truncated_record_and_keeps_its_boundaries() {
    let d = Daemon::start(|dir| {
        let config = "projects = [\"shop\"]\n[history]\nmax_run_bytes = 300\n";
        fs::write(dir.join("config/loop-manager/config.toml"), config).unwrap();
    })
    .await;
    let run = d.create("check", None).await;
    let records = to_end(&d, &run, 1).await;
    assert_eq!(
        kinds(&records),
        ["step", "attempt", "truncated", "attempt_end", "run"],
        "{records:#?}"
    );
    assert_eq!(records[2]["max_run_bytes"], 300);
    assert_eq!(numbers(&records), [1, 2, 3, 4, 5]);
    // The CLI prints the records from its cursor, one JSON line each, and ends with the run.
    let out = common::loop_manager()
        .args(["run", "watch", &run, "--history", "--from", "4", "--json"])
        .env("LOOP_MANAGER_SOCKET", d.socket())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let printed: Vec<Value> = (String::from_utf8(out.stdout).unwrap().lines())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(printed, records[3..]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_over_the_record_limit_is_cut_and_marked_truncated() {
    let agent = Path::new(common::EXE).with_file_name("acp-agent");
    let d = Daemon::start(|dir| {
        let shop = dir.join("config/loop-manager/shop");
        let section = format!(
            "command = \"{}\"\n[harness.fake.env]\nACP_AGENT_COMMAND = \"{} finish pass --summary built\"\n",
            agent.display(),
            common::EXE
        );
        let files = [
            (
                dir.join("config/loop-manager/config.toml"),
                format!("projects = [\"shop\"]\n[harness.fake]\n{section}"),
            ),
            (shop.join("agents/dev/agent.toml"), "harness = \"fake\"\n".to_owned()),
            (
                shop.join("formulas/build.toml"),
                "name = \"build\"\n[[step]]\nkind = \"ai\"\nagent = \"dev\"\n".to_owned(),
            ),
            (shop.join("agents/dev/prompt.template.md"), "word ".repeat(16 * 1024)),
        ];
        for (path, text) in files {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
    })
    .await;
    let run = d.create("build", None).await;
    let records = to_end(&d, &run, 1).await;
    let prompt = records.iter().find(|r| r["kind"] == "prompt").expect("a prompt record");
    assert_eq!(prompt["truncated"], true, "{}", prompt["key"]);
    let text = prompt["text"].as_str().unwrap();
    assert!(
        text.starts_with("word word") && text.len() < 64 * 1024,
        "{}",
        text.len()
    );
    assert_eq!(records.last().unwrap()["state"], "passed");
}
