//! `loop-manager acp` over the sessions of a run: resumed, continued and lost: scenario C of the epic dev-pa4.

use super::super::{
    claude::of_run,
    tasks::{br, claude},
};
use super::{Acp, held, kind, on, started};
use crate::common::{self, Daemon};
use loop_manager_testkit::anthropic::{Provider, Reply};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

/// The commands of the Bash calls on `session`, in order.
fn commands(lines: &[Value], session: &str) -> Vec<String> {
    (on(lines, session).into_iter())
        .filter(|u| kind(u) == "tool_call_update" && u["status"] == "in_progress")
        .filter_map(|u| u["rawInput"]["command"].as_str().map(str::to_owned))
        .collect()
}

/// The ids of the sessions announced among `lines`.
fn announced(lines: &[Value]) -> Vec<&str> {
    (lines.iter())
        .filter(|l| l["params"]["update"]["sessionUpdate"] == "subagent_update")
        .filter_map(|l| l["params"]["update"]["sessionId"].as_str())
        .collect()
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
            let procs = of_run(run, d.dir.path());
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

/// The replay of `root`: the lines from its snapshot on, without what an earlier view sent live before it.
async fn resume(acp: &mut Acp, id: u64, root: &str, shop: &Path) -> Vec<Value> {
    let params = json!({ "sessionId": root, "cwd": shop, "replayFrom": { "type": "start" } });
    let (mut lines, response) = acp.call(id, "session/resume", params).await;
    assert_eq!(response["result"], json!({}), "{response}");
    let body = |l: &Value| l["params"]["sessionId"] == root && l["params"]["update"]["messageId"] == "body";
    lines.split_off(lines.iter().position(body).expect("the snapshot"))
}

/// "Resumed and replaced sessions" (the resumed half), "round slicing", "missing historical data" and "live updates
/// for past runs": a session killed in a command and resumed is one thread; a later run that continues the ended
/// session shows live on a thread of its own round, and each round replays only its slice of the transcript; a
/// deleted transcript leaves both threads visible with the unavailable indication.
#[tokio::test(flavor = "multi_thread")]
async fn acp_shows_a_resumed_session_once_slices_a_continued_one_and_survives_a_lost_transcript() {
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
    let d = claude(&provider).await;
    let shop = d.dir.path().join("config/loop-manager/shop");
    let web = shop.join("web");
    let epic = br(&shop, &["create", "Ship the cart", "-t", "epic", "--silent"]);
    let under = format!("parent-child:{epic}");
    let bead = br(&shop, &["create", "Fix the total", "--deps", &under, "--silent"]);
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

    let root = format!("shop/{epic}");
    let thread = |run: &str| format!("{root}#t:{run}.1");
    let mut acp = Acp::spawn(&d);
    acp.call(1, "initialize", json!({ "protocolVersion": 2 })).await;
    let replay = resume(&mut acp, 2, &root, &shop).await;
    let threads: Vec<_> = (announced(&replay).into_iter()).filter(|s| s.contains("#t:")).collect();
    assert_eq!(threads, [thread(&first)], "{replay:#?}");
    let ran = commands(&replay, &thread(&first));
    assert!(ran[0].contains("c-one") && ran[1] == "echo two", "{ran:?}");

    // The second run continues the ended session: its records come live, on its own thread only.
    let second = start(Some(&first)).await;
    let live = acp
        .until("the live call of the continued session", |l| {
            let u = &l["params"]["update"];
            l["params"]["sessionId"] == thread(&second).as_str()
                && u["status"] == "in_progress"
                && u["rawInput"]["command"].as_str().is_some_and(|c| c.contains("c-three"))
        })
        .await;
    assert!(
        announced(&live).contains(&format!("{root}#r:{second}.1").as_str()),
        "{live:#?}"
    );
    assert!(commands(&live, &thread(&first)).is_empty(), "{live:#?}");
    fs::write(web.join("c-three-go"), "").unwrap();
    d.until_state(&second, "passed").await;

    // A reopen replays each round's slice of the one transcript.
    let replay = resume(&mut acp, 3, &root, &shop).await;
    let (old, new) = (commands(&replay, &thread(&first)), commands(&replay, &thread(&second)));
    assert!(old.iter().all(|c| !c.contains("c-three")) && old.len() == 3, "{old:?}");
    assert!(
        new[0].contains("c-three") && new.iter().all(|c| !c.contains("c-one")),
        "{new:?}"
    );
    let prompt = on(&replay, &thread(&second))
        .into_iter()
        .find(|u| kind(u) == "user_message");
    assert!(prompt.is_some(), "{replay:#?}");

    // The transcript is gone: both threads stay, each says so and replays nothing.
    let files = transcripts(d.dir.path());
    assert_eq!(files.len(), 1, "{files:?}");
    fs::remove_file(&files[0]).unwrap();
    let replay = resume(&mut acp, 4, &root, &shop).await;
    for run in [&first, &second] {
        let updates = on(&replay, &thread(run));
        let unavailable: Vec<_> = updates.iter().filter(|u| u["messageId"] == "unavailable").collect();
        assert_eq!(unavailable.len(), 1, "{updates:#?}");
        let text = unavailable[0]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.starts_with("The transcript of this session is unavailable: "),
            "{text}"
        );
        assert!(!updates.iter().any(|u| kind(u) == "tool_call_update"), "{updates:#?}");
    }
}
