//! `loop-manager acp` (docs/api/acp.md) as an editor drives it, on a real daemon with a br tracker and the real
//! `claude` on the fake Anthropic API: the list of tasks, the replay of a task with a child-bead session whose tool is
//! still running, the live result of that tool, the read-only methods, the snapshot of the root's comments, and a
//! client that goes away in the middle of a turn.

use super::tasks::{br, claude};
use crate::common::{self, Daemon};
use loop_manager_testkit::anthropic::{Provider, Reply};
use serde_json::{Value, json};
use std::{fs, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout},
};

/// A `loop-manager acp` process on the daemon's socket, with every line it wrote so far.
struct Acp {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Lines<BufReader<ChildStdout>>,
    seen: Vec<Value>,
}

impl Acp {
    fn spawn(d: &Daemon) -> Self {
        let mut child = tokio::process::Command::from(common::loop_manager())
            .kill_on_drop(true)
            .arg("acp")
            .env("LOOP_MANAGER_SOCKET", d.socket())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        Self {
            child,
            stdin,
            lines,
            seen: Vec::new(),
        }
    }

    async fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        stdin.write_all(format!("{message}\n").as_bytes()).await.unwrap();
        stdin.flush().await.unwrap();
    }

    /// The lines read up to and including the first that matches `done`.
    async fn until(&mut self, what: &str, done: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut got = Vec::new();
        let read = tokio::time::timeout(common::WAIT, async {
            while let Some(line) = self.lines.next_line().await.unwrap() {
                let line: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"));
                self.seen.push(line.clone());
                let matched = done(&line);
                got.push(line);
                if matched {
                    return true;
                }
            }
            false
        })
        .await;
        assert!(read == Ok(true), "no {what} ({read:?}), all lines: {:#?}", self.seen);
        got
    }

    /// Sends request `id` and reads up to its response: the lines before it, and the response.
    async fn call(&mut self, id: u64, method: &str, params: Value) -> (Vec<Value>, Value) {
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.send(request).await;
        let mut lines = self
            .until(&format!("the response to {method}"), |l| l["id"] == id)
            .await;
        let response = lines.pop().unwrap();
        (lines, response)
    }
}

/// The `update` of the `session/update` notifications on `session`.
fn on<'a>(lines: &'a [Value], session: &str) -> Vec<&'a Value> {
    (lines.iter())
        .filter(|l| l["method"] == "session/update" && l["params"]["sessionId"] == session)
        .map(|l| &l["params"]["update"])
        .collect()
}

fn kind(update: &Value) -> &str {
    update["sessionUpdate"].as_str().unwrap_or_default()
}

/// A Bash command that marks `name` started, waits for `<name>-go`, then runs `then`.
fn held(name: &str, then: &str) -> String {
    format!("touch {name} && while test ! -e {name}-go; do sleep 0.05; done && {then}")
}

/// Waits until the held command `name` has started in `dir`.
async fn started(dir: &Path, name: &str) {
    tokio::time::timeout(common::WAIT, async {
        while !dir.join(name).exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{name} did not start"));
}

/// The run's state and its number of attempts.
async fn progress(d: &Daemon, run: &str) -> (Value, usize) {
    let view = d.get(&format!("/v1/runs/{run}")).await;
    (view["state"].clone(), view["attempts"].as_array().unwrap().len())
}

/// The ids of the task's session and of its nodes.
struct Ids {
    root: String,
    bead: String,
    round: String,
    thread: String,
}

/// Checks the replay of the task while the first tool runs; returns the id of its call.
fn replayed(replay: &[Value], ids: &Ids, bead: &str) -> String {
    let Ids {
        root,
        bead: bead_node,
        round,
        thread,
    } = ids;
    let on_root = on(replay, root);
    assert_eq!(
        on_root[..3].iter().map(|u| kind(u)).collect::<Vec<_>>(),
        ["user_message", "agent_message", "agent_message"],
        "{replay:#?}"
    );
    assert_eq!(
        on_root[0]["content"],
        json!([{ "type": "text", "text": "The whole cart." }])
    );
    for (update, (id, text)) in on_root[1..3]
        .iter()
        .zip([("c1", "First note."), ("c2", "Second note.")])
    {
        assert_eq!(update["messageId"], id, "{update}");
        assert_eq!(update["_meta"]["loop-manager/author"], "alice", "{update}");
        assert!(update["_meta"]["loop-manager/createdAt"].is_string(), "{update}");
        let shown = update["content"][0]["text"].as_str().unwrap();
        assert!(shown.starts_with("**alice** · ") && shown.ends_with(text), "{update}");
    }

    let announced: Vec<_> = (replay.iter())
        .filter(|l| l["params"]["update"]["sessionUpdate"] == "subagent_update")
        .map(|l| (l["params"]["sessionId"].as_str().unwrap(), &l["params"]["update"]))
        .collect();
    let parents: Vec<_> = (announced.iter())
        .map(|(parent, u)| (*parent, u["sessionId"].as_str().unwrap(), &u["state"]))
        .collect();
    let running = json!({ "state": "running" });
    assert_eq!(
        parents,
        [
            (root.as_str(), bead_node.as_str(), &running),
            (bead_node.as_str(), round.as_str(), &running),
            (round.as_str(), thread.as_str(), &running),
        ],
        "{replay:#?}"
    );
    assert_eq!(announced[0].1["title"], format!("{bead} Fix the total"));
    assert_eq!(announced[1].1["title"], "Round 1");
    let thread_node = announced[2].1;
    assert!(
        thread_node["title"].as_str().unwrap().ends_with("(claude)"),
        "{thread_node}"
    );
    assert!(thread_node["cwd"].is_string(), "{thread_node}");

    let chat = on(replay, thread);
    let first_chat = replay.iter().position(|l| l["params"]["sessionId"] == thread.as_str());
    let thread_announced = (replay.iter()).position(|l| l["params"]["update"]["sessionId"] == thread.as_str());
    assert!(thread_announced < first_chat, "{replay:#?}");
    let prompt = chat.iter().find(|u| kind(u) == "user_message").expect("the prompt");
    assert!(
        prompt["content"][0]["text"].as_str().unwrap().contains("Build web."),
        "{prompt}"
    );
    let calls: Vec<_> = chat.iter().filter(|u| kind(u) == "tool_call_update").collect();
    assert_eq!(calls.len(), 1, "{chat:#?}");
    let call = calls[0];
    assert_eq!(
        (&call["status"], &call["name"], &call["kind"]),
        (&json!("in_progress"), &json!("Bash"), &json!("other")),
        "{call}"
    );

    let tail: Vec<_> = (replay[replay.len() - 5..].iter())
        .map(|l| (l["params"]["sessionId"].as_str().unwrap(), &l["params"]["update"]))
        .collect();
    let complete = json!({ "sessionUpdate": "replay_complete" });
    let idle = json!({ "sessionUpdate": "state_update", "state": "idle", "stopReason": "end_turn" });
    assert_eq!(
        tail,
        [
            (bead_node.as_str(), &complete),
            (round.as_str(), &complete),
            (thread.as_str(), &complete),
            (root.as_str(), &complete),
            (root.as_str(), &idle),
        ],
        "{replay:#?}"
    );
    call["toolCallId"].as_str().unwrap().to_owned()
}

/// A claude daemon with a finished beadless run, an epic with two comments, and a run of `fix` on a bead under it
/// whose first tool is running: the daemon, the epic, the bead, the run.
async fn task(provider: &Provider) -> (Daemon, String, String, String) {
    let d = claude(provider).await;
    let shop = d.dir.path().join("config/loop-manager/shop");
    let epic = [
        "create",
        "Ship the cart",
        "-t",
        "epic",
        "-d",
        "The whole cart.",
        "--silent",
    ];
    let epic = br(&shop, &epic);
    let under = format!("parent-child:{epic}");
    let bead = br(&shop, &["create", "Fix the total", "--deps", &under, "--silent"]);
    br(&shop, &["comments", "add", &epic, "First note."]);
    br(&shop, &["comments", "add", &epic, "Second note."]);
    let beadless = d.create("check", None).await;
    d.until_state(&beadless, "passed").await;
    let body = json!({ "project": "shop", "rig": "web", "formula": "fix", "inputs": { "bead": bead } });
    let run = d.post("/v1/runs", body, None).await.unwrap()["run"]
        .as_str()
        .unwrap()
        .to_owned();
    started(&shop.join("web"), "acp-one").await;
    (d, epic, bead, run)
}

#[tokio::test(flavor = "multi_thread")]
async fn acp_replays_a_task_follows_its_tool_reads_only_and_survives_the_client() {
    let finish = format!("{} finish pass --summary built", common::EXE);
    let provider = Provider::start(vec![Reply::Turn(vec![
        Reply::Bash(held("acp-one", "echo first")),
        Reply::Bash(held("acp-two", &finish)),
        Reply::Text("Done.".into()),
    ])])
    .await;
    let (d, epic, bead, run) = task(&provider).await;
    let shop = d.dir.path().join("config/loop-manager/shop");
    let web = shop.join("web");

    let mut acp = Acp::spawn(&d);
    let (_, init) = acp.call(1, "initialize", json!({ "protocolVersion": 2 })).await;
    assert_eq!(init["result"]["protocolVersion"], 2, "{init}");
    assert_eq!(init["result"]["capabilities"], json!({ "session": {} }), "{init}");
    assert_eq!(init["result"]["info"]["name"], "loop-manager", "{init}");

    // The list: the epic is the only task, the beadless run adds none.
    let (_, list) = acp.call(2, "session/list", json!({})).await;
    let sessions = list["result"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "{list}");
    let root = format!("shop/{epic}");
    assert_eq!(sessions[0]["sessionId"], root.as_str(), "{list}");
    assert_eq!(sessions[0]["cwd"], shop.to_string_lossy().as_ref(), "{list}");
    assert_eq!(sessions[0]["title"], format!("{epic} Ship the cart"), "{list}");
    assert!(sessions[0]["updatedAt"].is_string(), "{list}");

    // The replay while the tool runs: the root's history, the tree parent first, the thread's transcript, the end.
    let resume = json!({ "sessionId": root, "cwd": shop, "replayFrom": { "type": "start" } });
    let (replay, response) = acp.call(3, "session/resume", resume.clone()).await;
    assert_eq!(response, json!({ "jsonrpc": "2.0", "id": 3, "result": {} }));
    let ids = Ids {
        bead: format!("{root}#b:{bead}"),
        round: format!("{root}#r:{run}.1"),
        thread: format!("{root}#t:{run}.1"),
        root,
    };
    let tool_call = replayed(&replay, &ids, &bead);

    // Read-only: a prompt and a new session are refused and change nothing.
    let before = (progress(&d, &run).await, provider.requests().len());
    let prompt = json!({ "sessionId": ids.root, "prompt": [{ "type": "text", "text": "Stop." }] });
    let (_, refused) = acp.call(4, "session/prompt", prompt).await;
    assert_eq!(refused["error"]["code"], 1, "{refused}");
    let (_, refused) = acp
        .call(5, "session/new", json!({ "cwd": shop, "mcpServers": [] }))
        .await;
    assert_eq!(refused["error"]["code"], 1, "{refused}");
    assert_eq!((progress(&d, &run).await, provider.requests().len()), before);

    // A comment added while the task is open does not show; the tool's result arrives live on its call.
    br(&shop, &["comments", "add", &epic, "Third note."]);
    fs::write(web.join("acp-one-go"), "").unwrap();
    let thread_at = |l: &Value| l["params"]["sessionId"] == ids.thread.as_str();
    let live = acp
        .until("the live result of the tool", |l| {
            let u = &l["params"]["update"];
            thread_at(l) && u["toolCallId"] == tool_call.as_str() && u["status"] == "completed"
        })
        .await;
    let messages = on(&live, &ids.root)
        .into_iter()
        .filter(|u| kind(u).ends_with("_message"));
    assert_eq!(messages.count(), 0, "{live:#?}");

    // A second resume replays from scratch, with the new comment.
    let (again, response) = acp.call(6, "session/resume", resume.clone()).await;
    assert_eq!(response, json!({ "jsonrpc": "2.0", "id": 6, "result": {} }));
    let third = (on(&again, &ids.root).into_iter())
        .find(|u| u["messageId"] == "c3")
        .unwrap_or_else(|| panic!("no third comment: {again:#?}"));
    assert!(
        third["content"][0]["text"].as_str().unwrap().ends_with("Third note."),
        "{third}"
    );

    // The client goes away while the second tool runs: the run still passes.
    let second = |l: &Value| {
        let u = &l["params"]["update"];
        thread_at(l) && u["status"] == "in_progress" && u["toolCallId"] != tool_call.as_str()
    };
    if !again.iter().any(second) {
        acp.until("the second tool call", second).await;
    }

    // A resume that supersedes one still replaying, and a close: every request has exactly one response.
    let request = |id: u64, method: &str| json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": resume });
    acp.send(request(7, "session/resume")).await;
    let (_, response) = acp.call(8, "session/resume", resume.clone()).await;
    assert_eq!(response, json!({ "jsonrpc": "2.0", "id": 8, "result": {} }));
    let (_, response) = acp.call(9, "session/close", json!({ "sessionId": ids.root })).await;
    assert_eq!(response, json!({ "jsonrpc": "2.0", "id": 9, "result": {} }));
    let answers = |id: u64| acp.seen.iter().filter(|l| l["id"] == id).cloned().collect::<Vec<_>>();
    let first = answers(7);
    assert_eq!(first.len(), 1, "{:#?}", acp.seen);
    let superseded = json!({ "code": -32800, "message": "Request cancelled: superseded by a later session/resume" });
    assert!(
        first[0]["result"] == json!({}) || first[0]["error"] == superseded,
        "{first:#?}"
    );
    drop(acp.stdin.take());
    let exit = tokio::time::timeout(common::WAIT, acp.child.wait()).await;
    assert!(exit.expect("acp exits").unwrap().success());
    fs::write(web.join("acp-two-go"), "").unwrap();
    d.until_state(&run, "passed").await;
}
