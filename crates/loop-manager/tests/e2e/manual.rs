//! The operator's Stop and answers to permission requests over the API of a daemon on the `acp-agent` fixture
//! (docs/design/contracts.md 6.1): under loop-manager the policy declines a request; a manual run's waits for the
//! operator; a Stop cancels the turn and the agent lives on, or stops the command of an exec step. No model.

use crate::common::{self, Daemon, WAIT};
use loop_manager_front::client::ClientError;
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn agent() -> PathBuf {
    Path::new(common::EXE).with_file_name("acp-agent")
}

/// The journal events `kind` of the daemon.
fn journal(d: &Daemon, kind: &str) -> Vec<Value> {
    let text = fs::read_to_string(d.dir.path().join("state/loop-manager/journal/events.jsonl")).unwrap_or_default();
    // A line the daemon is still appending has no newline yet.
    let whole = text.split_inclusive('\n').filter(|l| l.ends_with('\n'));
    let lines = whole.map(|l| serde_json::from_str::<Value>(l).unwrap());
    lines.filter(|e| e["type"] == kind).collect()
}

/// Polls the view of `run` until `ready` holds for it.
async fn until(d: &Daemon, run: &str, what: &str, ready: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + WAIT;
    loop {
        let view = d.get(&format!("/v1/runs/{run}")).await;
        if ready(&view) {
            return view;
        }
        assert!(Instant::now() < deadline, "{what}: {view}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn answer(reply: Result<Value, ClientError>) -> Value {
    reply.unwrap()["answer"].clone()
}

/// A run of an `ai` step on `acp-agent --permission`, whose command touches `ran` and works for ten minutes: the
/// policy declines its first request; the run, kept waiting for the decision on that turn's end by a hook that never
/// answers, is taken over, and the request of the operator's instruction waits for the operator. Allowed, the command
/// runs; a Stop cancels its turn, and the agent's process stays.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_waits_for_the_operator_of_a_manual_run_and_a_stop_cancels_the_turn() {
    let out = tempfile::tempdir().unwrap();
    let ran = out.path().join("ran");
    let command = format!("touch {}; sleep 600", ran.display());
    let config = format!(
        "projects = [\"shop\"]\nbuiltin = {{ default-policy = {{ enabled = false }} }}\n[harness.fake]\ncommand = \
         \"{}\"\nargs = [\"--permission\"]\n[harness.fake.env]\nACP_AGENT_COMMAND = \"{command}\"\n",
        agent().display()
    );
    let d = Daemon::start(move |dir| {
        let shop = dir.join("config/loop-manager/shop");
        let hold = "name = \"hold\"\nversion = \"0.1.0\"\n[[hook]]\non = \"run.turn_ended_without_outcome\"\n\
                    command = [\"sleep\", \"600\"]\ntimeout = \"300s\"\n";
        let files = [
            (dir.join("config/loop-manager/config.toml"), config.as_str()),
            (
                shop.join("loop-manager.toml"),
                "[[addon]]\npath = \"hold\"\n[settings]\naddons = [{ name = \"hold\" }]\n[rigs.web]\npath = \"web\"\n",
            ),
            (shop.join("hold/addon.toml"), hold),
            (shop.join("agents/dev/agent.toml"), "harness = \"fake\"\n"),
            (
                shop.join("formulas/build.toml"),
                "name = \"build\"\n[[step]]\nkind = \"ai\"\nagent = \"dev\"\n",
            ),
            (shop.join("agents/dev/prompt.template.md"), "Build {{ rig.name }}."),
        ];
        for (path, text) in files {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
    })
    .await;
    let run = d.create("build", None).await;
    let path = |action: &str| format!("/v1/runs/{run}/{action}");
    let deadline = Instant::now() + WAIT;
    while journal(&d, "run.turn_ended_without_outcome").is_empty() {
        assert!(Instant::now() < deadline, "the first turn did not end");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let denied = journal(&d, "run.permission_denied");
    assert_eq!(denied.len(), 1, "{denied:?}");
    assert!(!ran.exists(), "the declined command ran");
    let attempt = d.get(&format!("/v1/runs/{run}")).await["attempt"].clone();
    assert_eq!(
        answer(d.post(&path("takeover"), json!({ "attempt": attempt }), None).await),
        "accepted"
    );
    assert_eq!(
        answer(d.post(&path("stop"), json!({ "attempt": attempt }), None).await),
        "nothing_to_stop"
    );
    let say = json!({ "attempt": attempt, "text": "try again" });
    assert_eq!(answer(d.post(&path("instruct"), say, None).await), "accepted");
    let view = until(&d, &run, "no request waits", |v| v["permissions"][0]["id"].is_string()).await;
    let request = view["permissions"][0].clone();
    let options: Vec<&Value> = request["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| &o["id"])
        .collect();
    assert_eq!(options, [&json!("allow"), &json!("reject")]);
    assert!(!ran.exists());
    let allow = json!({ "attempt": attempt, "request": request["id"], "option": "allow" });
    assert_eq!(
        answer(d.post(&path("permission"), allow.clone(), None).await),
        "accepted"
    );
    assert_eq!(
        answer(d.post(&path("permission"), allow, None).await),
        "already_answered"
    );
    let deadline = Instant::now() + WAIT;
    while !ran.exists() {
        assert!(Instant::now() < deadline, "the allowed command did not run");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(journal(&d, "run.permission_denied").len(), 1);

    let pid = d.get("/v1/instances").await[0]["pid"].clone();
    assert_eq!(
        answer(d.post(&path("stop"), json!({ "attempt": attempt }), None).await),
        "accepted"
    );
    let view = until(&d, &run, "the turn did not end", |v| {
        v["manual"]["deferred"] == "settled"
    })
    .await;
    assert_eq!(
        (&view["manual"]["stop"], &view["state"]),
        (&Value::Null, &json!("running"))
    );
    assert_eq!(journal(&d, "run.stop_requested")[0]["data"]["target"], "turn");
    let instances = d.get("/v1/instances").await;
    assert_eq!(instances[0]["pid"], pid, "{instances}");
    assert!(Path::new(&format!("/proc/{pid}")).exists());
    d.post(&path("cancel"), json!({}), None).await.unwrap();
}

/// A Stop of an exec step stops its command: its outcome `fail` waits for Continue, and the step after it starts
/// only then.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_of_an_exec_step_stops_the_command_and_the_next_step_waits_for_continue() {
    let d = Daemon::start(|dir| {
        let two = "name = \"two\"\n[[step]]\nname = \"wait\"\nkind = \"exec\"\n\
                   command = [\"sh\", \"-c\", \"echo up; exec sleep 600\"]\ntimeout = \"20m\"\n\
                   [[step]]\nname = \"after\"\nkind = \"exec\"\ncommand = [\"true\"]\n";
        fs::write(dir.join("config/loop-manager/shop/formulas/two.toml"), two).unwrap();
    })
    .await;
    let run = d.create("two", None).await;
    let path = |action: &str| format!("/v1/runs/{run}/{action}");
    // The command runs once its output comes: a stop before its start would find nothing to stop.
    let up = d.dir.path().join(format!("state/loop-manager/history/{run}.jsonl"));
    let deadline = Instant::now() + WAIT;
    while !fs::read_to_string(&up).unwrap_or_default().contains("exec_output") {
        assert!(Instant::now() < deadline, "the command did not start");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let attempt = d.get(&format!("/v1/runs/{run}")).await["attempt"].clone();
    assert_eq!(
        answer(d.post(&path("stop"), json!({ "attempt": attempt }), None).await),
        "accepted"
    );
    let view = until(&d, &run, "no outcome", |v| v["manual"]["deferred"] == "outcome").await;
    assert_eq!(
        (&view["step"], &view["outcome"]["result"]),
        (&json!("wait"), &json!("fail"))
    );
    assert_eq!(view["outcome"]["summary"], "stopped by the operator");
    let back = json!({ "attempt": attempt });
    assert_eq!(answer(d.post(&path("continue"), back, None).await), "accepted");
    until(&d, &run, "the run did not go on", |v| {
        v["control"] == "lm" && v["manual"].is_null()
    })
    .await;
    let deadline = Instant::now() + WAIT;
    while journal(&d, "run.finishing").is_empty() {
        assert!(Instant::now() < deadline, "no run.finishing after Continue");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
