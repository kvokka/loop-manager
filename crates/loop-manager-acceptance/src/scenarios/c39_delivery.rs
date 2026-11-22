//! Criterion 39, no model: `kill -9` at the delivery boundary of a prompt, through a real daemon and the
//! `acp-agent` fixture, which keeps its sessions in a store across its processes and holds a prompt before or after it
//! inserts it. The daemon killed while the agent holds a prompt it never got: the replay of the resumed session says
//! `not_delivered` and the prompt goes again under its key. The daemon or the agent killed after the agent inserted it:
//! `delivered`, and the reconcile prompt follows. A replay that cannot tell: the delivery is disputed; the run taken
//! over stays manual and disputed across another `kill -9`, Continue is refused until the operator chooses with
//! `run delivery`, and then it passes. Each prompt is inserted once, and no process is left.

use super::fake_agent;
use crate::{Env, daemon::Daemon};
use serde_json::{Value, json};
use std::{fs, sync::Arc, time::Duration};

const WITHIN: Duration = Duration::from_secs(60);
/// The prompt of an attempt that resumes a session which got the last prompt.
const RECONCILE: &str = "Your previous turn was interrupted.";

/// A daemon whose `build` runs the fixture with its store, `hold` on the marker `held`, and `flags`. With `second`,
/// the marker is there from the start and the first turn takes it away without an outcome: the prompt held is the
/// second of the session. The turn after the hold declares a pass.
async fn daemon(env: &Arc<Env>, hold: &str, flags: &str, second: bool) -> Daemon {
    Daemon::start(env, |root| {
        let (store, held, first) = (root.join("store"), root.join("held"), root.join("first"));
        fs::create_dir_all(&store).unwrap();
        let finish = format!("{} finish pass --summary built", env.loop_manager.display());
        let command = match second {
            true => {
                fs::write(&held, "").unwrap();
                format!(
                    "if [ -e {first} ]; then {finish}; else rm -f {held}; touch {first}; fi",
                    first = first.display(),
                    held = held.display()
                )
            }
            false => finish,
        };
        let args = format!(
            "\"--store\", \"{}\", \"{hold}\", \"{}\"{flags}",
            store.display(),
            held.display()
        );
        fake_agent(env, root, "", &args, &command);
    })
    .await
}

/// Waits up to `WITHIN` for `ready`; `what` says what did not come.
async fn until(what: &str, ready: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + WITHIN;
    while !ready() {
        assert!(tokio::time::Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Starts a run of `build` and waits until the agent holds a prompt.
async fn holding(d: &Daemon) -> String {
    let run = d.create("build").await;
    let (held, first) = (d.path("held"), d.path("first"));
    if held.exists() {
        until("the first turn did not end", || first.exists()).await;
    }
    until("no prompt held", || held.exists()).await;
    run
}

/// The `delivery` records of the history of `run`.
fn deliveries(d: &Daemon, run: &str) -> Vec<Value> {
    let text = fs::read_to_string(d.path(&format!("state/loop-manager/history/{run}.jsonl"))).unwrap_or_default();
    (text.lines())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|r| r["kind"] == "delivery")
        .collect()
}

/// The keys of the prompts the agent inserted, asserted to be each once.
fn inserted_once(d: &Daemon) -> Vec<String> {
    let text = fs::read_to_string(d.path("store/inserted")).unwrap_or_default();
    let keys: Vec<String> = text.lines().map(str::to_owned).collect();
    let mut unique = keys.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), keys.len(), "a prompt inserted twice: {keys:?}");
    keys
}

/// Whether the agent got the reconcile prompt, in any of its sessions.
fn reconciled(d: &Daemon) -> bool {
    let entries = fs::read_dir(d.path("store")).unwrap().flatten();
    let files = entries.filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"));
    let text: String = files.map(|e| fs::read_to_string(e.path()).unwrap()).collect();
    (text.lines())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .any(|m| m["text"].as_str().is_some_and(|t| t.starts_with(RECONCILE)))
}

/// The pids of the fixture's processes of `d`: their command line names its store.
fn agents(d: &Daemon) -> Vec<u32> {
    let store = d.path("store").to_string_lossy().into_owned();
    let entries = fs::read_dir("/proc").unwrap().flatten();
    entries
        .filter_map(|e| {
            let pid = e.file_name().to_str()?.parse().ok()?;
            let comm = fs::read_to_string(e.path().join("comm")).ok()?;
            let line = String::from_utf8_lossy(&fs::read(e.path().join("cmdline")).ok()?).into_owned();
            (comm.trim() == "acp-agent" && line.contains(&store)).then_some(pid)
        })
        .collect()
}

/// A prompt held at `hold`, then `kill -9` of the daemon or, with `agent`, of the agent: the run passes, the replay
/// told `outcome`, the reconcile prompt went only after a delivered one.
async fn killed(env: Arc<Env>, hold: &str, agent: bool, outcome: &str) -> String {
    let mut d = daemon(&env, hold, "", true).await;
    let run = holding(&d).await;
    let what = match agent {
        true => {
            let [pid] = agents(&d)[..] else {
                panic!("not one agent: {:?}", agents(&d))
            };
            let kill = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .status();
            assert!(kill.unwrap().success());
            "agent"
        }
        false => {
            d.restart().await;
            "daemon"
        }
    };
    let view = d.until_state(&run, "passed", WITHIN).await;
    if agent {
        assert_eq!(view["attempts"][0]["failure_kind"], "harness_crash", "{view}");
    }
    let records = deliveries(&d, &run);
    let [record] = &records[..] else {
        panic!("not one delivery record: {records:?}")
    };
    assert_eq!(
        (&record["outcome"], &record["by"]),
        (&json!(outcome), &json!("replay")),
        "{record}"
    );
    let keys = inserted_once(&d);
    assert!(keys.iter().any(|k| *k == record["key"]), "{keys:?}");
    assert_eq!(reconciled(&d), outcome == "delivered");
    d.until_no_steps(Duration::from_secs(15)).await;
    format!("{what} killed at {hold}: {outcome}")
}

/// A replay that cannot tell: the dispute, the takeover, another `kill -9`, the operator's choice and Continue.
async fn disputed(env: Arc<Env>) -> String {
    let mut d = daemon(&env, "--hold-after-insert", ", \"--partial-replay\"", false).await;
    let run = holding(&d).await;
    d.restart().await;
    let view = d
        .until_view(&run, "no dispute", WITHIN, |v| !v["disputed"].is_null())
        .await;
    let dispute = view["disputed"].clone();
    assert_eq!(dispute["why"], "replay_incomplete", "{view}");
    assert!(
        dispute["text"].as_str().unwrap_or_default().contains("Build web."),
        "{view}"
    );
    let attempt = view["attempt"].to_string();
    let (ok, answer) = d.cli_json(&["run", "takeover", &run, "--attempt", &attempt]);
    assert!(ok, "{answer}");

    d.restart().await;
    let view = d.get(&format!("/v1/runs/{run}")).await;
    assert_eq!(
        (&view["control"], &view["disputed"]["key"], &view["attempt"].to_string()),
        (&json!("manual"), &dispute["key"], &attempt),
        "{view}"
    );
    let (ok, answer) = d.cli_json(&["run", "continue", &run, "--attempt", &attempt]);
    assert!(!ok && answer["answer"] == "delivery_disputed", "{answer}");
    let (ok, answer) = d.cli_json(&["run", "delivery", &run, "delivered", "--attempt", &attempt]);
    assert!(ok, "{answer}");
    let (ok, answer) = d.cli_json(&["run", "continue", &run, "--attempt", &attempt]);
    assert!(ok, "{answer}");
    d.until_state(&run, "passed", WITHIN).await;
    let records = deliveries(&d, &run);
    let first = records.first().map(|r| (&r["outcome"], &r["by"]));
    assert_eq!(first, Some((&json!("disputed"), &json!("replay"))), "{records:?}");
    let by_operator: Vec<&Value> = records.iter().filter(|r| r["by"] == "operator").collect();
    assert_eq!(by_operator.len(), 1, "{records:?}");
    assert_eq!(by_operator[0]["outcome"], "delivered", "{records:?}");
    inserted_once(&d);
    assert!(
        reconciled(&d),
        "no reconcile prompt after a delivery the operator confirmed"
    );
    d.until_no_steps(Duration::from_secs(15)).await;
    format!(
        "disputed ({}), kept manual across kill -9, delivered by the operator",
        dispute["why"]
    )
}

pub(crate) async fn run(env: Arc<Env>) -> String {
    let (a, b, c, e) = tokio::join!(
        killed(env.clone(), "--hold-before-insert", false, "not_delivered"),
        killed(env.clone(), "--hold-after-insert", false, "delivered"),
        killed(env.clone(), "--hold-after-insert", true, "delivered"),
        disputed(env.clone()),
    );
    [a, b, c, e].join("; ")
}
