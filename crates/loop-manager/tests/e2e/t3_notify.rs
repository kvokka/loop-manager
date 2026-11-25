//! The addon `addons/t3-notify` of this repository on a real daemon, with the auto-open hook of the plugin
//! `t3-run-card` as the thread that starts the runs (addons/t3-notify/README.md): the thread is told when its run
//! starts, waits for a decision and ends, each once, and an end carries the run's spend at the detail of the param
//! `spend`.

use crate::{
    common::{self, Daemon, WAIT},
    run_card::hook_printing,
};
use loop_manager_testkit::anthropic::{Provider, Reply};
use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::Path, time::Instant};

/// A daemon whose project `shop` attaches the addon (`attachment` follows its name in the attachment, as
/// `, params = { spend = "off" }`) beside `rigs`, whose formula `ask` runs the step `work` and then waits in its
/// `await` step `sign_off`, and whose `PATH` has a stand-in `t3` and the built `loop-manager`; `setup` adds files.
async fn daemon(attachment: &str, rigs: &str, work: &str, setup: impl FnOnce(&Path)) -> Daemon {
    let addon = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../addons/t3-notify");
    Daemon::start(|dir| {
        let shop = dir.join("config/loop-manager/shop");
        let project = format!(
            "[[addon]]\npath = \"{}\"\n[settings]\naddons = [{{ name = \"t3-notify\"{attachment} }}]\n{rigs}",
            addon.display()
        );
        fs::write(shop.join("loop-manager.toml"), project).unwrap();
        let ask = format!(
            "name = \"ask\"\n[[step]]\nname = \"work\"\n{work}[[step]]\nname = \"sign_off\"\nkind = \"await\"\n"
        );
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
        // The hook asks the daemon under test for the spend table with the `loop-manager` it finds on its PATH.
        std::os::unix::fs::symlink(common::EXE, dir.join("path/loop-manager")).unwrap();
        setup(dir);
    })
    .await
}

/// The step `work` of `ask`: a script.
const SCRIPT: &str = "kind = \"exec\"\ncommand = [\"true\"]\n";

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
    // A run that begins with an `await` step starts without a place in the rig: the exec step makes it wait.
    let rigs = "[rigs.web]\npath = \"web\"\nmax_writers = 1\n[rigs.docs]\npath = \"web\"\n";
    let d = daemon("", rigs, SCRIPT, |_| {}).await;
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

/// The notice of the end of a run of `ask` in `web` that the thread started, once its `sign_off` passes.
async fn end_notice(d: &Daemon) -> String {
    let run = started_by_the_thread(d, "web", "running").await;
    let deadline = Instant::now() + WAIT;
    while d.get(&format!("/v1/runs/{run}")).await["step"] != "sign_off" {
        assert!(Instant::now() < deadline, "{run} does not reach sign_off");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let outcome = json!({"step": "sign_off", "outcome": {"result": "pass", "summary": "Signed off"}});
    d.post(&format!("/v1/runs/{run}/resolve"), outcome, None).await.unwrap();
    let passed = format!("{run} (shop.ask) passed");
    until_sent(d.dir.path(), &passed).await;
    sent(d.dir.path()).into_iter().find(|m| m.contains(&passed)).unwrap()
}

/// The lines of the spend table in `notice`, `None` if it has none.
fn table(notice: &str) -> Option<Vec<&str>> {
    let rest = notice.split_once("\n\nSpend:\n```\n")?.1;
    Some(rest.split_once("\n```")?.0.lines().collect())
}

/// The lines of the spend table of the end of a run whose step `work` is a session of the claude agent `dev` on a
/// fake provider: it spends, then passes. `attachment` as in [`daemon`].
async fn claude_table(attachment: &str) -> Vec<String> {
    let finish = format!("{} finish pass --summary built", common::EXE);
    let usage = json!({ "input_tokens": 1200, "output_tokens": 34 });
    let plan = vec![
        Reply::Usage(usage, Box::new(Reply::Bash(finish))),
        Reply::Text("Done.".into()),
    ];
    let provider = Provider::start(plan).await;
    let agent = format!(
        "harness = \"claude\"\nmodel = \"claude-haiku-5-5\"\ninherit_host = false\n[env]\nANTHROPIC_BASE_URL = \
         \"{}\"\nANTHROPIC_API_KEY = \"fake\"\nCLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC = \"1\"\n\
         CLAUDE_CODE_MAX_RETRIES = \"0\"\n",
        provider.base_url()
    );
    let work = "kind = \"ai\"\nagent = \"dev\"\n";
    let d = daemon(attachment, "[rigs.web]\npath = \"web\"\n", work, |dir| {
        let shop = dir.join("config/loop-manager/shop");
        let config = "projects = [\"shop\"]\n[harness.claude]\n";
        fs::write(dir.join("config/loop-manager/config.toml"), config).unwrap();
        fs::create_dir_all(shop.join("agents/dev")).unwrap();
        fs::write(shop.join("agents/dev/agent.toml"), agent).unwrap();
        fs::write(shop.join("agents/dev/prompt.template.md"), "Work on {{ rig.name }}.").unwrap();
    })
    .await;
    let notice = end_notice(&d).await;
    let lines = table(&notice).unwrap_or_else(|| panic!("no spend table: {notice}"));
    lines.into_iter().map(str::to_owned).collect()
}

/// By default the end of a run tells its spend a row per agent, then the total.
#[tokio::test(flavor = "multi_thread")]
async fn the_end_of_a_run_tells_its_spend_by_agent() {
    let lines = claude_table("").await;
    assert_eq!(lines.len(), 3, "{lines:#?}");
    assert!(
        lines[0].starts_with("agent  ") && lines[0].ends_with("cost USD"),
        "{lines:#?}"
    );
    assert!(lines[1].starts_with("shop.dev  "), "{lines:#?}");
    assert!(lines[2].starts_with("total  "), "{lines:#?}");
}

/// `spend = "total"` tells the total alone.
#[tokio::test(flavor = "multi_thread")]
async fn the_param_spend_total_tells_the_total_alone() {
    let lines = claude_table(", params = { spend = \"total\" }").await;
    assert_eq!(lines.len(), 2, "{lines:#?}");
    assert!(lines[1].starts_with("total  "), "{lines:#?}");
}

/// `spend = "model"` tells a row per agent, provider and model.
#[tokio::test(flavor = "multi_thread")]
async fn the_param_spend_model_tells_a_row_per_model() {
    let lines = claude_table(", params = { spend = \"model\" }").await;
    let head: Vec<_> = lines[0].split_whitespace().collect();
    assert_eq!(head[..3], ["agent", "provider", "model"], "{lines:#?}");
    let rows = &lines[1..lines.len() - 1];
    assert!(
        rows.iter()
            .any(|r| r.starts_with("shop.dev  ") && r.contains("  claude-haiku-5-5  ")),
        "{lines:#?}"
    );
    assert!(lines.last().unwrap().starts_with("total  "), "{lines:#?}");
}

/// `spend = "off"` tells no spend. A value that is no detail level tells none either, and fails the hook on the
/// notice it sent: `addon.hook_failed` names the value.
#[tokio::test]
async fn the_param_spend_off_or_wrong_leaves_the_spend_out() {
    for (value, wrong) in [("off", false), ("all", true)] {
        let attachment = format!(", params = {{ spend = \"{value}\" }}");
        let d = daemon(&attachment, "[rigs.web]\npath = \"web\"\n", SCRIPT, |_| {}).await;
        let notice = end_notice(&d).await;
        assert!(notice.contains("Signed off") && table(&notice).is_none(), "{notice}");
        if !wrong {
            continue;
        }
        let journal = d.dir.path().join("state/loop-manager/journal/events.jsonl");
        let deadline = Instant::now() + WAIT;
        let failed = loop {
            let text = fs::read_to_string(&journal).unwrap_or_default();
            let events = text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok());
            let mut failed = events.filter(|e| e["type"] == "addon.hook_failed");
            if let Some(event) = failed.find(|e| e["data"]["hook_type"] == "run.passed") {
                break event;
            }
            assert!(Instant::now() < deadline, "no failed hook of run.passed");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        let stderr = failed["data"]["stderr_tail"].as_str().unwrap();
        assert!(stderr.contains("the param spend is 'all'"), "{failed}");
    }
}
