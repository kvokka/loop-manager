//! `loop-manager acp` (docs/api/frontends.md, "ACP"): an outside ACP v2 client, the SDK's, on the built binary over
//! stdio, observing, taking over and stopping the runs of a daemon whose harness is the `acp-agent` fixture, and
//! answering their permission requests. No model.

use crate::common::{self, Daemon};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v2::{ConnectionTo, InitializeOutcome, Responder, schema};
use agent_client_protocol::{Agent, Client, Lines, v1};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Instant,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Child,
    sync::{mpsc, oneshot},
};

/// A daemon of the projects `shop` and `lab` whose `build` formula is one `ai` step of an agent on the `acp-agent`
/// fixture running `command` as its tool.
async fn daemon(command: &str) -> Daemon {
    configured(command, "", "", &[]).await
}

/// The daemon of [`daemon`] with `top` before the tables of its config, `args` of the fixture (a TOML array's
/// items) and the files `shop` (a path in the project's directory, its text).
async fn configured(command: &str, top: &str, args: &str, shop: &[(&str, &str)]) -> Daemon {
    let agent = Path::new(common::EXE).with_file_name("acp-agent");
    let config = format!(
        "projects = [\"shop\", \"lab\"]\n{top}[harness.fake]\ncommand = \"{}\"\nargs = [{args}]\n[harness.fake.env]\n\
         ACP_AGENT_COMMAND = \"{command}\"\n",
        agent.display()
    );
    let shop: Vec<(String, String)> = (shop.iter()).map(|(p, t)| ((*p).to_owned(), (*t).to_owned())).collect();
    Daemon::start(move |dir| {
        let projects = dir.join("config/loop-manager");
        let mut files = vec![
            (projects.join("config.toml"), config),
            (projects.join("lab/loop-manager.toml"), String::new()),
            (
                projects.join("shop/agents/dev/agent.toml"),
                "harness = \"fake\"\n".to_owned(),
            ),
            (
                projects.join("shop/formulas/build.toml"),
                "name = \"build\"\n[[step]]\nkind = \"ai\"\nagent = \"dev\"\n".to_owned(),
            ),
            (
                projects.join("shop/agents/dev/prompt.template.md"),
                "Build {{ rig.name }}.".to_owned(),
            ),
        ];
        files.extend(
            shop.into_iter()
                .map(|(path, text)| (projects.join("shop").join(path), text)),
        );
        for (path, text) in files {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
    })
    .await
}

fn project_dir(d: &Daemon, project: &str) -> PathBuf {
    d.dir.path().join("config/loop-manager").join(project)
}

/// A `session/request_permission` received, with the responder that answers it.
type Asked = (
    schema::RequestPermissionRequest,
    Responder<schema::RequestPermissionResponse>,
);

/// One `loop-manager acp` process with the SDK's client on its stdio, initialized; dropped, it goes.
struct Acp {
    cx: ConnectionTo<Agent>,
    /// The `session/update` notifications received, as JSON.
    updates: mpsc::UnboundedReceiver<Value>,
    /// The permission requests received, unanswered.
    asked: mpsc::UnboundedReceiver<Asked>,
    _close: oneshot::Sender<()>,
    _child: Child,
}

fn spawn(d: &Daemon, args: &[&str]) -> Child {
    let mut command = tokio::process::Command::from(common::loop_manager());
    command
        .arg("acp")
        .args(args)
        .env("LOOP_MANAGER_SOCKET", d.socket())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    command.spawn().unwrap()
}

/// The client's side of the child's stdio, a JSON-RPC message per line.
fn lines(child: &mut Child) -> impl agent_client_protocol::ConnectTo<Client> + 'static {
    let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
    let incoming = futures::stream::unfold(stdout, async |mut lines| {
        lines.next_line().await.transpose().map(|line| (line, lines))
    });
    let outgoing = futures::sink::unfold(child.stdin.take().unwrap(), async |mut stdin, line: String| {
        stdin.write_all(format!("{line}\n").as_bytes()).await?;
        stdin.flush().await?;
        Ok::<_, std::io::Error>(stdin)
    });
    Lines::new(Box::pin(outgoing), Box::pin(incoming))
}

impl Acp {
    async fn start(d: &Daemon, args: &[&str]) -> Self {
        let mut child = spawn(d, args);
        let transport = lines(&mut child);
        let (tx, updates) = mpsc::unbounded_channel();
        let (asked_tx, asked) = mpsc::unbounded_channel();
        let (cx_tx, cx_rx) = oneshot::channel();
        let (close, closed) = oneshot::channel::<()>();
        tokio::spawn(
            Client
                .v2()
                .name("test")
                .on_receive_notification(
                    async move |n: schema::UpdateSessionNotification, _: ConnectionTo<Agent>| {
                        let _ = tx.send(serde_json::to_value(&n).unwrap());
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .on_receive_request(
                    async move |request: schema::RequestPermissionRequest,
                                responder: Responder<schema::RequestPermissionResponse>,
                                _: ConnectionTo<Agent>| {
                        let _ = asked_tx.send((request, responder));
                        Ok(())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_with(transport, async move |cx: ConnectionTo<Agent>| {
                    let _ = cx_tx.send(cx.clone());
                    let _ = closed.await;
                    Ok(())
                }),
        );
        let cx = cx_rx.await.unwrap();
        let me = schema::Implementation::new("test", "0");
        let answer = cx
            .send_initialize(schema::InitializeRequest::new(ProtocolVersion::V2, me))
            .block_task()
            .await
            .unwrap();
        let InitializeOutcome::Initialized(answer) = answer else {
            panic!("not initialized on v2")
        };
        assert_eq!(answer.info.name, "loop-manager");
        assert!(answer.capabilities.session.is_some());
        Self {
            cx,
            updates,
            asked,
            _close: close,
            _child: child,
        }
    }

    async fn list(&self, cwd: &Path) -> Vec<Value> {
        let request: schema::ListSessionsRequest = serde_json::from_value(json!({ "cwd": cwd })).unwrap();
        let answer = self.cx.send_request(request).block_task().await.unwrap();
        let answer = serde_json::to_value(answer).unwrap();
        answer["sessions"].as_array().unwrap().clone()
    }

    /// `session/resume` of `run`, replaying from the start when `replay`; the updates that came before the answer.
    async fn resume(&mut self, run: &str, replay: bool) -> Result<Vec<Value>, agent_client_protocol::Error> {
        let mut request = json!({ "sessionId": run, "cwd": "/" });
        if replay {
            request["replayFrom"] = json!({ "type": "start" });
        }
        let request: schema::ResumeSessionRequest = serde_json::from_value(request).unwrap();
        self.cx.send_request(request).block_task().await?;
        let mut before = Vec::new();
        while let Ok(update) = self.updates.try_recv() {
            before.push(update);
        }
        Ok(before)
    }

    /// `session/prompt` of `text` to `run`, with the idempotency key `key`: the answer.
    async fn prompt(&self, run: &str, text: &str, key: Option<&str>) -> Result<Value, agent_client_protocol::Error> {
        let mut request = json!({ "sessionId": run, "prompt": [{ "type": "text", "text": text }] });
        if let Some(key) = key {
            request["idempotencyKey"] = json!(key);
        }
        let request: schema::PromptRequest = serde_json::from_value(request).unwrap();
        let answer = self.cx.send_request(request).block_task().await?;
        Ok(serde_json::to_value(answer).unwrap())
    }

    /// The command `text` to `run`, answered: its echo and the notice of the daemon's answer, which comes last.
    async fn command(&mut self, run: &str, text: &str, key: Option<&str>) -> (Value, Vec<Value>) {
        let answer = self.prompt(run, text, key).await.unwrap();
        let reply = format!("{}-reply", answer["messageId"].as_str().unwrap());
        let seen = self.until(|u| u["update"]["messageId"] == reply.as_str()).await;
        (answer, seen)
    }

    /// The updates up to the first that `stop` takes, which comes last.
    async fn until(&mut self, stop: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut seen = Vec::new();
        loop {
            let update = tokio::time::timeout(common::WAIT, self.updates.recv()).await;
            let update = update.unwrap_or_else(|_| panic!("{seen:#?}")).unwrap();
            let done = stop(&update);
            seen.push(update);
            if done {
                return seen;
            }
        }
    }
}

/// The text of an update's content, of a message chunk or a tool call's content chunk.
fn text(update: &Value) -> &str {
    let content = &update["update"]["content"];
    content["text"]
        .as_str()
        .or(content["content"]["text"].as_str())
        .unwrap_or_default()
}

/// The names of the commands an `available_commands_update` lists; none for another update.
fn commands(update: &Value) -> Vec<&str> {
    let commands = update["update"]["availableCommands"].as_array().into_iter().flatten();
    commands.map(|c| c["name"].as_str().unwrap()).collect()
}

/// The control of `run` (`lm` or `manual`) and its attempt, from its view.
async fn control(d: &Daemon, run: &str) -> (String, u64) {
    let view = d.get(&format!("/v1/runs/{run}")).await;
    (
        view["control"].as_str().unwrap().to_owned(),
        view["attempt"].as_u64().unwrap(),
    )
}

fn printed(update: &Value) -> bool {
    update["update"]["sessionUpdate"] == "tool_call_content_chunk" && text(update) == "printed-before-the-hang\n"
}

fn state(update: &Value) -> &str {
    match update["update"]["sessionUpdate"] == "state_update" {
        true => update["update"]["state"].as_str().unwrap(),
        false => "",
    }
}

const HANG: &str = "echo printed-before-the-hang; sleep 600";

#[tokio::test(flavor = "multi_thread")]
async fn session_list_gives_the_runs_of_the_project_of_the_cwd_or_of_the_project_named() {
    let d = daemon(&format!("{} finish pass --summary built", common::EXE)).await;
    let run = d.create("build", None).await;
    d.until_state(&run, "passed").await;
    let acp = Acp::start(&d, &[]).await;
    let sessions = acp.list(&project_dir(&d, "shop").join("web")).await;
    assert_eq!(sessions.len(), 1, "{sessions:#?}");
    let session = &sessions[0];
    assert_eq!(session["sessionId"], run.as_str());
    assert_eq!(session["title"], "shop.build · 1 · passed");
    assert!(session["cwd"].as_str().unwrap().starts_with('/'), "{session}");
    assert!(session["updatedAt"].is_string(), "{session}");
    assert_eq!(acp.list(&project_dir(&d, "lab")).await, Vec::<Value>::new());
    // A cwd of no project lists every project.
    assert_eq!(acp.list(Path::new("/")).await.len(), 1);
    // `--project` wins over the cwd.
    let lab = Acp::start(&d, &["--project", "lab"]).await;
    assert_eq!(lab.list(&project_dir(&d, "shop")).await, Vec::<Value>::new());
    let shop = Acp::start(&d, &["--project", "shop"]).await;
    assert_eq!(shop.list(&project_dir(&d, "lab")).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn clients_replay_a_running_run_follow_it_live_and_leave_it_running_when_they_go() {
    let d = daemon(HANG).await;
    let run = d.create("build", None).await;
    let mut first = Acp::start(&d, &[]).await;
    // The run may not have written a record yet: what the replay lacks follows live, up to the hanging tool's output.
    // Live updates can already be queued behind the replay's state when the answer arrives.
    let replayed = first.resume(&run, true).await.unwrap();
    assert!(replayed.iter().any(|u| state(u) == "running"), "{replayed:#?}");
    if !replayed.iter().any(printed) {
        first.until(printed).await;
    }
    // A client attaching now gets it all in the replay: the history from its first record, then the current state.
    let mut second = Acp::start(&d, &[]).await;
    let again = second.resume(&run, true).await.unwrap();
    assert!(again.iter().all(|u| u["sessionId"] == run.as_str()), "{again:#?}");
    assert!(text(&again[0]).starts_with("— step "), "{again:#?}");
    let [.., now, commands_now] = again.as_slice() else {
        panic!("{again:#?}")
    };
    assert_eq!(state(now), "running", "{again:#?}");
    assert_eq!(commands(commands_now), ["lm-takeover", "lm-continue"], "{again:#?}");
    let texts: Vec<&str> = again.iter().map(text).collect();
    assert!(texts.contains(&"— attempt 1: new_session on fake —\n"), "{texts:#?}");
    assert!(texts.contains(&"Build web."), "{texts:#?}");
    assert!(texts.iter().any(|t| t.starts_with("Echo: Build web.")), "{texts:#?}");
    assert!(again.iter().any(printed), "{again:#?}");
    // A client without a replay gets only the current state, then what comes.
    let mut live = Acp::start(&d, &[]).await;
    let now = live.resume(&run, false).await.unwrap();
    assert_eq!(now.iter().map(state).collect::<Vec<_>>(), ["running", ""], "{now:#?}");
    // Closing a session and a client leaves the run running.
    let close: schema::CloseSessionRequest = serde_json::from_value(json!({ "sessionId": run })).unwrap();
    first.cx.send_request(close).block_task().await.unwrap();
    drop(first);
    assert_eq!(d.get(&format!("/v1/runs/{run}")).await["state"], "running");
    d.post(&format!("/v1/runs/{run}/cancel"), json!({}), None)
        .await
        .unwrap();
    for client in [&mut second, &mut live] {
        client.until(|u| text(u).starts_with("— run cancelled")).await;
        // The run is over: the last update is its idle state.
        client.until(|u| state(u) == "idle").await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_session_a_prompt_an_unknown_run_and_a_v1_client_get_their_errors() {
    let d = Daemon::start(|_| {}).await;
    let mut acp = Acp::start(&d, &[]).await;
    let new: schema::NewSessionRequest = serde_json::from_value(json!({ "cwd": "/" })).unwrap();
    let error = acp.cx.send_request(new).block_task().await.unwrap_err();
    assert_eq!(
        (i32::from(error.code), error.message.as_str()),
        (-32601, "runs are started with `loop-manager run start`")
    );
    // An instruction and a command need the attempt of a session open here.
    let prompt: schema::PromptRequest =
        serde_json::from_value(json!({ "sessionId": "r1", "prompt": [{ "type": "text", "text": "hi" }] })).unwrap();
    let error = acp.cx.send_request(prompt).block_task().await.unwrap_err();
    assert_eq!(i32::from(error.code), -32600, "{error:?}");
    assert!(error.message.contains("`session/resume` opens it"), "{error:?}");
    let error = acp.prompt("r1", "/lm-takeover", None).await.unwrap_err();
    assert_eq!(i32::from(error.code), -32600, "{error:?}");
    // A command with words it does not take is no instruction.
    let error = acp.prompt("r1", "/lm-continue later", None).await.unwrap_err();
    assert_eq!(i32::from(error.code), -32602, "{error:?}");
    assert!(error.message.contains("`/lm-continue withdraw`"), "{error:?}");
    let error = acp.resume("no-such-run", true).await.unwrap_err();
    assert_eq!(i32::from(error.code), -32002, "{error:?}");
    // A client of protocol version 1 only.
    let mut child = spawn(&d, &[]);
    let answer = Client
        .v1()
        .name("old")
        .connect_with(lines(&mut child), async |cx: v1::ConnectionTo<Agent>| {
            Ok(cx
                .send_request(v1::schema::InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await)
        })
        .await
        .unwrap();
    assert_eq!(i32::from(answer.unwrap_err().code), -32022);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_follows_a_run_across_a_restart_of_the_daemon_with_no_hole_and_no_repeat() {
    let d = daemon(HANG).await;
    let run = d.create("build", None).await;
    let mut acp = Acp::start(&d, &[]).await;
    let mut seen = acp.resume(&run, true).await.unwrap();
    if !seen.iter().any(printed) {
        seen.extend(acp.until(printed).await);
    }
    d.post("/v1/daemon/restart?timeout_s=1", json!({}), None).await.unwrap();
    seen.extend(acp.until(|u| text(u).starts_with("— attempt 2")).await);
    d.post(&format!("/v1/runs/{run}/cancel"), json!({}), None)
        .await
        .unwrap();
    seen.extend(acp.until(|u| text(u).starts_with("— run cancelled")).await);
    let notices: Vec<&str> = (seen.iter())
        .filter_map(|u| u["update"]["messageId"].as_str().filter(|id| id.starts_with("lm-")))
        .collect();
    let numbers: Vec<u64> = notices.iter().map(|id| id[3..].parse().unwrap()).collect();
    assert!(numbers.is_sorted_by(|a, b| a < b), "{seen:#?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_take_a_run_over_and_hand_it_back_in_the_same_attempt_and_another_client_sees_it() {
    let d = daemon(HANG).await;
    let run = d.create("build", None).await;
    let mut first = Acp::start(&d, &[]).await;
    let mut replayed = first.resume(&run, true).await.unwrap();
    // The commands come after the state, with and without a replay; the replay can hold the agent's own states too.
    let at = (replayed.iter().position(|u| !commands(u).is_empty())).unwrap_or_else(|| panic!("{replayed:#?}"));
    assert_eq!(state(&replayed[at - 1]), "running", "{replayed:#?}");
    assert_eq!(commands(&replayed[at]), ["lm-takeover", "lm-continue"], "{replayed:#?}");
    // The session has shown attempt 1 once the tool has printed.
    if !replayed.iter().any(printed) {
        replayed.extend(first.until(printed).await);
    }
    let mut second = Acp::start(&d, &[]).await;
    let now = second.resume(&run, false).await.unwrap();
    assert_eq!(commands(&now[1]), ["lm-takeover", "lm-continue"], "{now:#?}");

    let (answer, seen) = first.command(&run, "/lm-takeover", Some("k1")).await;
    let echo = seen.iter().find(|u| u["update"]["messageId"] == answer["messageId"]);
    assert_eq!(echo.map(text), Some("/lm-takeover"), "{seen:#?}");
    assert!(
        text(seen.last().unwrap()).starts_with("— /lm-takeover accepted"),
        "{seen:#?}"
    );
    assert_eq!(control(&d, &run).await, ("manual".to_owned(), 1));
    second.until(|u| text(u) == "— taken over by an operator —\n").await;
    // The same key again is the same submission.
    let (again, _) = first.command(&run, "/lm-takeover", Some("k1")).await;
    assert_eq!(again["messageId"], answer["messageId"]);

    // A repeat of a key gets the daemon's first answer and changes nothing.
    for _ in 0..2 {
        let (_, seen) = first.command(&run, "/lm-continue", Some("k2")).await;
        let accepted = text(seen.last().unwrap());
        assert!(accepted.starts_with("— /lm-continue accepted"), "{seen:#?}");
        assert_eq!(control(&d, &run).await, ("lm".to_owned(), 1));
    }
    assert_eq!(d.get(&format!("/v1/runs/{run}")).await["state"], "running");
    second.until(|u| text(u) == "— handed back to loop-manager —\n").await;
    let (_, seen) = second.command(&run, "/lm-continue", None).await;
    let refused = text(seen.last().unwrap());
    assert!(
        refused.starts_with("— /lm-continue refused: the run is not taken over"),
        "{seen:#?}"
    );

    // The clients go; the run stays manual.
    second.command(&run, "/lm-takeover", None).await;
    drop((first, second));
    assert_eq!(control(&d, &run).await, ("manual".to_owned(), 1));
    d.post(&format!("/v1/runs/{run}/cancel"), json!({}), None)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_from_the_screen_of_an_earlier_attempt_is_refused_and_changes_nothing() {
    let d = daemon(HANG).await;
    let run = d.create("build", None).await;
    let mut acp = Acp::start(&d, &[]).await;
    let seen = acp.resume(&run, true).await.unwrap();
    if !seen.iter().any(printed) {
        acp.until(printed).await;
    }
    // Closed, the session still shows attempt 1, while a restart of the daemon resumes the run in attempt 2.
    let close: schema::CloseSessionRequest = serde_json::from_value(json!({ "sessionId": run })).unwrap();
    acp.cx.send_request(close).block_task().await.unwrap();
    d.post("/v1/daemon/restart?timeout_s=1", json!({}), None).await.unwrap();
    let deadline = Instant::now() + common::WAIT;
    loop {
        let view = d.client.request("GET", &format!("/v1/runs/{run}"), None, None).await;
        if view.is_ok_and(|view| view["attempt"] == 2) {
            break;
        }
        assert!(Instant::now() < deadline, "{run} stays in attempt 1");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let (_, seen) = acp.command(&run, "/lm-takeover", None).await;
    let refused = text(seen.last().unwrap());
    assert!(
        refused.starts_with("— /lm-takeover refused: the run is past attempt 1"),
        "{seen:#?}"
    );
    assert_eq!(control(&d, &run).await, ("lm".to_owned(), 2));
    // Opened again, the session shows attempt 2, and the command goes through.
    acp.resume(&run, false).await.unwrap();
    let (_, seen) = acp.command(&run, "/lm-takeover", None).await;
    assert!(
        text(seen.last().unwrap()).starts_with("— /lm-takeover accepted"),
        "{seen:#?}"
    );
    assert_eq!(control(&d, &run).await, ("manual".to_owned(), 2));
    d.post(&format!("/v1/runs/{run}/cancel"), json!({}), None)
        .await
        .unwrap();
}

/// Opens the session of `run` with a replay once its agent's session is open: the session shows attempt 1, the attempt
/// its prompts carry.
async fn shown(d: &Daemon, acp: &mut Acp, run: &str) {
    let deadline = Instant::now() + common::WAIT;
    loop {
        let view = d.get(&format!("/v1/runs/{run}")).await;
        if view["attempts"][0]["session"].is_object() {
            break;
        }
        assert!(Instant::now() < deadline, "no session: {view:#}");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let seen = acp.resume(run, true).await.unwrap();
    assert!(seen.iter().any(|u| text(u).starts_with("— attempt 1")), "{seen:#?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plain_prompt_takes_the_run_over_and_reaches_the_agent_once_whatever_the_repeats() {
    // The first turn's tool waits for the file `go`; the turn of the instruction finds it there.
    let gate = tempfile::tempdir().unwrap();
    let go = gate.path().join("go");
    let d = daemon(&format!("while [ ! -e {} ]; do sleep 0.05; done", go.display())).await;
    let run = d.create("build", None).await;
    let mut acp = Acp::start(&d, &[]).await;
    shown(&d, &mut acp, &run).await;
    let (answer, seen) = acp.command(&run, "check the tests too", Some("i1")).await;
    let id = answer["messageId"].as_str().unwrap().to_owned();
    assert!(id.starts_with("prompt-"), "{answer}");
    assert!(
        text(seen.last().unwrap()).starts_with("— the instruction is taken: loop-manager holds it"),
        "{seen:#?}"
    );
    assert_eq!(control(&d, &run).await, ("manual".to_owned(), 1));
    // The same key again is the same submission, and no second instruction.
    let (again, repeated) = acp.command(&run, "check the tests too", Some("i1")).await;
    assert_eq!(again["messageId"], answer["messageId"]);
    let view = d.get(&format!("/v1/runs/{run}")).await;
    let unsent = view["manual"]["unsent"].as_array().unwrap();
    assert_eq!(unsent.len(), 1, "{view:#}");
    assert_eq!(
        (&unsent[0]["text"], &unsent[0]["state"]),
        (&json!("check the tests too"), &json!("queued"))
    );

    fs::write(&go, "").unwrap();
    let mut after = acp.until(|u| text(u) == "Echo: check the tests too").await;
    after.extend(acp.until(|u| state(u) == "idle").await);
    let all: Vec<&Value> = seen.iter().chain(&repeated).chain(&after).collect();
    let echoes = all.iter().filter(|u| text(u) == "Echo: check the tests too").count();
    assert_eq!(echoes, 1, "{all:#?}");
    // The user's message is the history's record of the instruction, under the id the answer gave.
    let messages: Vec<&&Value> = (all.iter())
        .filter(|u| u["update"]["sessionUpdate"] == "user_message_chunk" && text(u) == "check the tests too")
        .collect();
    assert_eq!(messages.len(), 1, "{all:#?}");
    assert_eq!(messages[0]["update"]["messageId"], id.as_str());
    let view = d.get(&format!("/v1/runs/{run}")).await;
    assert_eq!(view["manual"]["unsent"], json!([]), "{view:#}");
    d.post(&format!("/v1/runs/{run}/cancel"), json!({}), None)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn lm_continue_with_an_instruction_unsent_needs_a_choice_and_withdraw_hands_back_now() {
    let d = daemon(HANG).await;
    let run = d.create("build", None).await;
    let mut acp = Acp::start(&d, &[]).await;
    shown(&d, &mut acp, &run).await;
    acp.command(&run, "never mind the docs", None).await;
    let (_, seen) = acp.command(&run, "/lm-continue", None).await;
    let refused = text(seen.last().unwrap());
    assert!(
        refused.starts_with("— /lm-continue refused: 1 instruction(s) the agent has not taken in yet")
            && refused.contains("/lm-continue send")
            && refused.contains("/lm-continue withdraw"),
        "{seen:#?}"
    );
    assert_eq!(control(&d, &run).await, ("manual".to_owned(), 1));
    let (_, seen) = acp.command(&run, "/lm-continue withdraw", Some("w1")).await;
    assert!(
        text(seen.last().unwrap()).starts_with("— /lm-continue withdraw accepted"),
        "{seen:#?}"
    );
    assert_eq!(control(&d, &run).await, ("lm".to_owned(), 1));
    // The history's record of the hand-back may come before or after the answer's notice.
    let back = |u: &Value| text(u) == "— handed back to loop-manager —\n";
    if !seen.iter().any(back) {
        acp.until(back).await;
    }
    d.post(&format!("/v1/runs/{run}/cancel"), json!({}), None)
        .await
        .unwrap();
}

/// The updates of `seen` that tell of the run's records: not the state and the commands each resume sends.
fn records(seen: &[Value]) -> Vec<&Value> {
    let kind = |u: &&Value| u["update"]["sessionUpdate"].clone();
    (seen.iter())
        .filter(|u| kind(u) != "state_update" && kind(u) != "available_commands_update")
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn clients_resuming_while_records_are_written_get_each_record_once_in_order() {
    let d = daemon("for i in $$(seq 1 300); do echo line-$$i; sleep 0.002; done; sleep 600").await;
    let run = d.create("build", None).await;
    let last = |u: &Value| text(u).contains("line-300\n");
    // Clients attach one after another while the command writes: each one's replay meets the live records somewhere.
    let mut clients = Vec::new();
    for _ in 0..4 {
        let mut acp = Acp::start(&d, &[]).await;
        let seen = acp.resume(&run, true).await.unwrap();
        clients.push((acp, seen));
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
    for (acp, seen) in &mut clients {
        if !seen.iter().any(last) {
            seen.extend(acp.until(last).await);
        }
    }
    // A client attaching once it is all written gets it in the replay alone: what every other one has seen.
    let mut late = Acp::start(&d, &[]).await;
    let all = late.resume(&run, true).await.unwrap();
    assert!(all.iter().any(last), "{all:#?}");
    for (_, seen) in &clients {
        assert_eq!(records(seen), records(&all));
    }
    // And so on to the run's end.
    d.post(&format!("/v1/runs/{run}/cancel"), json!({}), None)
        .await
        .unwrap();
    clients.push((late, all));
    let mut ends = Vec::new();
    for (acp, seen) in &mut clients {
        seen.extend(acp.until(|u| state(u) == "idle").await);
        ends.push(seen.clone());
    }
    for seen in &ends {
        assert!(seen.iter().any(|u| text(u).starts_with("— run cancelled")), "{seen:#?}");
        assert_eq!(records(seen), records(&ends[ends.len() - 1]));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn session_cancel_stops_the_turn_of_the_run_and_takes_it_over() {
    let d = daemon(HANG).await;
    let run = d.create("build", None).await;
    let mut acp = Acp::start(&d, &[]).await;
    let seen = acp.resume(&run, true).await.unwrap();
    if !seen.iter().any(printed) {
        acp.until(printed).await;
    }
    let cancel = schema::CancelSessionNotification::new(run.clone());
    acp.cx.send_notification(cancel.clone()).unwrap();
    // The answer and the history's record of the Stop, whichever first, then the turn's end.
    let seen = acp.until(|u| state(u) == "idle").await;
    let texts: Vec<&str> = seen.iter().map(text).collect();
    assert!(
        texts.contains(&"— Stop accepted: the run is taken over until /lm-continue —\n"),
        "{seen:#?}"
    );
    assert!(
        texts.contains(&"— Stop sent to the agent: the turn ends when the harness stops it —\n"),
        "{seen:#?}"
    );
    assert_eq!(seen.last().unwrap()["update"]["stopReason"], "cancelled", "{seen:#?}");
    let deadline = Instant::now() + common::WAIT;
    loop {
        let view = d.get(&format!("/v1/runs/{run}")).await;
        assert_eq!(
            (&view["control"], &view["state"]),
            (&json!("manual"), &json!("running"))
        );
        if view["manual"]["stop"].is_null() {
            break;
        }
        assert_eq!(view["manual"]["stop"], "pending");
        assert!(Instant::now() < deadline, "the stop stays pending: {view:#}");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    acp.cx.send_notification(cancel).unwrap();
    acp.until(|u| text(u).starts_with("— Stop: nothing to stop, no turn is in flight"))
        .await;
    d.post(&format!("/v1/runs/{run}/cancel"), json!({}), None)
        .await
        .unwrap();
}

/// A daemon on `acp-agent --permission` running `command` once allowed, whose run is held after a turn that ended
/// without an outcome: no default policy, and a hook on that event that never answers.
async fn held(command: &str) -> Daemon {
    let hold = "name = \"hold\"\nversion = \"0.1.0\"\n[[hook]]\non = \"run.turn_ended_without_outcome\"\n\
                command = [\"sleep\", \"600\"]\ntimeout = \"300s\"\n";
    let shop = [
        (
            "loop-manager.toml",
            "[[addon]]\npath = \"hold\"\n[settings]\naddons = [{ name = \"hold\" }]\n[rigs.web]\npath = \"web\"\n",
        ),
        ("hold/addon.toml", hold),
    ];
    let top = "builtin = { default-policy = { enabled = false } }\n";
    configured(command, top, "\"--permission\"", &shop).await
}

/// The next permission request `acp` receives.
async fn asked(acp: &mut Acp) -> Asked {
    let asked = tokio::time::timeout(common::WAIT, acp.asked.recv()).await;
    asked.expect("no permission request").unwrap()
}

fn outcome(outcome: Value) -> schema::RequestPermissionResponse {
    serde_json::from_value(json!({ "outcome": outcome })).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_permission_request_reaches_no_client_under_loop_manager_and_every_client_of_a_manual_run() {
    let out = tempfile::tempdir().unwrap();
    let ran = out.path().join("ran");
    let command = format!("touch {}; sleep 600", ran.display());
    let d = held(&command).await;
    let run = d.create("build", None).await;
    let mut first = Acp::start(&d, &[]).await;
    // Under loop-manager the policy declines the first turn's request: the client sees it in the history only.
    let mut seen = first.resume(&run, true).await.unwrap();
    if !seen.iter().any(|u| state(u) == "idle") {
        seen.extend(first.until(|u| state(u) == "idle").await);
    }
    let texts: Vec<&str> = seen.iter().map(text).collect();
    let line = |what: &str| format!("— permission {what} —\n");
    for notice in [
        format!("asked: {command}"),
        format!("{command}: declined by loop-manager"),
    ] {
        assert!(texts.contains(&line(&notice).as_str()), "{seen:#?}");
    }
    assert!(
        first.asked.try_recv().is_err(),
        "a request of a run under loop-manager reached the client"
    );
    assert!(!ran.exists());

    // An instruction takes the run over, and the request of its turn waits for an operator: it is put to the client.
    first.command(&run, "try again", None).await;
    let (request, first_asked) = asked(&mut first).await;
    let request = serde_json::to_value(request).unwrap();
    assert_eq!(
        (&request["sessionId"], &request["title"]),
        (&json!(run), &json!(command))
    );
    assert_eq!(
        request["options"],
        json!([{ "optionId": "allow", "name": "Allow", "kind": "allow_once" },
            { "optionId": "reject", "name": "Reject", "kind": "reject_once" }])
    );
    let call = &request["subject"]["toolCall"];
    assert!(call["toolCallId"].as_str().unwrap().starts_with("1-"), "{request:#}");
    assert_eq!(call["rawInput"]["command"], command.as_str());
    // A client that opens the session now gets it at once, with a replay or without.
    let mut second = Acp::start(&d, &[]).await;
    second.resume(&run, false).await.unwrap();
    let (_, second_asked) = asked(&mut second).await;
    let mut third = Acp::start(&d, &[]).await;
    third.resume(&run, true).await.unwrap();
    let (_, third_asked) = asked(&mut third).await;

    // One answer counts: the run gets it, and the other clients' requests are cancelled.
    second_asked
        .respond(outcome(json!({ "outcome": "selected", "optionId": "allow" })))
        .unwrap();
    for responder in [first_asked, third_asked] {
        let cancelled = tokio::time::timeout(common::WAIT, responder.cancellation().cancelled()).await;
        cancelled.expect("the request answered elsewhere is not cancelled");
        responder.respond(outcome(json!({ "outcome": "cancelled" }))).unwrap();
    }
    let allowed = line(&format!("{command}: Allow, by an operator"));
    for client in [&mut first, &mut second, &mut third] {
        client.until(|u| text(u) == allowed).await;
        assert!(client.asked.try_recv().is_err(), "a request reached a client twice");
    }
    let deadline = Instant::now() + common::WAIT;
    while !ran.exists() {
        assert!(Instant::now() < deadline, "the allowed command did not run");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let view = d.get(&format!("/v1/runs/{run}")).await;
    assert_eq!(view["permissions"], json!([]), "{view:#}");
    // The history has the one answer.
    let mut late = Acp::start(&d, &[]).await;
    let all = late.resume(&run, true).await.unwrap();
    let answers = all
        .iter()
        .filter(|u| text(u).starts_with(&format!("— permission {command}: ")));
    assert_eq!(answers.count(), 2, "{all:#?}");
    assert!(late.asked.try_recv().is_err());
    d.post(&format!("/v1/runs/{run}/cancel"), json!({}), None)
        .await
        .unwrap();
}
