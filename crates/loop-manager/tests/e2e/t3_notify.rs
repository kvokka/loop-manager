//! The addon `addons/t3-notify` of this repository on a real daemon, with the auto-open hook of the plugin
//! `t3-run-card` as the thread that starts the runs (addons/t3-notify/README.md): the thread is told when its run
//! starts, waits for a decision and ends, each once.

use crate::{
    common::{Daemon, WAIT},
    run_card::hook_printing,
};
use serde_json::json;
use std::{fs, os::unix::fs::PermissionsExt, path::Path, time::Instant};

/// A run of `ask` in `rig` started by a tool call of the thread, as the hook of the plugin sees it once the run is in
/// `state`.
async fn started_by_the_thread(d: &Daemon, rig: &str, state: &str) -> String {
    assert_eq!(hook_printing(d, "PreToolUse", ""), None);
    let body = json!({ "project": "shop", "rig": rig, "formula": "ask" });
    let run = d.post("/v1/runs", body, None).await.unwrap()["run"]
        .as_str()
        .unwrap()
        .to_owned();
    d.until_state(&run, state).await;
    let shown = hook_printing(d, "PostToolUse", &run).unwrap();
    assert!(shown.contains(&format!(r#""run": "{run}""#)), "{shown}");
    run
}

/// What the stand-in `t3` was called with, a call per entry: its arguments, then the message.
fn sent(dir: &Path) -> Vec<String> {
    let calls = fs::read_dir(dir.join("sent")).unwrap();
    calls.map(|f| fs::read_to_string(f.unwrap().path()).unwrap()).collect()
}

async fn until_sent(dir: &Path, text: &str) {
    let deadline = Instant::now() + WAIT;
    while !sent(dir).iter().any(|m| m.contains(text)) {
        assert!(Instant::now() < deadline, "no message with {text:?}: {:?}", sent(dir));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_thread_is_told_of_the_start_the_wait_and_the_end_of_its_run() {
    let addon = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../addons/t3-notify");
    let d = Daemon::start(|dir| {
        let shop = dir.join("config/loop-manager/shop");
        let project = format!(
            "[[addon]]\npath = \"{}\"\n[settings]\naddons = [{{ name = \"t3-notify\" }}]\n\
             [rigs.web]\npath = \"web\"\nmax_writers = 1\n[rigs.docs]\npath = \"web\"\n",
            addon.display()
        );
        fs::write(shop.join("loop-manager.toml"), project).unwrap();
        // A run that begins with an `await` step starts without a place in the rig: the exec step makes it wait.
        let ask = "name = \"ask\"\n[[step]]\nname = \"work\"\nkind = \"exec\"\ncommand = [\"true\"]\n\
                   [[step]]\nname = \"sign_off\"\nkind = \"await\"\n";
        fs::write(shop.join("formulas/ask.toml"), ask).unwrap();
        fs::create_dir_all(dir.join("path")).unwrap();
        fs::create_dir(dir.join("sent")).unwrap();
        // Each call is a file of its own: the hooks of two runs run side by side.
        let t3 = format!(
            "#!/bin/sh\n{{ echo \"$*\"; cat; }} >\"$(mktemp '{}/XXXXXX')\"\n",
            dir.join("sent").display()
        );
        fs::write(dir.join("path/t3"), t3).unwrap();
        fs::set_permissions(dir.join("path/t3"), PermissionsExt::from_mode(0o755)).unwrap();
    })
    .await;
    let dir = d.dir.path();
    let threads = dir.join("run/loop-manager/t3-threads");
    let outcome = json!({"step": "sign_off", "outcome": {"result": "pass", "summary": "Signed off"}});

    // A run nobody follows holds the one place of the rig: the thread's run waits for it.
    let first = d.create("slow", None).await;
    d.until_state(&first, "running").await;
    let second = started_by_the_thread(&d, "web", "queued").await;
    assert!(threads.join(&second).exists() && !threads.join(format!("{second}.started")).exists());
    // A run that started before its call returned: the call shows its card, so its start counts as announced.
    let third = started_by_the_thread(&d, "docs", "running").await;
    assert!(threads.join(format!("{third}.started")).exists());

    d.post(&format!("/v1/runs/{first}/cancel"), json!({"reason": "done"}), None)
        .await
        .unwrap();
    until_sent(
        dir,
        &format!("{second} (shop.ask) waits for a decision at its step sign_off until"),
    )
    .await;
    d.post(&format!("/v1/runs/{second}/resolve"), outcome, None)
        .await
        .unwrap();
    until_sent(dir, &format!("{second} (shop.ask) passed")).await;

    let messages = sent(dir);
    assert!(
        !messages.iter().any(|m| m.contains(&format!("{first} (shop.slow)"))),
        "{messages:?}"
    );
    let told = format!("thread send test-thread -\n[loop-manager] {second} (shop.ask) ");
    let mut about: Vec<_> = messages.iter().filter_map(|m| m.strip_prefix(&told)).collect();
    about.sort_unstable();
    let socket = fs::canonicalize(d.socket()).unwrap();
    let card = format!(
        r#"Show its card: call show_run_card with {{"socket": "{}", "run": "{second}"}}."#,
        socket.display()
    );
    assert_eq!(about[0], format!("left the queue and started. {card}"));
    assert!(
        about[1].starts_with("passed.\n\nSummary of the last step, sign_off:\nSigned off\n\n")
            && about[1].contains(&card),
        "{}",
        about[1]
    );
    let answer = format!("run resolve {second} sign_off pass --summary '<the answer>'`");
    assert!(
        about[2].starts_with("waits for") && about[2].contains(&answer),
        "{}",
        about[2]
    );
    assert_eq!(about.len(), 3);
    // An ended run leaves nothing behind.
    let left = || {
        fs::read_dir(&threads)
            .unwrap()
            .any(|f| f.unwrap().file_name().to_string_lossy().starts_with(&second))
    };
    let deadline = Instant::now() + WAIT;
    while left() {
        assert!(Instant::now() < deadline, "files of {second} stay in {threads:?}");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}
