//! `loop-manager acp` (docs/api/acp.md): a read-only ACP v2 agent on stdio. Each task of the daemon is a session, its
//! rounds, child beads and harness sessions are child sessions, and their transcripts are replayed and followed. It
//! only reads the task operations of the API: it starts, prompts and cancels nothing.
//!
//! One thread reads stdin, one owns stdout, and one task per open root relays that root's stream. What a line or an
//! event becomes is decided by `Agent` and `View`, without I/O.

use super::encode;
use crate::client::{Client, ClientError, Events};
use loop_manager_core::driver::chat::ChatEntry;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{self, BufRead, Write},
    process::ExitCode,
    thread,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

const VERSION: u64 = 2;
const READ_ONLY: &str = "loop-manager acp is read-only: start work with loop-manager run start";

/// A line for stdout, or the reason the process stops with an error.
enum Out {
    Line(Value),
    Fatal(String),
}

/// Serves ACP on stdin and stdout until stdin closes (success) or a stream of the daemon ends (failure).
pub(super) async fn serve(client: &Client) -> ExitCode {
    let (lines_tx, mut lines) = mpsc::channel::<String>(16);
    thread::spawn(move || {
        for line in io::stdin().lock().split(b'\n') {
            let Ok(line) = line else { break };
            if lines_tx
                .blocking_send(String::from_utf8_lossy(&line).into_owned())
                .is_err()
            {
                break;
            }
        }
    });
    let (out, rx) = mpsc::channel(64);
    let (done_tx, mut done) = oneshot::channel();
    thread::spawn(move || done_tx.send(write(rx)));
    let mut agent = Agent::default();
    let mut open: HashMap<String, Relay> = HashMap::new();
    loop {
        let line = tokio::select! {
            line = lines.recv() => line,
            code = &mut done => return code.unwrap_or(ExitCode::FAILURE),
        };
        let Some(line) = line else { break };
        let reply = match agent.call(&line) {
            None => None,
            Some(Call::Reply(reply)) => Some(reply),
            Some(Call::List { id, cwd }) => Some(match client.request("GET", "/v1/tasks", None, None).await {
                Ok(tasks) => sessions(id, &tasks, cwd.as_deref()),
                Err(e) => error(id, -32603, &e.to_string()),
            }),
            Some(Call::Resume { id, root, path, replay }) => {
                if let Some(old) = open.remove(&root) {
                    old.stop("superseded by a later session/resume").await;
                }
                match resume(client, &out, id, &root, &path, replay).await {
                    Ok(relay) => {
                        open.insert(root, relay);
                        None
                    }
                    Err(reply) => Some(reply),
                }
            }
            Some(Call::Close { id, root }) => Some(match open.remove(&root) {
                Some(relay) => {
                    relay.stop("the session was closed").await;
                    result(id, json!({}))
                }
                None => missing(id, &root),
            }),
        };
        if let Some(reply) = reply
            && out.send(Out::Line(reply)).await.is_err()
        {
            break;
        }
    }
    open.values().for_each(|relay| relay.task.abort());
    drop(out);
    done.await.unwrap_or(ExitCode::FAILURE)
}

/// Writes the lines of `rx` to stdout until every sender is gone (success) or a fatal reason comes (failure).
fn write(mut rx: mpsc::Receiver<Out>) -> ExitCode {
    let mut stdout = io::stdout().lock();
    while let Some(out) = rx.blocking_recv() {
        let failed = match out {
            Out::Line(line) => writeln!(stdout, "{line}")
                .and_then(|()| stdout.flush())
                .err()
                .map(|e| format!("stdout: {e}")),
            Out::Fatal(reason) => Some(reason),
        };
        if let Some(reason) = failed {
            eprintln!("loop-manager acp: {reason}");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

/// The relay of an open root, and the way to stop it.
struct Relay {
    task: JoinHandle<()>,
    stop: oneshot::Sender<&'static str>,
}

impl Relay {
    /// Stops the relay once it has answered a `session/resume` still waiting for its replay with `reason`.
    async fn stop(self, reason: &'static str) {
        // A relay that already ended has nothing left to answer.
        let _ = self.stop.send(reason);
        let _ = self.task.await;
    }
}

/// Opens the stream of `root` and starts its relay; the snapshot of the bead goes out first when `replay`.
async fn resume(
    client: &Client,
    out: &mpsc::Sender<Out>,
    id: Value,
    root: &str,
    path: &str,
    replay: bool,
) -> Result<Relay, Value> {
    let failure = |e: ClientError| match e {
        ClientError::Api { status: 404, .. } => missing(id.clone(), root),
        e => error(id.clone(), -32603, &e.to_string()),
    };
    let snapshot = match replay {
        true => snapshot(root, &client.request("GET", path, None, None).await.map_err(failure)?),
        false => Vec::new(),
    };
    let events = client.stream(&format!("{path}/live")).await.map_err(failure)?;
    for line in snapshot {
        // The writer is gone only when the process ends anyway.
        let _ = out.send(Out::Line(line)).await;
    }
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(relay(events, View::new(root, id, replay), out.clone(), stopped));
    Ok(Relay { task, stop })
}

/// Sends the updates of each event of a root's stream; an end of the stream ends the process. A stop answers the
/// pending `session/resume`, if any, and ends the relay alone.
async fn relay(
    mut events: Events,
    mut view: View,
    out: mpsc::Sender<Out>,
    mut stopped: oneshot::Receiver<&'static str>,
) {
    let reason = loop {
        let next = tokio::select! {
            next = events.next() => next,
            reason = &mut stopped => {
                if let Some(reply) = reason.ok().and_then(|reason| view.cancel(reason)) {
                    let _ = out.send(Out::Line(reply)).await;
                }
                return;
            }
        };
        let event = match next {
            Ok(Some(event)) => event,
            Ok(None) => break "the daemon ended the stream".to_owned(),
            Err(e) => break e.to_string(),
        };
        match view.updates(&event.event, &event.data) {
            Ok(lines) => {
                for line in lines {
                    if out.send(Out::Line(line)).await.is_err() {
                        return;
                    }
                }
            }
            Err(reason) => break reason,
        }
    };
    let _ = out.send(Out::Fatal(format!("{}: {reason}", view.root))).await;
}

/// What a line of stdin asks for.
#[derive(Debug, PartialEq)]
enum Call {
    /// The reply is ready.
    Reply(Value),
    /// `session/list`: the tasks, filtered to those in `cwd`.
    List { id: Value, cwd: Option<String> },
    /// `session/resume` of the task `root` at the API path `path`.
    Resume {
        id: Value,
        root: String,
        path: String,
        replay: bool,
    },
    /// `session/close` of the task `root`.
    Close { id: Value, root: String },
}

/// The connection's state: `initialized` once `initialize` succeeded, `started` once another request came.
#[derive(Default)]
struct Agent {
    initialized: bool,
    started: bool,
}

impl Agent {
    /// What a line asks for; `None` for a notification or a response, which get no answer.
    fn call(&mut self, line: &str) -> Option<Call> {
        if line.trim().is_empty() {
            return None;
        }
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return Some(Call::Reply(error(Value::Null, -32700, "Parse error")));
        };
        if message.is_array() {
            return Some(Call::Reply(error(Value::Null, -32600, "Batches are not supported")));
        }
        let Some(id) = message.get("id").cloned() else {
            return match message.is_object() {
                true => None,
                false => Some(Call::Reply(error(Value::Null, -32600, "Invalid request"))),
            };
        };
        if !(id.is_string() || id.is_i64() || id.is_u64()) {
            return Some(Call::Reply(error(
                Value::Null,
                -32600,
                "The id is not a string or an integer",
            )));
        }
        let Some(method) = message["method"].as_str() else {
            // A response to a request: this agent sends none.
            return (message.get("result").is_none() && message.get("error").is_none())
                .then(|| Call::Reply(error(id, -32600, "Invalid request")));
        };
        Some(self.request(id, method, &message["params"]))
    }

    fn request(&mut self, id: Value, method: &str, params: &Value) -> Call {
        if method == "initialize" {
            return Call::Reply(match self.started {
                true => error(id, -32600, "initialize must come before any other request"),
                false => self.initialize(id, params),
            });
        }
        if !self.initialized {
            return Call::Reply(error(id, -32600, "Send initialize first"));
        }
        self.started = true;
        let session = params["sessionId"].as_str();
        let child = session.is_some_and(|s| s.contains('#'));
        let reply = match method {
            "session/list" => return list(id, params),
            "session/new" => error(id, 1, READ_ONLY),
            "session/resume" | "session/close" | "session/prompt" if session.is_none() => {
                error(id, -32602, "sessionId is missing")
            }
            "session/resume" | "session/close" | "session/prompt" if child => error(
                id,
                -32602,
                "a child session is opened with its root and cannot be the target of a request",
            ),
            "session/prompt" => error(id, 1, READ_ONLY),
            "session/close" => {
                return Call::Close {
                    id,
                    root: session.unwrap_or_default().to_owned(),
                };
            }
            "session/resume" => return resume_call(id, session.unwrap_or_default(), &params["replayFrom"]),
            _ => error(id, -32601, "Method not found"),
        };
        Call::Reply(reply)
    }

    fn initialize(&mut self, id: Value, params: &Value) -> Value {
        let Some(requested) = params["protocolVersion"].as_u64() else {
            return error(id, -32602, "protocolVersion is not an integer");
        };
        if requested < VERSION {
            let data = json!({ "requested": requested, "supported": [VERSION] });
            return with_data(error(id, -32022, "Unsupported protocol version"), data);
        }
        self.initialized = true;
        let info = json!({ "name": "loop-manager", "title": "loop-manager", "version": env!("CARGO_PKG_VERSION") });
        result(
            id,
            json!({ "protocolVersion": VERSION, "capabilities": { "session": {} }, "info": info }),
        )
    }
}

fn list(id: Value, params: &Value) -> Call {
    if !params["cursor"].is_null() {
        return Call::Reply(error(id, -32602, "there is no cursor: the list is one page"));
    }
    match &params["cwd"] {
        Value::Null => Call::List { id, cwd: None },
        Value::String(cwd) if cwd.starts_with('/') => Call::List {
            id,
            cwd: Some(cwd.clone()),
        },
        _ => Call::Reply(error(id, -32602, "cwd is not an absolute path")),
    }
}

fn resume_call(id: Value, session: &str, from: &Value) -> Call {
    let replay = match from {
        Value::Null => false,
        from if from["type"] == "start" => true,
        _ => return Call::Reply(error(id, -32602, "replayFrom supports only {\"type\": \"start\"}")),
    };
    match session.split_once('/') {
        Some((project, bead)) => Call::Resume {
            id,
            root: session.to_owned(),
            path: format!("/v1/tasks/{}/{}", encode(project), encode(bead)),
            replay,
        },
        None => Call::Reply(missing(id, session)),
    }
}

/// The reply to `session/list` from the reply of `GET /v1/tasks`.
fn sessions(id: Value, tasks: &Value, cwd: Option<&str>) -> Value {
    let sessions: Vec<Value> = (tasks.as_array().into_iter().flatten())
        .filter(|t| cwd.is_none_or(|cwd| t["dir"] == cwd))
        .map(|t| {
            let (bead, title) = (
                t["bead"].as_str().unwrap_or_default(),
                t["title"].as_str().unwrap_or_default(),
            );
            json!({
                "sessionId": format!("{}/{bead}", t["project"].as_str().unwrap_or_default()),
                "cwd": t["dir"],
                "title": if title.is_empty() { bead.to_owned() } else { format!("{bead} {title}") },
                "updatedAt": t["updated_at"],
            })
        })
        .collect();
    result(id, json!({ "sessions": sessions }))
}

/// The root's own history from the reply of `GET /v1/tasks/{project}/{bead}`: the bead's body as the user's message,
/// then each comment, oldest first, as the agent's.
fn snapshot(root: &str, body: &Value) -> Vec<Value> {
    let description = body["description"].as_str().filter(|d| !d.is_empty());
    let text = description.or(body["title"].as_str()).unwrap_or_default();
    let first = json!({ "sessionUpdate": "user_message", "messageId": "body", "content": [plain(text)] });
    let comments = (body["comments"].as_array().into_iter().flatten())
        .enumerate()
        .map(|(i, c)| {
            let (author, at) = (
                c["author"].as_str().unwrap_or_default(),
                c["created_at"].as_str().unwrap_or_default(),
            );
            let text = format!("**{author}** · {at}\n\n{}", c["text"].as_str().unwrap_or_default());
            json!({
                "sessionUpdate": "agent_message", "messageId": format!("c{}", i + 1), "content": [plain(&text)],
                "_meta": { "loop-manager/author": author, "loop-manager/createdAt": at },
            })
        });
    std::iter::once(first)
        .chain(comments)
        .map(|u| update(root, u))
        .collect()
}

/// The ACP view of one root's stream: what has been announced, and the tool calls still waiting for a result.
struct View {
    root: String,
    /// The `session/resume` whose response waits for the end of the replay.
    request: Option<Value>,
    /// The resume asked for the history: without it, what the stream replays is dropped.
    replay: bool,
    /// The announced children, in order, with their last `live`.
    nodes: Vec<(String, bool)>,
    /// Per thread, the last tool call that came without an id.
    calls: HashMap<String, String>,
}

impl View {
    fn new(root: &str, request: Value, replay: bool) -> Self {
        Self {
            root: root.to_owned(),
            request: Some(request),
            replay,
            nodes: Vec::new(),
            calls: HashMap::new(),
        }
    }

    fn child(&self, node: &str) -> String {
        format!("{}#{node}", self.root)
    }

    /// The lines an event of the stream gives; `Err` ends the process with the reason.
    fn updates(&mut self, event: &str, data: &str) -> Result<Vec<Value>, String> {
        let data: Value = serde_json::from_str(data).map_err(|e| format!("a malformed `{event}` event: {e}"))?;
        let quiet = self.request.is_some() && !self.replay;
        let lines = match event {
            "node" => self.node(&data)?,
            "chat" => self.chat(&data)?.into_iter().filter(|_| !quiet).collect(),
            "unavailable" if !quiet => {
                let text = format!(
                    "The transcript of this session is unavailable: {}",
                    super::chat::content(&data["message"])
                );
                let message =
                    json!({ "sessionUpdate": "agent_message", "messageId": "unavailable", "content": [plain(&text)] });
                vec![update(&self.child(field(&data, "thread")?), message)]
            }
            "replayed" => self.replayed(),
            "failed" => {
                return Err(format!(
                    "the daemon failed the stream: {}",
                    super::chat::content(&data["message"])
                ));
            }
            _ => Vec::new(),
        };
        Ok(lines)
    }

    fn node(&mut self, data: &Value) -> Result<Vec<Value>, String> {
        let id = field(data, "id")?;
        let live = data["live"].as_bool().unwrap_or_default();
        let parent = match data["parent"].as_str() {
            Some(parent) => self.child(parent),
            None => self.root.clone(),
        };
        let child = self.child(id);
        let mut announce = json!({ "sessionUpdate": "subagent_update", "sessionId": child, "title": data["title"], "state": state(live) });
        if let Some(workdir) = data["workdir"].as_str() {
            announce["cwd"] = workdir.into();
        }
        let mut lines = vec![update(&parent, announce)];
        match self.nodes.iter_mut().find(|(n, _)| n == id) {
            Some((_, was)) if *was != live => {
                *was = live;
                let mut now = state(live);
                now["sessionUpdate"] = "state_update".into();
                lines.push(update(&child, now));
            }
            Some(_) => {}
            None => self.nodes.push((id.to_owned(), live)),
        }
        Ok(lines)
    }

    fn chat(&mut self, data: &Value) -> Result<Vec<Value>, String> {
        let thread = field(data, "thread")?;
        let n = data["n"].as_u64().ok_or("a `chat` event without `n`")?;
        let entry: ChatEntry =
            serde_json::from_value(data["entry"].clone()).map_err(|e| format!("a malformed chat entry: {e}"))?;
        let message = |kind: &str, prefix: &str, text: &str| json!({ "sessionUpdate": kind, "messageId": format!("{prefix}{n}"), "content": [plain(text)] });
        let update_ = match entry {
            ChatEntry::User { text } => message("user_message", "u", &text),
            ChatEntry::Assistant { text } => message("agent_message", "a", &text),
            ChatEntry::ToolCall { id, name, input } => {
                let id = id.unwrap_or_else(|| {
                    let id = format!("t{n}");
                    self.calls.insert(thread.to_owned(), id.clone());
                    id
                });
                json!({
                    "sessionUpdate": "tool_call_update", "toolCallId": id, "title": name, "name": name, "kind": "other",
                    "status": "in_progress", "rawInput": input,
                })
            }
            ChatEntry::ToolResult { id, output, is_error } => {
                let id = (id.or_else(|| self.calls.remove(thread))).unwrap_or_else(|| format!("t{n}"));
                let status = if is_error == Some(true) { "failed" } else { "completed" };
                let content = json!([{ "type": "content", "content": plain(&super::chat::content(&output)) }]);
                json!({
                    "sessionUpdate": "tool_call_update", "toolCallId": id, "status": status, "rawOutput": output,
                    "content": content,
                })
            }
        };
        Ok(vec![update(&self.child(thread), update_)])
    }

    /// The answer to a `session/resume` still waiting for the end of its replay when the view is dropped.
    fn cancel(&mut self, reason: &str) -> Option<Value> {
        (self.request.take()).map(|id| error(id, -32800, &format!("Request cancelled: {reason}")))
    }

    /// The end of the replay: each session's `replay_complete` when the history was asked for, the root's state, and
    /// the response to the resume.
    fn replayed(&mut self) -> Vec<Value> {
        let Some(request) = self.request.take() else {
            return Vec::new();
        };
        let complete = json!({ "sessionUpdate": "replay_complete" });
        let sessions = (self.nodes.iter().map(|(id, _)| self.child(id))).chain([self.root.clone()]);
        let mut lines: Vec<Value> = match self.replay {
            true => sessions.map(|s| update(&s, complete.clone())).collect(),
            false => Vec::new(),
        };
        let mut idle = state(false);
        idle["sessionUpdate"] = "state_update".into();
        lines.push(update(&self.root, idle));
        lines.push(result(request, json!({})));
        lines
    }
}

fn field<'a>(data: &'a Value, key: &str) -> Result<&'a str, String> {
    data[key].as_str().ok_or_else(|| format!("an event without `{key}`"))
}

/// The state of a session: a live node is running, any other has ended its turn.
fn state(live: bool) -> Value {
    match live {
        true => json!({ "state": "running" }),
        false => json!({ "state": "idle", "stopReason": "end_turn" }),
    }
}

fn plain(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

fn update(session: &str, update: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": "session/update", "params": { "sessionId": session, "update": update } })
}

fn result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn with_data(mut error: Value, data: Value) -> Value {
    error["error"]["data"] = data;
    error
}

/// `-32002`: no session `session`.
fn missing(id: Value, session: &str) -> Value {
    with_data(error(id, -32002, "Resource not found"), json!({ "sessionId": session }))
}

#[cfg(test)]
mod tests;
