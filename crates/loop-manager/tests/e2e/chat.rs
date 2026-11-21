//! A CLI reader arrives while a tool is running, reads its call, then releases it and receives its result.

use crate::common::{self, Daemon};
use serde_json::Value;
use std::{fs, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};

pub(super) fn command() -> String {
    format!(
        "touch chat-started && while test ! -e chat-release; do sleep 0.05; done && {} finish pass --summary built",
        common::EXE
    )
}

pub(super) async fn check(d: &Daemon, tool: &str) {
    // The second session ends quickly while the same watch is connected. A later connection starts at that session.
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
