//! The T3 card and its local ACPv2 provider against isolated daemons and harness transcripts.

use super::{
    super::{claude, codex, tracker::br_in},
    card,
};
use crate::common::{self, Daemon};
use loop_manager_testkit::{
    anthropic::{Provider, Reply},
    openai,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdout},
};

struct CardProvider {
    child: Child,
    lines: Lines<BufReader<ChildStdout>>,
}

impl CardProvider {
    async fn start(d: &Daemon) -> Self {
        let program = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/t3-run-card/sessions.py");
        let mut child = tokio::process::Command::new("python3")
            .arg("-B")
            .arg(program)
            .arg("--socket")
            .arg(d.socket())
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut provider = Self { child, lines };
        let (_, init) = provider.call("initialize", json!({"protocolVersion": 2})).await;
        assert_eq!(init["result"]["protocolVersion"], 2, "{init}");
        provider
    }

    async fn resume(&mut self, session: &str) -> (Vec<Value>, Value) {
        self.call(
            "session/resume",
            json!({"sessionId": session, "replayFrom": {"type": "start"}}),
        )
        .await
    }

    async fn call(&mut self, method: &str, params: Value) -> (Vec<Value>, Value) {
        let request = json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params});
        self.child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        tokio::time::timeout(common::WAIT, async {
            let mut updates = Vec::new();
            loop {
                let line = self.lines.next_line().await.unwrap().expect("provider closed stdout");
                let message: Value = serde_json::from_str(&line).unwrap();
                if message["id"] == 1 {
                    return (updates, message);
                }
                updates.push(message);
            }
        })
        .await
        .expect("provider response")
    }
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

/// The claude transcripts under `dir`: the `.jsonl` files of a `projects` directory.
fn transcripts(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            found.extend(transcripts(&path));
        } else if path.extension().is_some_and(|e| e == "jsonl")
            && path.components().any(|c| c.as_os_str() == "projects")
        {
            found.push(path);
        }
    }
    found
}

/// Kills the claude of `run` with `-9` once its held command `name` has started in `dir` and is in the transcript
/// (a resume needs it there), then lets the command end.
async fn kill_in(d: &Daemon, run: &str, dir: &Path, name: &str) {
    started(dir, name).await;
    let pid = tokio::time::timeout(common::WAIT, async {
        loop {
            let written =
                (transcripts(d.dir.path()).iter()).any(|t| fs::read_to_string(t).is_ok_and(|text| text.contains(name)));
            let procs = claude::of_run(run, d.dir.path());
            if let Some((pid, _)) = procs
                .iter()
                .find(|(_, cmd)| cmd.contains("--session-id"))
                .filter(|_| written)
            {
                return *pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the claude of the run");
    let status = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status();
    assert!(status.unwrap().success());
    fs::write(dir.join(format!("{name}-go")), "").unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn resumed_and_continued_sessions_replay_only_the_selected_work() {
    let finish = |result: &str| format!("{} finish {result} --summary {result}ed", common::EXE);
    // The first run fails: its bead stays open and unclaimed for the second.
    let provider = Provider::start(vec![
        Reply::Bash(held("c-one", "echo one")),
        Reply::Bash("echo two".into()),
        Reply::Bash(finish("fail")),
        Reply::Text("Done.".into()),
        Reply::Bash(held("c-three", "echo three")),
        Reply::Bash(finish("pass")),
        Reply::Text("Done again.".into()),
    ])
    .await;
    let d = claude::daemon(&provider).await;
    let shop = d.dir.path().join("config/loop-manager/shop");
    let web = shop.join("web");
    let bead = br_in(&shop, Some("w"), &["create", "Fix the total", "--silent"]);
    let start = async |continue_from: Option<&str>| {
        let body = json!({ "project": "shop", "rig": "web", "formula": "fix", "inputs": { "bead": bead },
            "continue_from": continue_from });
        let reply = d.post("/v1/runs", body, None).await.unwrap();
        reply["run"].as_str().unwrap().to_owned()
    };

    // The first run: its claude dies in a command and the next attempt resumes the session.
    let first = start(None).await;
    kill_in(&d, &first, &web, "c-one").await;
    d.until_state(&first, "failed").await;
    let attempts = d.get(&format!("/v1/runs/{first}")).await["attempts"].clone();
    assert_eq!(attempts.as_array().unwrap().len(), 2, "{attempts}");

    let second = start(Some(&first)).await;
    started(&web, "c-three").await;
    let url = |run: &str| format!("/v1/runs/{run}/live?chat=true&attempt=1");
    assert_eq!(d.get(&url(&second)).await["live"], true);
    assert_eq!(d.get(&url(&first)).await["live"], false);
    let mut provider = CardProvider::start(&d).await;
    let native = format!("lm-agent/shop/{bead}#t:{second}.1");
    let (updates, response) = provider.resume(&native).await;
    assert_eq!(response["result"], json!({}));
    assert!(updates.iter().any(|m| m["params"]["update"]["state"] == "running"));
    fs::write(web.join("c-three-go"), "").unwrap();
    tokio::time::timeout(common::WAIT, async {
        let mut finished = false;
        loop {
            let line = provider.lines.next_line().await.unwrap().unwrap();
            let message: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(message["params"]["sessionId"], native);
            let update = &message["params"]["update"];
            finished |= update["content"][0]["text"]
                .as_str()
                .is_some_and(|s| s.contains("Done again."));
            if update["state"] == "idle" {
                assert!(finished);
                break;
            }
        }
    })
    .await
    .expect("live transcript through the end of the turn");
    d.until_state(&second, "passed").await;
    let (_, list) = provider.call("session/list", json!({})).await;
    let sessions = list["result"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2, "{list}");
    for (run, expected, excluded) in [(&first, "c-one", "c-three"), (&second, "c-three", "c-one")] {
        let native = format!("lm-agent/shop/{bead}#t:{run}.1");
        assert!(sessions.iter().any(|s| s["sessionId"] == native));
        let page = d.get(&url(run)).await;
        let entries = page["entries"].as_array().unwrap();
        assert_eq!(
            d.get(&format!("{}&offset=1", url(run))).await["entries"],
            json!(entries[1..])
        );
        let (updates, response) = provider.resume(&native).await;
        assert_eq!(response["result"], json!({}), "{response}");
        assert!(updates.iter().all(|m| m["params"]["sessionId"] == native));
        let texts: Vec<_> = updates
            .iter()
            .filter_map(|m| m["params"]["update"]["content"][0]["text"].as_str())
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("Tool call") && t.contains(expected)),
            "{texts:?}"
        );
        assert!(texts.iter().any(|t| t.contains("Tool result")));
        assert!(!texts.iter().any(|t| t.contains(excluded)));
    }
    let files = transcripts(d.dir.path());
    assert_eq!(files.len(), 1, "{files:?}");
    fs::remove_file(&files[0]).unwrap();
    let native = sessions[0]["sessionId"].as_str().unwrap();
    let (_, missing) = provider.resume(native).await;
    assert!(missing["error"]["message"].as_str().unwrap().contains("transcript"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_card_lists_replacement_sessions_by_round() {
    let finish = |result: &str| format!("{} finish {result} --summary done", common::EXE);
    let command = format!(
        "if test -e worked; then {}; else touch worked && {}; fi",
        finish("pass"),
        finish("fail")
    );
    let provider = Provider::start(vec![Reply::Turn(vec![
        Reply::Bash(command),
        Reply::Text("Done.".into()),
    ])])
    .await;
    let d = claude::daemon(&provider).await;
    let formula = r#"name = "fix"
rounds = 2
[[step]]
name = "work"
kind = "ai"
agent = "dev"
max_attempts = 2
[[step]]
name = "review"
kind = "exec"
back_to = "work"
command = ["sh", "-c", "test -e reviewed || { touch reviewed; exit 1; }"]
"#;
    fs::write(d.dir.path().join("config/loop-manager/shop/formulas/fix.toml"), formula).unwrap();
    d.post("/v1/config/reload", json!({}), None).await.unwrap();
    let run = d.create("fix", None).await;
    d.until_state(&run, "passed").await;
    let data = card(&d, "show_run_card", json!({"run":run}));
    for (round, attempts) in [("1", vec![1, 2]), ("2", vec![4])] {
        let steps = &data["roundHistory"][round];
        let sessions = steps["work"]["sessions"].as_array().unwrap();
        assert_eq!(
            sessions
                .iter()
                .map(|s| s["attempt"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            attempts
        );
        assert_eq!(steps["review"]["sessions"], json!([]));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_beadless_codex_session_opens_by_its_listed_id() {
    let finish = format!("{} finish pass --summary built", common::EXE);
    let provider = openai::Provider::start(vec![
        openai::Reply::Command(finish),
        openai::Reply::Text("Done.".into()),
    ])
    .await;
    let d = codex::daemon(&provider).await;
    let run = d.create("build", None).await;
    d.until_state(&run, "passed").await;
    let mut provider = CardProvider::start(&d).await;
    let (_, list) = provider.call("session/list", json!({})).await;
    let sessions = list["result"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    let native = sessions[0]["sessionId"].as_str().unwrap();
    assert!(native.starts_with("lm-run/shop/"));
    let (updates, response) = provider.resume(native).await;
    assert_eq!(response["result"], json!({}));
    let history = updates.iter().map(|m| &m["params"]["update"]).collect::<Vec<_>>();
    assert!(
        history.iter().any(|u| u["sessionUpdate"] == "user_message"
            && u["content"][0]["text"].as_str().unwrap().contains("Build web."))
    );
    for text in ["Tool call", "finish pass --summary built", "Tool result"] {
        assert!(
            history
                .iter()
                .any(|u| u["content"][0]["text"].as_str().is_some_and(|s| s.contains(text))),
            "{history:?}"
        );
    }
}
