//! A prompt the agent may or may not have got when its daemon died (docs/design/contracts.md 6.4,
//! docs/guide/recovery.md): the replay of the resumed session tells, and when it does not, the delivery is disputed
//! until the operator chooses. The `acp-agent` fixture keeps its sessions in a store across its processes and holds a
//! prompt before or after it inserts it. No model.

use crate::{
    acp::gone,
    common::{self, Daemon},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::Output,
    time::{Duration, Instant},
};

/// The prompt of an attempt that resumes a session which got the last prompt.
const RECONCILE: &str = "Your previous turn was interrupted.";

/// A daemon whose `build` formula is one `ai` step on the `acp-agent` fixture with `--store <dir>/store`, `hold`
/// (`--hold-before-insert` or `--hold-after-insert`) on the marker `<dir>/held`, and `flags` more. `top` goes before
/// the tables of the config. With `second`, the marker is there from the start and the first turn takes it away
/// without an outcome: the prompt held is the policy's reminder, the second of the session, whose first the agent
/// named; else the prompt held is the first of the session. The turn after the hold declares a pass.
async fn daemon(hold: &str, flags: &[&str], top: &str, second: bool) -> Daemon {
    let agent = Path::new(common::EXE).with_file_name("acp-agent");
    let (hold, top) = (hold.to_owned(), top.to_owned());
    let flags: Vec<String> = flags.iter().map(|f| format!(", \"{f}\"")).collect();
    Daemon::start(move |dir| {
        let (store, held, first) = (dir.join("store"), dir.join("held"), dir.join("first"));
        fs::create_dir_all(&store).unwrap();
        let finish = format!("{} finish pass --summary built", common::EXE);
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
        let config = format!(
            "projects = [\"shop\"]\n{top}[harness.fake]\ncommand = \"{}\"\nargs = [\"--store\", \"{}\", \"{hold}\", \
             \"{}\"{}]\n[harness.fake.env]\nACP_AGENT_COMMAND = \"{command}\"\n",
            agent.display(),
            store.display(),
            held.display(),
            flags.concat()
        );
        let shop = dir.join("config/loop-manager/shop");
        let files = [
            (dir.join("config/loop-manager/config.toml"), config),
            (shop.join("agents/dev/agent.toml"), "harness = \"fake\"\n".to_owned()),
            (
                shop.join("formulas/build.toml"),
                "name = \"build\"\n[[step]]\nkind = \"ai\"\nagent = \"dev\"\n".to_owned(),
            ),
            (
                shop.join("agents/dev/prompt.template.md"),
                "Build {{ rig.name }}.".to_owned(),
            ),
        ];
        for (path, text) in files {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
    })
    .await
}

/// Waits until `ready` holds; `what` says what did not come.
async fn until(what: impl Fn() -> String, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + common::WAIT;
    while !ready() {
        assert!(Instant::now() < deadline, "{}", what());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// `kill -9` of the daemon, then a new one on the same directories.
pub(crate) async fn restart(d: &mut Daemon) {
    d.child.kill().unwrap();
    d.child.wait().unwrap();
    d.child = common::spawn(d.dir.path());
    let deadline = Instant::now() + common::WAIT;
    while d.client.request("GET", "/v1/daemon", None, None).await.is_err() {
        assert!(Instant::now() < deadline, "the daemon did not come up");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Starts a run of `build` and kills the daemon once the agent holds a prompt; a new daemon takes the run on.
async fn held(d: &mut Daemon) -> String {
    let run = holding(d).await;
    restart(d).await;
    run
}

/// Starts a run of `build` and waits until the agent holds a prompt.
async fn holding(d: &Daemon) -> String {
    let run = d.create("build", None).await;
    let (marker, first) = (d.dir.path().join("held"), d.dir.path().join("first"));
    // With the marker there from the start, the first turn takes it away, then leaves `first`.
    if marker.exists() {
        until(|| "the first turn did not end".into(), || first.exists()).await;
    }
    until(|| "no prompt held".into(), || marker.exists()).await;
    run
}

/// The view of `run` once its delivery is disputed.
pub(crate) async fn disputed(d: &Daemon, run: &str) -> Value {
    let deadline = Instant::now() + common::WAIT;
    loop {
        let view = d.get(&format!("/v1/runs/{run}")).await;
        if !view["disputed"].is_null() {
            return view;
        }
        assert!(Instant::now() < deadline, "no dispute: {view:#}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The keys of the prompts the agent inserted, in order.
fn inserted(d: &Daemon) -> Vec<String> {
    let text = fs::read_to_string(d.dir.path().join("store/inserted")).unwrap_or_default();
    text.lines().map(str::to_owned).collect()
}

/// The texts of the user messages the agent keeps, of every session.
fn kept(d: &Daemon) -> Vec<String> {
    let entries = fs::read_dir(d.dir.path().join("store")).unwrap().flatten();
    let files = entries.filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"));
    let text: String = files.map(|e| fs::read_to_string(e.path()).unwrap()).collect();
    (text.lines())
        .filter_map(|l| Some(serde_json::from_str::<Value>(l).ok()?["text"].as_str()?.to_owned()))
        .collect()
}

/// The `delivery` records of the run's history, once there are `n`.
async fn deliveries(d: &Daemon, run: &str, n: usize) -> Vec<Value> {
    let path = d.dir.path().join(format!("state/loop-manager/history/{run}.jsonl"));
    let read = || -> Vec<Value> {
        let text = fs::read_to_string(&path).unwrap_or_default();
        (text.lines())
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|r| r["kind"] == "delivery")
            .collect()
    };
    until(
        || format!("not {n} delivery records: {:?}", read()),
        || read().len() >= n,
    )
    .await;
    read()
}

/// `loop-manager <args> --json` on the daemon: its exit and its answer.
fn cli(d: &Daemon, args: &[&str]) -> (bool, Value) {
    let out: Output = common::loop_manager()
        .args(args)
        .arg("--json")
        .env("LOOP_MANAGER_SOCKET", d.socket())
        .output()
        .unwrap();
    let answer = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|_| panic!("{args:?}: {}", String::from_utf8_lossy(&out.stderr)));
    (out.status.success(), answer)
}

/// Each key the agent inserted, once.
fn once_each(d: &Daemon) -> Vec<String> {
    let keys = inserted(d);
    let mut unique = keys.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), keys.len(), "a prompt inserted twice: {keys:?}");
    keys
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_the_agent_never_got_goes_again_with_its_key_after_a_daemon_restart() {
    let mut d = daemon("--hold-before-insert", &[], "", true).await;
    let run = held(&mut d).await;
    d.until_state(&run, "passed").await;
    let records = deliveries(&d, &run, 1).await;
    assert_eq!(
        (&records[0]["outcome"], &records[0]["by"]),
        (&json!("not_delivered"), &json!("replay"))
    );
    let keys = once_each(&d);
    assert!(keys.iter().any(|k| *k == records[0]["key"]), "{keys:?}");
    assert!(!kept(&d).iter().any(|t| t.starts_with(RECONCILE)), "{:?}", kept(&d));
    gone(&run, &d).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_the_agent_got_is_followed_by_the_reconcile_prompt_after_a_daemon_restart() {
    let mut d = daemon("--hold-after-insert", &[], "", true).await;
    let run = held(&mut d).await;
    d.until_state(&run, "passed").await;
    let records = deliveries(&d, &run, 1).await;
    assert_eq!(
        (&records[0]["outcome"], &records[0]["by"]),
        (&json!("delivered"), &json!("replay"))
    );
    let keys = once_each(&d);
    assert!(keys.iter().any(|k| *k == records[0]["key"]), "{keys:?}");
    assert!(kept(&d).iter().any(|t| t.starts_with(RECONCILE)), "{:?}", kept(&d));
    gone(&run, &d).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_disputed_delivery_stands_across_restarts_and_the_operator_settles_it() {
    let mut d = daemon("--hold-after-insert", &["--partial-replay"], "", false).await;
    let run = held(&mut d).await;
    let view = disputed(&d, &run).await;
    let (_, shown) = cli(&d, &["run", "show", &run]);
    assert_eq!(shown["disputed"], view["disputed"]);
    let dispute = &view["disputed"];
    assert!(dispute["text"].as_str().unwrap().contains("Build web."), "{view:#}");
    assert_eq!(dispute["why"], "replay_incomplete", "{view:#}");
    // Another restart: the dispute stays as it was, held by the next attempt.
    restart(&mut d).await;
    let deadline = Instant::now() + common::WAIT;
    let view = loop {
        let now = d.get(&format!("/v1/runs/{run}")).await;
        if now["attempt"].as_u64() > view["attempt"].as_u64() && !now["disputed"].is_null() {
            break now;
        }
        assert!(Instant::now() < deadline, "{now:#}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(view["disputed"]["key"], dispute["key"], "{view:#}");
    let attempt = view["attempt"].to_string();
    let (ok, answer) = cli(&d, &["run", "takeover", &run, "--attempt", &attempt]);
    assert!(ok, "{answer}");
    let (ok, answer) = cli(&d, &["run", "continue", &run, "--attempt", &attempt]);
    assert!(!ok);
    assert_eq!(answer["answer"], "delivery_disputed", "{answer}");
    assert_eq!(answer["disputed"]["key"], dispute["key"], "{answer}");
    let (ok, answer) = cli(&d, &["run", "delivery", &run, "delivered", "--attempt", &attempt]);
    assert!(ok, "{answer}");
    assert_eq!(answer["answer"], "accepted");
    // The first choice is the one: applied by now, or kept for the attempt that is opening the session.
    let (ok, answer) = cli(&d, &["run", "delivery", &run, "resend", "--attempt", &attempt]);
    assert!(!ok);
    let second = (&answer["answer"], &answer["choice"]);
    assert!(
        second == (&json!("not_disputed"), &Value::Null) || second == (&json!("already_resolved"), &json!("delivered")),
        "{answer}"
    );
    let (ok, answer) = cli(&d, &["run", "continue", &run, "--attempt", &attempt]);
    assert!(ok, "{answer}");
    d.until_state(&run, "passed").await;
    let records = deliveries(&d, &run, 2).await;
    let outcomes: Vec<(&Value, &Value)> = records.iter().map(|r| (&r["outcome"], &r["by"])).collect();
    assert_eq!(
        outcomes,
        [
            (&json!("disputed"), &json!("replay")),
            (&json!("delivered"), &json!("operator"))
        ]
    );
    once_each(&d);
    assert!(kept(&d).iter().any(|t| t.starts_with(RECONCILE)), "{:?}", kept(&d));
    gone(&run, &d).await;
}

/// A silence short enough for the attempt the agent acknowledges a resent prompt in, with no turn, to stall soon.
const SILENCE: &str = "[watchdog]\nsilence = \"2s\"\n";

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_resent_that_the_agent_holds_is_not_inserted_again() {
    let mut d = daemon("--hold-after-insert", &["--partial-replay"], SILENCE, false).await;
    let run = held(&mut d).await;
    let view = disputed(&d, &run).await;
    let attempt = view["attempt"].to_string();
    let (ok, answer) = cli(&d, &["run", "delivery", &run, "resend", "--attempt", &attempt]);
    assert!(ok, "{answer}");
    d.until_state(&run, "passed").await;
    let records = deliveries(&d, &run, 2).await;
    assert_eq!(
        (&records[1]["outcome"], &records[1]["by"]),
        (&json!("not_delivered"), &json!("operator"))
    );
    let keys = once_each(&d);
    assert_eq!(keys.iter().filter(|k| **k == records[1]["key"]).count(), 1, "{keys:?}");
    gone(&run, &d).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn of_two_operators_choosing_at_once_one_settles_the_dispute() {
    let mut d = daemon("--hold-after-insert", &["--partial-replay"], SILENCE, false).await;
    let run = held(&mut d).await;
    let view = disputed(&d, &run).await;
    let path = format!("/v1/runs/{run}/delivery");
    let body = |choice: &str| json!({ "attempt": view["attempt"], "choice": choice });
    let (a, b) = tokio::join!(
        d.post(&path, body("delivered"), None),
        d.post(&path, body("resend"), None)
    );
    let mut answers = [a.unwrap()["answer"].clone(), b.unwrap()["answer"].clone()];
    answers.sort_by_key(ToString::to_string);
    assert_eq!(answers[0], "accepted", "{answers:?}");
    assert!(
        answers[1] == "not_disputed" || answers[1] == "already_resolved",
        "{answers:?}"
    );
    d.until_state(&run, "passed").await;
    let records = deliveries(&d, &run, 2).await;
    let by_operator = records.iter().filter(|r| r["by"] == "operator").count();
    assert_eq!(by_operator, 1, "{records:#?}");
    // The prompt disputed and the one prompt after it: reconcile, at once or after the resent one is acknowledged.
    assert_eq!(once_each(&d).len(), 2, "{:?}", inserted(&d));
    gone(&run, &d).await;
}

/// The processes of the `acp-agent` fixture of this daemon's directory: their command line names its store.
fn agents(d: &Daemon) -> Vec<i32> {
    let store = d.dir.path().join("store");
    let store = store.to_string_lossy();
    let entries = fs::read_dir("/proc").unwrap().flatten();
    entries
        .filter_map(|e| {
            let pid: i32 = e.file_name().to_str()?.parse().ok()?;
            let comm = fs::read_to_string(e.path().join("comm")).ok()?;
            let line = String::from_utf8_lossy(&fs::read(e.path().join("cmdline")).ok()?).into_owned();
            (comm.trim() == "acp-agent" && line.contains(&*store)).then_some(pid)
        })
        .collect()
}

/// `kill -9` of the agent that holds a prompt while the daemon lives: the attempt fails as `harness_crash`, and the
/// next one resumes the session and checks the delivery as after a restart of the daemon; the outcome of the check.
async fn agent_killed(hold: &str) -> Value {
    let d = daemon(hold, &[], "", true).await;
    let run = holding(&d).await;
    let [agent] = agents(&d)[..] else {
        panic!("not one agent: {:?}", agents(&d))
    };
    let killed = std::process::Command::new("kill")
        .args(["-9", &agent.to_string()])
        .status();
    assert!(killed.unwrap().success());
    let view = d.until_state(&run, "passed").await;
    assert_eq!(view["attempts"][0]["failure_kind"], "harness_crash", "{view:#}");
    let records = deliveries(&d, &run, 1).await;
    assert_eq!(records[0]["by"], "replay", "{records:#?}");
    let keys = once_each(&d);
    assert!(keys.iter().any(|k| *k == records[0]["key"]), "{keys:?}");
    let reconciled = kept(&d).iter().any(|t| t.starts_with(RECONCILE));
    assert_eq!(reconciled, records[0]["outcome"] == "delivered", "{:?}", kept(&d));
    gone(&run, &d).await;
    records[0]["outcome"].clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_a_killed_agent_never_got_goes_again_with_its_key_while_the_daemon_lives() {
    assert_eq!(agent_killed("--hold-before-insert").await, "not_delivered");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_a_killed_agent_got_is_followed_by_the_reconcile_prompt_while_the_daemon_lives() {
    assert_eq!(agent_killed("--hold-after-insert").await, "delivered");
}
