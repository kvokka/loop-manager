//! Criterion 37, model-free: an outside ACP client on `loop-manager acp` over stdio, JSON-RPC a line, as an editor
//! speaks it. It lists the runs of the project, opens a running one late, while its tool hangs, and sees the output so
//! far; takes it over with `/lm-takeover` and hands it back with `/lm-continue` while a second client watches; goes,
//! and the run goes on; comes back and takes the run over by a plain prompt, whose instruction reaches the agent once
//! after `/lm-continue send`, and the run passes in the same attempt. The harness is the `acp-agent` fixture.

use super::fake_agent;
use crate::{Env, daemon::Daemon};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin},
    sync::mpsc,
};

const WITHIN: Duration = Duration::from_secs(60);
const PRINTED: &str = "printed-before-the-hang\n";

/// One `loop-manager acp` process with a client on its stdio, initialized on protocol version 2; dropped, it goes.
struct Acp {
    stdin: ChildStdin,
    next: u64,
    /// The answers to this client's requests, in order.
    answers: mpsc::UnboundedReceiver<Value>,
    /// The `session/update` notifications, in order.
    updates: mpsc::UnboundedReceiver<Value>,
    _child: Child,
}

impl Acp {
    async fn start(d: &Daemon) -> Self {
        let mut command = tokio::process::Command::from(d.program());
        let mut child = command
            .arg("acp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let (answer, answers) = mpsc::unbounded_channel();
        let (update, updates) = mpsc::unbounded_channel();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                let message: Value = serde_json::from_str(&line).unwrap_or_else(|_| panic!("not JSON: {line}"));
                let _ = match message["method"].as_str() {
                    None => answer.send(message),
                    // The notification's params: `{sessionId, update}`.
                    Some("session/update") => update.send(message["params"].clone()),
                    Some(other) => panic!("an unexpected {other}: {message}"),
                };
            }
        });
        let mut acp = Self {
            stdin: child.stdin.take().unwrap(),
            next: 0,
            answers,
            updates,
            _child: child,
        };
        let hello = json!({ "protocolVersion": 2, "info": { "name": "acceptance", "version": "0" } });
        let answer = acp.request("initialize", hello).await.unwrap();
        assert_eq!(
            (&answer["protocolVersion"], &answer["info"]["name"]),
            (&json!(2), &json!("loop-manager")),
            "{answer}"
        );
        acp
    }

    /// The result of `method` with `params`, or its error.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        self.next += 1;
        let request = json!({ "jsonrpc": "2.0", "id": self.next, "method": method, "params": params });
        let line = format!("{request}\n");
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
        let answer = tokio::time::timeout(WITHIN, self.answers.recv()).await;
        let answer = answer.unwrap_or_else(|_| panic!("no answer to {request}")).unwrap();
        assert_eq!(answer["id"], self.next, "{answer}");
        match answer.get("error") {
            Some(error) => Err(error.clone()),
            None => Ok(answer["result"].clone()),
        }
    }

    /// `session/resume` of `run`, from its first record when `replay`: the updates that came before the answer.
    async fn resume(&mut self, run: &str, replay: bool) -> Vec<Value> {
        let mut params = json!({ "sessionId": run, "cwd": "/" });
        if replay {
            params["replayFrom"] = json!({ "type": "start" });
        }
        self.request("session/resume", params).await.unwrap();
        let mut before = Vec::new();
        while let Ok(update) = self.updates.try_recv() {
            before.push(update);
        }
        before
    }

    /// The prompt `text` to `run`, answered, then the updates up to the notice of the daemon's answer, which comes
    /// last.
    async fn say(&mut self, run: &str, text: &str) -> Vec<Value> {
        let params = json!({ "sessionId": run, "prompt": [{ "type": "text", "text": text }] });
        let answer = self.request("session/prompt", params).await.unwrap();
        let reply = format!("{}-reply", answer["messageId"].as_str().unwrap());
        self.until(|u| u["update"]["messageId"] == reply.as_str()).await
    }

    /// The updates up to the first that `stop` takes, which comes last.
    async fn until(&mut self, stop: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut seen = Vec::new();
        loop {
            let update = tokio::time::timeout(WITHIN, self.updates.recv()).await;
            let update = update.unwrap_or_else(|_| panic!("{seen:#?}")).unwrap();
            let done = stop(&update);
            seen.push(update);
            if done {
                return seen;
            }
        }
    }
}

/// The text of an update: a message chunk's or a tool call's content chunk's.
fn text(update: &Value) -> &str {
    let content = &update["update"]["content"];
    content["text"]
        .as_str()
        .or(content["content"]["text"].as_str())
        .unwrap_or_default()
}

fn printed(update: &Value) -> bool {
    update["update"]["sessionUpdate"] == "tool_call_content_chunk" && text(update) == PRINTED
}

/// The text of the last update.
fn last(seen: &[Value]) -> &str {
    seen.last().map(text).unwrap_or_default()
}

/// The control of `run` (`lm` or `manual`) and its attempt.
async fn control(d: &Daemon, run: &str) -> (String, u64) {
    let view = d.get(&format!("/v1/runs/{run}")).await;
    (
        view["control"].as_str().unwrap_or_default().to_owned(),
        view["attempt"].as_u64().unwrap_or_default(),
    )
}

/// The first client lists the runs and opens `run` late, while its tool hangs; a second client opens it too. The
/// first takes the run over and hands it back in the same attempt, the second sees both, and the first goes.
async fn observe(d: &Daemon, run: &str) -> Acp {
    let mut first = Acp::start(d).await;
    let cwd = d.path("config/loop-manager/shop");
    let listed = first.request("session/list", json!({ "cwd": cwd })).await.unwrap();
    let sessions = listed["sessions"].as_array().cloned().unwrap_or_default();
    assert!(sessions.iter().any(|s| s["sessionId"] == run), "{listed}");
    // Late: the replay holds what the run did so far, and the tool's output follows live if it was not there yet.
    let replayed = first.resume(run, true).await;
    if !replayed.iter().any(printed) {
        first.until(printed).await;
    }
    assert_eq!(d.get(&format!("/v1/runs/{run}")).await["state"], "running");
    let mut second = Acp::start(d).await;
    second.resume(run, false).await;

    let taken = first.say(run, "/lm-takeover").await;
    assert!(last(&taken).starts_with("— /lm-takeover accepted"), "{taken:#?}");
    assert_eq!(control(d, run).await, ("manual".to_owned(), 1));
    second.until(|u| text(u) == "— taken over by an operator —\n").await;
    let back = first.say(run, "/lm-continue").await;
    assert!(last(&back).starts_with("— /lm-continue accepted"), "{back:#?}");
    assert_eq!(control(d, run).await, ("lm".to_owned(), 1));
    second.until(|u| text(u) == "— handed back to loop-manager —\n").await;
    drop(first);
    assert_eq!(d.get(&format!("/v1/runs/{run}")).await["state"], "running");
    second
}

/// A client that comes back takes `run` over by a plain prompt and sends it with `/lm-continue send`; once `go` is
/// there the run passes in attempt 1, the instruction echoed once, and `second` sees the end.
async fn come_back(d: &Daemon, run: &str, go: &Path, mut second: Acp) {
    let mut again = Acp::start(d).await;
    again.resume(run, true).await;
    let said = again.say(run, "check the docs").await;
    assert!(
        last(&said).starts_with("— the instruction is taken: loop-manager holds it"),
        "{said:#?}"
    );
    assert_eq!(control(d, run).await, ("manual".to_owned(), 1));
    let view = d.get(&format!("/v1/runs/{run}")).await;
    assert_eq!(view["manual"]["unsent"][0]["state"], "queued", "{view}");
    let sent = again.say(run, "/lm-continue send").await;
    assert!(last(&sent).starts_with("— /lm-continue send accepted"), "{sent:#?}");

    std::fs::write(go, "").unwrap();
    let mut after = again.until(|u| text(u).starts_with("— run passed")).await;
    after.extend(said);
    let echoes = after.iter().filter(|u| text(u) == "Echo: check the docs").count();
    assert_eq!(echoes, 1, "{after:#?}");
    second.until(|u| text(u).starts_with("— run passed")).await;
    let view = d.until_state(run, "passed", WITHIN).await;
    assert_eq!(
        (&view["attempt"], &view["control"]),
        (&json!(1), &json!("lm")),
        "{view}"
    );
}

pub(crate) async fn run(env: Arc<Env>) -> String {
    // The tool prints, then waits for `go`; the first turn ends without an outcome, the turn of the instruction passes.
    let gate = tempfile::tempdir().unwrap();
    let (go, seen) = (gate.path().join("go"), gate.path().join("seen"));
    let command = format!(
        "echo {}; while [ ! -e {go} ]; do sleep 0.05; done; if [ -e {seen} ]; then {lm} finish pass --summary built; \
         fi; touch {seen}",
        PRINTED.trim_end(),
        go = go.display(),
        seen = seen.display(),
        lm = env.loop_manager.display()
    );
    let d = Daemon::start(&env, |root| fake_agent(&env, root, "", "", &command)).await;
    let run = d.create("build").await;
    let second = observe(&d, &run).await;
    come_back(&d, &run, &go, second).await;
    d.until_no_steps(Duration::from_secs(15)).await;
    String::new()
}
