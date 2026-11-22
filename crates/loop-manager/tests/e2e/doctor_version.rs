//! The version checks of `loop-manager doctor` and of the daemon's start on fake harness and br binaries: a harness
//! from the minimum of its adapter up, br within its tested range (docs/design/contracts.md 4.5,
//! docs/design/tracker-br.md).

use super::doctor::{adapter, doctor, level, script, tree};
use crate::common;
use loop_manager_front::client::Client;
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// What `loop-manager daemon` in `dir` prints on stderr from its start until it is stopped once it serves.
async fn daemon_stderr(dir: &Path) -> String {
    let mut child = common::command(dir).stderr(Stdio::piped()).spawn().unwrap();
    let client = Client::new(dir.join("run/loop-manager/loop-manager.sock"));
    let deadline = Instant::now() + common::WAIT;
    while client.request("GET", "/v1/daemon", None, None).await.is_err() {
        assert!(child.try_wait().unwrap().is_none(), "the daemon exited at start");
        assert!(Instant::now() < deadline, "the daemon did not come up");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let pid = child.id().to_string();
    assert!(Command::new("kill").args(["-TERM", &pid]).status().unwrap().success());
    String::from_utf8(child.wait_with_output().unwrap().stderr).unwrap()
}

#[tokio::test]
async fn a_harness_below_its_minimum_warns_and_the_minimum_and_newer_do_not() {
    let dir = tree("");
    let d = dir.path();
    let config = format!(
        "projects = [\"shop\"]\n[harness.claude]\ncommand = \"{}\"\n[harness.codex]\ncommand = \"{}\"\n",
        d.join("tools/claude").display(),
        d.join("tools/codex-acp").display()
    );
    fs::write(d.join("config/loop-manager/config.toml"), config).unwrap();
    // Each harness: below its minimum, at it, and above it, with no upper bound.
    let codex = |v: &str| format!("@agentclientprotocol/codex-acp {v}");
    for [claude, codex] in [
        ["0.109.0".into(), codex("2.9.0")],
        ["0.109.1".into(), codex("2.8.9")],
        ["1.0.0".into(), codex("3.0.0")],
        ["0.110.0".into(), codex("2.10.0")],
    ] {
        adapter(&d.join("tools/claude"), &claude);
        adapter(&d.join("tools/codex-acp"), &codex);
        let (passed, found) = doctor(d);
        assert!(passed, "warnings do not fail: {found:?}");
        let stderr = daemon_stderr(d).await;
        for (name, file, out, minimum, below) in [
            ("claude", "claude", &claude, "0.109.1", claude == "0.109.0"),
            ("codex", "codex-acp", &codex, "2.9.0", codex.ends_with("2.8.9")),
        ] {
            let path = d.join("tools").join(file).display().to_string();
            let warning = format!("{path} is `{out}`; supported: {minimum} or newer");
            let (level, detail) = level(&found, name);
            if below {
                assert_eq!((level, detail), ("warning", warning.as_str()), "{found:?}");
                assert!(stderr.contains(&format!("warning: {warning}\n")), "{stderr}");
            } else {
                assert_eq!(
                    (level, detail),
                    ("ok", format!("{path} `{out}` (from config)").as_str())
                );
                assert!(!stderr.contains(&path), "{stderr}");
            }
        }
    }
}

#[test]
fn a_tracker_project_gets_the_br_version_and_its_license_warning() {
    let dir = tree("");
    fs::create_dir_all(dir.path().join("config/loop-manager/shop/.beads")).unwrap();
    script(&dir.path().join("bin/br"), "echo 'br 0.7.5'");
    let (passed, found) = doctor(dir.path());
    assert!(passed, "warnings do not fail: {found:?}");
    let (br, why) = level(&found, "br");
    assert!(br == "warning" && why.contains("tested: 0.7.6"), "{found:?}");
    assert!(
        why.starts_with(&dir.path().join("bin/br").display().to_string()),
        "found on PATH: {why}"
    );
    assert!(level(&found, "br license").1.contains("rider"));
}
