//! `loop-manager acp` over a codex thread: the same tree and the replayed tool call.

use super::super::tasks::br;
use super::{Acp, kind, on};
use crate::common::{self, Daemon};
use loop_manager_testkit::openai::{Provider, Reply};
use serde_json::json;
use std::fs;

/// A daemon whose `fix` formula is a prompt run of a codex agent on the fake `provider` on a `bead` input, with a br
/// tracker.
async fn codex(provider: &Provider) -> Daemon {
    let base_url = provider.base_url();
    Daemon::start(|dir| {
        let shop = dir.join("config/loop-manager/shop");
        let files = [
            (
                dir.join("config/loop-manager/config.toml"),
                "projects = [\"shop\"]\n[harness.codex]\n".to_owned(),
            ),
            (
                shop.join("agents/dev/agent.toml"),
                // The host forbids user namespaces, so the codex sandbox cannot run here (docs/design/experiments.md).
                format!(
                    "harness = \"codex\"\nmodel = \"gpt-fake\"\ninherit_host = false\n\
                     [native.codex]\nmodel_provider = \"fake\"\nsandbox_mode = \"danger-full-access\"\n\
                     [native.codex.model_providers.fake]\nname = \"fake\"\nbase_url = \"{base_url}\"\n\
                     wire_api = \"responses\"\n"
                ),
            ),
            (
                shop.join("agents/dev/prompt.template.md"),
                "Build {{ rig.name }}.".to_owned(),
            ),
            (
                shop.join("formulas/fix.toml"),
                "name = \"fix\"\n[inputs]\nbead = { type = \"task\", required = true }\n\
                 [[step]]\nkind = \"ai\"\nagent = \"dev\"\n"
                    .to_owned(),
            ),
        ];
        for (path, text) in files {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        br(&shop, &["init", "--prefix", "t", "-q"]);
    })
    .await
}

/// The replay of a codex transcript: the projection does not depend on the harness. A passed run of a codex thread on
/// a bead under an epic replays as the bead, its round and a `(codex)` thread whose prompt, command call and result
/// map as claude's do, the result completing the call by its id.
#[tokio::test(flavor = "multi_thread")]
async fn acp_replays_a_codex_thread_and_pairs_its_command_with_its_result() {
    let finish = format!("{} finish pass --summary built", common::EXE);
    let provider = Provider::start(vec![Reply::Command(finish), Reply::Text("Done.".into())]).await;
    let d = codex(&provider).await;
    let shop = d.dir.path().join("config/loop-manager/shop");
    let epic = br(&shop, &["create", "Ship the cart", "-t", "epic", "--silent"]);
    let under = format!("parent-child:{epic}");
    let bead = br(&shop, &["create", "Fix the total", "--deps", &under, "--silent"]);
    let body = json!({ "project": "shop", "rig": "web", "formula": "fix", "inputs": { "bead": bead } });
    let run = d.post("/v1/runs", body, None).await.unwrap()["run"]
        .as_str()
        .unwrap()
        .to_owned();
    d.until_state(&run, "passed").await;

    let mut acp = Acp::spawn(&d);
    acp.call(1, "initialize", json!({ "protocolVersion": 2 })).await;
    let root = format!("shop/{epic}");
    let resume = json!({ "sessionId": root, "cwd": shop, "replayFrom": { "type": "start" } });
    let (replay, response) = acp.call(2, "session/resume", resume).await;
    assert_eq!(response, json!({ "jsonrpc": "2.0", "id": 2, "result": {} }));
    let (bead_node, round, thread) = (
        format!("{root}#b:{bead}"),
        format!("{root}#r:{run}.1"),
        format!("{root}#t:{run}.1"),
    );

    // The tree: the bead, its round and the codex thread, each on its parent, idle now the run has passed.
    let announced: Vec<_> = (replay.iter())
        .filter(|l| l["params"]["update"]["sessionUpdate"] == "subagent_update")
        .map(|l| (l["params"]["sessionId"].as_str().unwrap(), &l["params"]["update"]))
        .collect();
    let parents: Vec<_> = (announced.iter())
        .map(|(parent, u)| (*parent, u["sessionId"].as_str().unwrap(), &u["state"]))
        .collect();
    let idle = json!({ "state": "idle", "stopReason": "end_turn" });
    assert_eq!(
        parents,
        [
            (root.as_str(), bead_node.as_str(), &idle),
            (bead_node.as_str(), round.as_str(), &idle),
            (round.as_str(), thread.as_str(), &idle),
        ],
        "{replay:#?}"
    );
    assert_eq!(announced[0].1["title"], format!("{bead} Fix the total"));
    assert_eq!(announced[1].1["title"], "Round 1");
    let thread_node = announced[2].1;
    assert!(
        thread_node["title"].as_str().unwrap().ends_with("(codex)"),
        "{thread_node}"
    );
    assert!(thread_node["cwd"].is_string(), "{thread_node}");

    // The thread: the prompt, the command in progress, then its result on the same call.
    let chat = on(&replay, &thread);
    let prompt = chat.iter().find(|u| kind(u) == "user_message").expect("the prompt");
    assert!(
        prompt["content"][0]["text"].as_str().unwrap().contains("Build web."),
        "{prompt}"
    );
    let calls: Vec<_> = (chat.iter().enumerate())
        .filter(|(_, u)| kind(u) == "tool_call_update")
        .collect();
    let (at, call) = calls
        .iter()
        .find(|(_, u)| u["status"] == "in_progress")
        .unwrap_or_else(|| panic!("no command call: {chat:#?}"));
    assert!(
        call["rawInput"].to_string().contains("finish pass --summary built"),
        "{call}"
    );
    let id = call["toolCallId"].as_str().unwrap();
    let result = (calls.iter())
        .find(|(after, u)| after > at && u["toolCallId"] == id)
        .unwrap_or_else(|| panic!("no result on {id}: {chat:#?}"))
        .1;
    assert_eq!(result["status"], "completed", "{result}");

    let tail: Vec<_> = (replay[replay.len() - 5..].iter())
        .map(|l| (l["params"]["sessionId"].as_str().unwrap(), &l["params"]["update"]))
        .collect();
    let complete = json!({ "sessionUpdate": "replay_complete" });
    let root_idle = json!({ "sessionUpdate": "state_update", "state": "idle", "stopReason": "end_turn" });
    assert_eq!(
        tail,
        [
            (bead_node.as_str(), &complete),
            (round.as_str(), &complete),
            (thread.as_str(), &complete),
            (root.as_str(), &complete),
            (root.as_str(), &root_idle),
        ],
        "{replay:#?}"
    );
}
