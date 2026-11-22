//! A CLI reader arrives while a tool is running, reads its call, then releases it and receives its result: through
//! the chat of a session, or through the run's history for a harness whose driver keeps no transcript.

use crate::common::{self, Daemon};
use serde_json::Value;
use std::{fs, path::PathBuf, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};

pub(super) fn command() -> String {
    format!(
        "touch chat-started && while test ! -e chat-release; do sleep 0.05; done && {} finish pass --summary built",
        common::EXE
    )
}

/// A run of two prompt steps whose first session runs [`command`]: its id and working directory, once the tool started.
async fn started(d: &Daemon) -> (String, PathBuf) {
    // The second session ends quickly while the same watch is connected.
    let formula = r#"name = "pair"
[[step]]
name = "one"
kind = "ai"
agent = "dev"
[[step]]
name = "two"
kind = "ai"
agent = "dev"
"#;
    fs::write(
        d.dir.path().join("config/loop-manager/shop/formulas/pair.toml"),
        formula,
    )
    .unwrap();
    d.post("/v1/config/reload", serde_json::json!({}), Some("pair"))
        .await
        .unwrap();
    let run = d.create("pair", None).await;
    let cwd = d.dir.path().join("config/loop-manager/shop/web");
    tokio::time::timeout(common::WAIT, async {
        while !cwd.join("chat-started").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the tool started before watch");
    (run, cwd)
}

pub(super) async fn check(d: &Daemon, tool: &str) {
    // A later connection starts at the second session.
    let (run, cwd) = started(d).await;
    let cli = || {
        let mut cli = tokio::process::Command::from(common::loop_manager());
        cli.kill_on_drop(true)
            .args(["run", "watch", &run, "--chat"])
            .env("LOOP_MANAGER_SOCKET", d.socket());
        cli
    };
    let mut watch = cli().arg("--json").stdout(Stdio::piped()).spawn().unwrap();
    let mut lines = BufReader::new(watch.stdout.take().unwrap()).lines();
    let mut entries = Vec::new();
    tokio::time::timeout(common::WAIT, async {
        while let Some(line) = lines.next_line().await.unwrap() {
            let entry: Value = serde_json::from_str(&line).unwrap();
            if entry["type"] == "tool_call" {
                assert_eq!(entry["name"], tool);
                fs::write(cwd.join("chat-release"), "").unwrap();
            }
            entries.push(entry);
        }
    })
    .await
    .expect("chat followed the tool and ended");
    assert!(watch.wait().await.unwrap().success(), "{entries:?}");
    let kinds: Vec<_> = entries.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        ["user", "tool_call", "tool_result", "assistant"].repeat(2),
        "{entries:?}"
    );
    for [user, call, result, assistant] in entries.as_chunks::<4>().0 {
        assert!(user["text"].as_str().unwrap().contains("Build web."));
        assert_eq!(call["id"], result["id"]);
        assert!(result["output"].to_string().contains("accepted"));
        assert_eq!(assistant["text"], "Готово.");
    }

    let replay = cli().output().await.unwrap();
    assert!(replay.status.success(), "{}", String::from_utf8_lossy(&replay.stderr));
    let text = String::from_utf8(replay.stdout).unwrap();
    for label in ["User\n", "Tool result [", "Assistant\nГотово."] {
        assert_eq!(text.matches(label).count(), 1, "{text}");
    }
}

/// [`check`] on the run's history: a reader of `run watch --history` arriving while the tool runs reads its call,
/// releases it, and follows both sessions to the run's end; a replay prints the same records.
pub(super) async fn history(d: &Daemon) {
    let (run, cwd) = started(d).await;
    let cli = || {
        let mut cli = tokio::process::Command::from(common::loop_manager());
        cli.kill_on_drop(true)
            .args(["run", "watch", &run, "--history", "--json"])
            .env("LOOP_MANAGER_SOCKET", d.socket());
        cli
    };
    let mut watch = cli().stdout(Stdio::piped()).spawn().unwrap();
    let mut lines = BufReader::new(watch.stdout.take().unwrap()).lines();
    let mut records = Vec::new();
    tokio::time::timeout(common::WAIT, async {
        while let Some(line) = lines.next_line().await.unwrap() {
            let record: Value = serde_json::from_str(&line).unwrap();
            if record["kind"] == "tool_call" && !cwd.join("chat-release").exists() {
                fs::write(cwd.join("chat-release"), "").unwrap();
            }
            records.push(record);
        }
    })
    .await
    .expect("the history followed the tool and ended");
    assert!(watch.wait().await.unwrap().success(), "{records:#?}");
    // The reader caught up while the first tool still ran: before the outcome its command declares.
    let at = |test: &dyn Fn(&Value) -> bool| records.iter().position(test).unwrap_or_else(|| panic!("{records:#?}"));
    assert!(
        at(&|r| r["kind"].is_null()) < at(&|r| r["kind"] == "outcome"),
        "{records:#?}"
    );
    let records: Vec<&Value> = records.iter().filter(|r| !r["kind"].is_null()).collect();
    for step in ["one", "two"] {
        let of = |kind: &str| -> Vec<&Value> {
            let found = records.iter().filter(|r| r["step"] == step && r["kind"] == kind);
            found.map(|r| &r["update"]).collect()
        };
        assert_eq!(of("user")[0]["content"][0]["text"], "Build web.", "{records:#?}");
        let calls = of("tool_call");
        assert_eq!(calls[0]["name"], "Bash", "{records:#?}");
        assert!(
            calls.iter().all(|c| c["toolCallId"] == calls[0]["toolCallId"]),
            "{records:#?}"
        );
        assert_eq!(calls.last().unwrap()["status"], "completed", "{records:#?}");
        // `accepted`, the finisher's answer, in base64.
        let output = of("terminal").iter().any(|t| t["output"]["data"] == "YWNjZXB0ZWQ=");
        assert!(output, "{records:#?}");
        assert_eq!(of("assistant")[0]["content"]["text"], "Готово.", "{records:#?}");
    }
    assert_eq!(records.last().unwrap()["state"], "passed");

    let replay = cli().output().await.unwrap();
    assert!(replay.status.success(), "{}", String::from_utf8_lossy(&replay.stderr));
    let replayed: Vec<Value> = String::from_utf8(replay.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let replayed: Vec<&Value> = replayed.iter().filter(|r| !r["kind"].is_null()).collect();
    assert_eq!(replayed, records);
}
