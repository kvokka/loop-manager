//! The optional chat plugin opens a card on the daemon socket and resumes from the cursor after a terminal event.

mod sessions;

use crate::common::{self, Daemon, WAIT};
use serde_json::{Value, json};
use std::{io::Write, path::Path, process::Stdio};

fn card(d: &Daemon, name: &str, args: Value) -> Value {
    card_in(d, name, args, &d.dir.path().join("run"))
}

fn card_in(d: &Daemon, name: &str, args: Value, runtime: &Path) -> Value {
    let server = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/t3-run-card/server.py");
    let mut child = common::program("timeout")
        .arg(WAIT.as_secs().to_string())
        .arg("python3")
        .arg(server)
        .env_remove("LOOP_MANAGER_SOCKET")
        .env("T3CODE_HOME", d.dir.path().join("t3"))
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_CONFIG_HOME", d.dir.path().join("config"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let rpc = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}
    });
    writeln!(child.stdin.take().unwrap(), "{rpc}").unwrap();
    let reply = child.wait_with_output().unwrap();
    assert!(reply.status.success());
    let payload: Value = serde_json::from_slice(&reply.stdout).unwrap();
    assert!(payload["result"]["structuredContent"].is_object(), "{payload}");
    payload["result"]["structuredContent"].clone()
}

#[tokio::test]
async fn the_card_catches_up_after_a_disconnected_client_returns() {
    let d = Daemon::start(|_| {}).await;
    let run = d.create("slow", None).await;
    d.until_state(&run, "running").await;
    let opened = card(&d, "show_run_card", json!({ "run": run, "socket": d.socket() }));
    assert_eq!(opened["socket"], json!(d.socket()));
    assert_eq!(opened["card"]["rounds"], 3);
    assert_eq!(opened["terminal"], false);
    assert_eq!(opened["plan"][0]["kind"], "exec");
    d.post(&format!("/v1/runs/{run}/cancel"), json!({"reason": "done"}), None)
        .await
        .unwrap();
    d.until_state(&run, "cancelled").await;
    let changed = card(&d, "watch_run_card", json!({ "run": run, "cursor": opened["cursor"] }));
    assert_eq!(changed["card"]["state"], "cancelled");
    assert_eq!(changed["terminal"], true);
}

#[tokio::test]
async fn a_card_keeps_the_summaries_of_the_previous_and_final_steps() {
    let d = Daemon::start(|dir| {
        let formula = r#"
name = "handoff"
[[step]]
name = "review"
kind = "await"
[[step]]
name = "publish"
kind = "exec"
command = ["true"]
"#;
        std::fs::write(dir.join("config/loop-manager/shop/formulas/handoff.toml"), formula).unwrap();
    })
    .await;
    let run = d.create("handoff", None).await;
    d.until_state(&run, "running").await;
    let args = json!({"run": run});
    let open = card(&d, "show_run_card", args.clone());
    assert_eq!(open["card"]["summaries"], json!({}));
    d.post(
        &format!("/v1/runs/{run}/resolve"),
        json!({"step": "review", "outcome": {"result": "pass", "summary": "Ready for release"}}),
        None,
    )
    .await
    .unwrap();
    d.until_state(&run, "passed").await;
    let done = card(&d, "show_run_card", args);
    assert_eq!(
        done["card"]["summaries"],
        json!({"publish": "exit 0", "review": "Ready for release"})
    );
}

/// Different daemons may both have a run_1. Follow-up reads must stay on the selected daemon.
#[tokio::test]
async fn the_card_selects_the_default_named_or_sole_registered_socket() {
    let local = Daemon::start(|_| {}).await;
    let other = Daemon::start(|_| {}).await;
    let run = local.create("check", None).await;
    assert_eq!(other.create("slow", None).await, run);
    let registry = local.dir.path().join("config/loop-manager/sockets");
    std::fs::create_dir_all(&registry).unwrap();
    std::os::unix::fs::symlink(other.socket(), registry.join("work")).unwrap();
    let args = json!({"run": run});
    let default = card(&local, "show_run_card", args.clone());
    assert_eq!(default["socket"], json!(local.socket()));
    let named = card(&local, "show_run_card", json!({"run": run, "daemon": "work"}));
    let fallback = card_in(&local, "show_run_card", args, &local.dir.path().join("no-default"));
    for selected in [named, fallback] {
        assert_eq!(selected["socket"], json!(other.socket()));
        assert_eq!(selected["card"]["formula"], "shop.slow");
    }
}

/// The card says why a run does not move: a writer place another run holds, and the command for an awaited outcome.
#[tokio::test]
async fn a_card_tells_what_a_queued_or_awaiting_run_waits_for() {
    let d = Daemon::start(|dir| {
        let rig = "[rigs.web]\npath = \"web\"\nmax_writers = 1\n";
        std::fs::write(dir.join("config/loop-manager/shop/loop-manager.toml"), rig).unwrap();
        let ask = "name = \"ask\"\n[[step]]\nname = \"sign_off\"\nkind = \"await\"\n";
        std::fs::write(dir.join("config/loop-manager/shop/formulas/ask.toml"), ask).unwrap();
    })
    .await;
    let first = d.create("slow", None).await;
    d.until_state(&first, "running").await;
    let second = d.create("slow", None).await;
    let queued = card(&d, "show_run_card", json!({ "run": second }));
    assert_eq!(queued["card"]["state"], "queued");
    assert_eq!(
        queued["card"]["why"],
        format!(
            "Waiting for a free place in rig web: its max_writers is reached. Active in the rig: {first} (1, running)"
        )
    );
    assert_eq!(queued["card"]["answer"], Value::Null);
    let asking = d.create("ask", None).await;
    d.until_state(&asking, "running").await;
    let awaiting = card(&d, "show_run_card", json!({ "run": asking }));
    assert_eq!(awaiting["card"]["why"], "Waiting for your decision on sign_off");
    assert_eq!(
        awaiting["card"]["answer"],
        format!("loop-manager run resolve {asking} sign_off pass --summary '<your answer>'")
    );
}

/// The auto-open hook on the hook event `name` of one tool call: the context it adds.
fn hook(d: &Daemon, name: &str) -> Option<String> {
    let hook = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/t3-run-card/hooks/auto_open.py");
    let event = json!({ "hook_event_name": name, "session_id": "s1", "tool_use_id": "t1", "tool_name": "Bash",
        "tool_input": { "command": "br create" }, "tool_response": { "stdout": "", "stderr": "" } });
    let mut child = common::program("python3")
        .arg(hook)
        .env_remove("LOOP_MANAGER_SOCKET")
        .env("T3CODE_THREAD_ID", "test-thread")
        .env("CLAUDE_PLUGIN_OPTION_AUTO_OPEN", "true")
        .env("XDG_RUNTIME_DIR", d.dir.path().join("run"))
        .env("XDG_CONFIG_HOME", d.dir.path().join("config"))
        .env("TMPDIR", d.dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    write!(child.stdin.take().unwrap(), "{event}").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let reply: Value = serde_json::from_slice(&out.stdout).ok()?;
    Some(reply["hookSpecificOutput"]["additionalContext"].as_str()?.to_owned())
}

/// A trigger that fires on an issue the call filed opens the card of its run; a call that changes nothing opens none.
#[tokio::test]
async fn the_card_opens_for_a_run_a_trigger_started_on_the_change_of_a_tool_call() {
    let epic = "name = \"epic\"\n[inputs]\nepic = { type = \"task\", required = true }\n\
                [[step]]\nkind = \"exec\"\ncommand = [\"sleep\", \"30\"]\ntimeout = \"60s\"\n";
    let d = Daemon::start(|dir| {
        let project = dir.join("config/loop-manager/shop");
        std::fs::write(project.join("formulas/epic.toml"), epic).unwrap();
        std::fs::write(project.join("loop-manager.toml"), crate::tracker::devloop_project()).unwrap();
        std::fs::create_dir(project.join("repo")).unwrap();
        let git = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&project)
            .status();
        assert!(git.unwrap().success());
        crate::tracker::br_in(&project, None, &["init", "--prefix", "lm", "-q"]);
    })
    .await;
    let project = d.dir.path().join("config/loop-manager/shop");
    assert_eq!(hook(&d, "PreToolUse"), None);
    assert_eq!(hook(&d, "PostToolUse"), None);
    assert_eq!(hook(&d, "PreToolUse"), None);
    crate::tracker::br_in(&project, None, &["create", "--silent", "Filed", "--labels", "auto"]);
    // The daemon reads the write on its own time: the hook waits for it only briefly.
    let deadline = std::time::Instant::now() + WAIT;
    let run = loop {
        if let Some(run) = d.get("/v1/runs").await[0]["id"].as_str() {
            break run.to_owned();
        }
        assert!(std::time::Instant::now() < deadline, "no run started");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    let opened = hook(&d, "PostToolUse").unwrap();
    assert!(
        opened.contains(&format!(r#"call show_run_card now with {{"run": "{run}"}}"#)),
        "{opened}"
    );
}
