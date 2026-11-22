//! The task view on a real `loop-manager daemon` with a br tracker (docs/api/frontends.md, Tasks): the list, the body
//! of a root bead with its comments, and the live tree of a session of the real `claude` on the fake Anthropic API.

use super::tracker::br_in;
use crate::common::{self, Daemon};
use loop_manager_front::client::ClientError;
use loop_manager_testkit::anthropic::{Provider, Reply};
use serde_json::{Value, json};
use std::{fs, path::Path, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};

/// A formula of one step on a `bead` input: `step` is the TOML of the step.
fn fix(step: &str) -> String {
    format!("name = \"fix\"\n[inputs]\nbead = {{ type = \"task\", required = true }}\n[[step]]\n{step}")
}

/// br in the project as `alice`, filing beads of the rig `web`.
pub(super) fn br(project: &Path, args: &[&str]) -> String {
    br_in(project, Some("w"), args)
}

/// Starts `fix` on `bead` in the rig `web`.
async fn start(d: &Daemon, bead: &str) -> String {
    let body = json!({ "project": "shop", "rig": "web", "formula": "fix", "inputs": { "bead": bead } });
    let reply = d.post("/v1/runs", body, None).await.unwrap();
    reply["run"].as_str().unwrap().to_owned()
}

async fn not_found(d: &Daemon, path: &str) {
    match d.client.request("GET", path, None, None).await {
        Err(ClientError::Api { status: 404, error }) => assert_eq!(error["code"], "not_found", "{path}"),
        other => panic!("{path}: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_root_bead_with_an_exec_run_under_it_is_a_task_with_its_body_and_comments() {
    let d = Daemon::start(|dir| {
        let shop = dir.join("config/loop-manager/shop");
        fs::write(
            shop.join("formulas/fix.toml"),
            fix("kind = \"exec\"\ncommand = [\"true\"]\ntimeout = \"10s\"\n"),
        )
        .unwrap();
        br(&shop, &["init", "--prefix", "t", "-q"]);
    })
    .await;
    let shop = d.dir.path().join("config/loop-manager/shop");
    let root = br(&shop, &["create", "Ship the cart", "-d", "The whole cart.", "--silent"]);
    let child = br(
        &shop,
        &[
            "create",
            "Fix the total",
            "--deps",
            &format!("parent-child:{root}"),
            "--silent",
        ],
    );
    let idle = br(&shop, &["create", "Nobody works here", "--silent"]);
    br(&shop, &["comments", "add", &root, "First note."]);
    br(&shop, &["comments", "add", &root, "Second note."]);
    for bead in [&child, &root] {
        let run = start(&d, bead).await;
        d.until_state(&run, "passed").await;
    }
    let plain = d.create("check", None).await;
    d.until_state(&plain, "passed").await;

    let listed = d.get("/v1/tasks").await;
    let task = json!({
        "project": "shop", "bead": root, "title": "Ship the cart", "dir": shop.to_string_lossy(), "live": false,
    });
    let tasks = listed.as_array().unwrap();
    assert_eq!(tasks.len(), 1, "{listed}");
    let mut got = tasks[0].clone();
    assert!(got["updated_at"].is_string(), "{got}");
    got.as_object_mut().unwrap().remove("updated_at");
    assert_eq!(got, task);
    assert_eq!(d.get("/v1/tasks?project=shop").await, listed);

    let body = d.get(&format!("/v1/tasks/shop/{root}")).await;
    assert_eq!(
        (&body["title"], &body["description"], &body["dir"]),
        (&json!("Ship the cart"), &json!("The whole cart."), &task["dir"]),
        "{body}"
    );
    let comments = body["comments"].as_array().unwrap();
    let texts: Vec<_> = comments.iter().map(|c| c["text"].as_str().unwrap()).collect();
    assert_eq!(texts, ["First note.", "Second note."], "{body}");
    for comment in comments {
        assert_eq!(comment["author"], "alice", "{comment}");
        assert!(comment["created_at"].is_string(), "{comment}");
    }

    not_found(&d, &format!("/v1/tasks/nope/{root}")).await;
    not_found(&d, &format!("/v1/tasks/shop/{idle}")).await;
    not_found(&d, &format!("/v1/tasks/shop/{child}")).await;

    let out = common::loop_manager()
        .args(["task", "list", "--json"])
        .env("LOOP_MANAGER_SOCKET", d.socket())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), listed);
}

/// A daemon whose `fix` formula is a prompt run of a claude agent on the fake `provider`, with a br tracker.
pub(super) async fn claude(provider: &Provider) -> Daemon {
    let base_url = provider.base_url();
    Daemon::start(|dir| {
        let shop = dir.join("config/loop-manager/shop");
        let files = [
            (
                dir.join("config/loop-manager/config.toml"),
                "projects = [\"shop\"]\n[harness.claude]\n".to_owned(),
            ),
            (
                shop.join("agents/dev/agent.toml"),
                format!(
                    "harness = \"claude\"\nmodel = \"claude-haiku-5-5\"\ninherit_host = false\n\
                     [env]\nANTHROPIC_BASE_URL = \"{base_url}\"\nANTHROPIC_API_KEY = \"fake\"\n\
                     CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC = \"1\"\nCLAUDE_CODE_MAX_RETRIES = \"0\"\n"
                ),
            ),
            (
                shop.join("agents/dev/prompt.template.md"),
                "Build {{ rig.name }}.".to_owned(),
            ),
            (shop.join("formulas/fix.toml"), fix("kind = \"ai\"\nagent = \"dev\"\n")),
        ];
        for (path, text) in files {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        br(&shop, &["init", "--prefix", "t", "-q"]);
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn task_watch_replays_a_child_bead_session_and_follows_its_running_tool() {
    let provider = Provider::start(vec![Reply::Bash(super::chat::command()), Reply::Text("Done.".into())]).await;
    let d = claude(&provider).await;
    let shop = d.dir.path().join("config/loop-manager/shop");
    let epic = br(&shop, &["create", "Ship the cart", "-t", "epic", "--silent"]);
    let under = format!("parent-child:{epic}");
    let bead = br(&shop, &["create", "Fix the total", "--deps", &under, "--silent"]);
    let sibling = br(&shop, &["create", "Fix the tax", "--deps", &under, "--silent"]);
    let run = start(&d, &bead).await;
    let web = shop.join("web");
    tokio::time::timeout(common::WAIT, async {
        while !web.join("chat-started").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the tool started before the watch");

    let mut watch = tokio::process::Command::from(common::loop_manager())
        .kill_on_drop(true)
        .args(["task", "watch", "shop", &epic, "--json"])
        .env("LOOP_MANAGER_SOCKET", d.socket())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(watch.stdout.take().unwrap()).lines();
    let thread = format!("t:{run}.1");
    // Every event's data, the index of `replayed` (`{}`) and that of the first event after the release.
    let (mut events, mut replayed, mut released): (Vec<Value>, _, _) = (Vec::new(), None, None);
    tokio::time::timeout(common::WAIT, async {
        while let Some(line) = lines.next_line().await.unwrap() {
            let event: Value = serde_json::from_str(&line).unwrap();
            let entry = &event["entry"];
            if event == json!({}) {
                replayed = Some(events.len());
            } else if entry["type"] == "tool_call" && released.is_none() {
                fs::write(web.join("chat-release"), "").unwrap();
                released = Some(events.len() + 1);
            }
            let done = entry["type"] == "tool_result";
            events.push(event);
            if done {
                break;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no tool result in time: {events:?}"));
    drop(watch);

    let (replayed, released) = (replayed.expect("replayed"), released.expect("a tool call"));
    let nodes: Vec<_> = events.iter().filter(|e| e.get("kind").is_some()).collect();
    let summary: Vec<_> = (nodes.iter().take(3))
        .map(|n| {
            (
                n["id"].as_str().unwrap(),
                n["parent"].clone(),
                n["kind"].as_str().unwrap(),
            )
        })
        .collect();
    let (bead_node, round) = (format!("b:{bead}"), format!("r:{run}.1"));
    assert_eq!(
        summary,
        [
            (bead_node.as_str(), Value::Null, "bead"),
            (round.as_str(), json!(bead_node), "round"),
            (thread.as_str(), json!(round), "thread"),
        ],
        "{events:?}"
    );
    assert_eq!(nodes[0]["title"], format!("{bead} Fix the total"));
    assert!(nodes[2]["workdir"].is_string(), "{}", nodes[2]);
    assert!(
        !events.iter().any(|e| e.to_string().contains(&sibling)),
        "the sibling with no run appeared: {events:?}"
    );

    let chat: Vec<_> = events.iter().filter(|e| e.get("entry").is_some()).collect();
    assert!(chat.iter().all(|c| c["thread"] == thread.as_str()), "{chat:?}");
    let ns: Vec<_> = chat.iter().map(|c| c["n"].as_u64().unwrap()).collect();
    assert!(ns.windows(2).all(|w| w[0] < w[1]), "{ns:?}");
    assert_eq!((ns[0], &chat[0]["entry"]["type"]), (0, &json!("user")), "{chat:?}");
    let call = chat.iter().find(|c| c["entry"]["type"] == "tool_call").unwrap();
    let result = chat.last().unwrap();
    assert_eq!(call["entry"]["id"], result["entry"]["id"], "{chat:?}");
    assert!(
        events[..replayed].contains(call),
        "the call was not replayed: {events:?}"
    );
    assert!(events[released..].contains(result), "{events:?}");

    d.until_state(&run, "passed").await;
}
