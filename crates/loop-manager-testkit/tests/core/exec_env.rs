//! The environment of an exec step: `[exec]` of the orchestrator and the project, the step's `inherit_env` and `env`,
//! `${NAME}` references to the daemon's variables (docs/design/config.md, "Environment values"). Real commands, so
//! real time. The test process has `CARGO_PKG_NAME`, set by cargo, and no `LOOP_MANAGER_TEST_NO_SUCH_VARIABLE`.

use crate::common;

use common::{World, world_with};
use loop_manager_core::run::{Create, ExecHooks, Msg};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

const ORCHESTRATOR: &str =
    "projects = [\"shop\"]\nbuiltin = { default-policy = { enabled = false } }\n[harness.scripted]\n";

/// A world whose formula `check` runs `sh -c <script>` with the step keys `step`, the orchestrator's `[exec]` being
/// `top` and the project's `own`.
fn world_of(top: &str, own: &str, step: &str, script: &str) -> World {
    world_hooked(top, own, step, script, "", &[])
}

/// [`world_of`] with `addons` in the orchestrator's config and the files `extra`; hooks run as commands.
fn world_hooked(top: &str, own: &str, step: &str, script: &str, addons: &str, extra: &[(&str, &str)]) -> World {
    let config = format!("{ORCHESTRATOR}{addons}[exec]\n{top}\n");
    let project = format!("[rigs.web]\npath = \"web\"\n[exec]\n{own}\n");
    let formula = format!(
        "name = \"check\"\n[[step]]\nkind = \"exec\"\ncommand = [\"sh\", \"-c\", {}]\ntimeout = \"5s\"\n{step}\n",
        json!(script)
    );
    let mut files = vec![
        ("config.toml", config.as_str()),
        ("shop/loop-manager.toml", project.as_str()),
        ("shop/formulas/check.toml", formula.as_str()),
    ];
    files.extend_from_slice(extra);
    world_with(&files, Arc::new(ExecHooks::default()))
}

impl World {
    async fn start_check(&self) -> String {
        self.start_of("check").await
    }

    async fn start_of(&self, formula: &str) -> String {
        let create = Create {
            project: "shop".into(),
            rig: Some("web".into()),
            formula: formula.into(),
            inputs: serde_json::Map::new(),
            correlation: None,
            depth: 0,
            parent_run: None,
            token: None,
            continue_from: None,
            budget: None,
        };
        let reply = self.core.send(None, Msg::Create(create)).await.unwrap();
        reply["run"].as_str().unwrap().to_owned()
    }

    /// The stdout of the command of a run of `check` that passed.
    async fn stdout(&self) -> (String, String) {
        self.stdout_of("check").await
    }

    /// The stdout of the last command of a run of `formula` that passed.
    async fn stdout_of(&self, formula: &str) -> (String, String) {
        let run = self.start_of(formula).await;
        let (kind, data) = self.repeat_end(&run).await;
        assert_eq!(kind, "run.passed", "{data}");
        let out = data["outcome"]["data"]["stdout"].as_str().unwrap().to_owned();
        (run, out)
    }
}

#[tokio::test]
async fn a_step_gets_literals_references_and_a_dollar() {
    let top =
        r#"env = { LIT = "plain", GH_TOKEN = "${CARGO_PKG_NAME}", PRICE = "$$5", BOTH = "$${X}${CARGO_PKG_NAME}" }"#;
    let w = world_of(
        top,
        "",
        "",
        r#"printf '%s %s %s %s' "$LIT" "$GH_TOKEN" "$PRICE" "$BOTH""#,
    );
    assert_eq!(
        w.stdout().await.1,
        "plain loop-manager-testkit $5 ${X}loop-manager-testkit"
    );
}

#[tokio::test]
async fn the_environment_is_clean_unless_inherit_env() {
    let script = r#"printf '%s %s' "${CARGO_PKG_NAME-unset}" "${PATH:+path}""#;
    let w = world_of("", "", "", script);
    assert_eq!(w.stdout().await.1, "unset path", "only PATH, HOME and LANG");
    let w = world_of("", "", "inherit_env = true", script);
    assert_eq!(w.stdout().await.1, "loop-manager-testkit path");
    // `env` is over what is inherited, and the variables of loop-manager over both.
    let script = r#"printf '%s %s' "$CARGO_PKG_NAME" "$LOOP_MANAGER_RUN""#;
    let step = "inherit_env = true\nenv = { CARGO_PKG_NAME = \"mine\", LOOP_MANAGER_RUN = \"mine\" }";
    let w = world_of("", "", step, script);
    let (run, out) = w.stdout().await;
    assert_eq!(out, format!("mine {run}"));
}

#[tokio::test]
async fn the_step_is_over_the_project_over_the_orchestrator() {
    let top = "inherit_env = true\nenv = { A = \"top\", B = \"top\", C = \"top\" }";
    let own = "inherit_env = false\nenv = { B = \"own\", C = \"own\" }";
    let script = r#"printf '%s %s %s %s' "$A" "$B" "$C" "${CARGO_PKG_NAME-unset}""#;
    let w = world_of(top, own, "env = { C = \"step\" }", script);
    assert_eq!(w.stdout().await.1, "top own step unset");
    // The scalar is the strongest layer's that says it.
    let w = world_of(top, "", "", script);
    assert_eq!(w.stdout().await.1, "top top top loop-manager-testkit");
}

#[tokio::test]
async fn a_variable_the_daemon_lacks_fails_the_step_as_config_error() {
    let step = "env = { GH_TOKEN = \"tok-${LOOP_MANAGER_TEST_NO_SUCH_VARIABLE}-en\" }";
    let w = world_of("", "", step, "touch ran");
    let run = w.start_check().await;
    let failed = w.until(Some(&run), "run.attempt_failed", 0).await;
    let failure = &failed["failure"];
    assert_eq!(failure["kind"], "config_error", "{failed}");
    let text = failure["text"].as_str().unwrap();
    assert!(
        text.contains("`GH_TOKEN`") && text.contains("`LOOP_MANAGER_TEST_NO_SUCH_VARIABLE`"),
        "{text}"
    );
    assert!(!text.contains("tok-"), "the value is not quoted: {text}");
    let (kind, data) = w.repeat_end(&run).await;
    assert_eq!(
        (kind.as_str(), &data["class"]),
        ("run.failed", &json!("infrastructure")),
        "{data}"
    );
    assert!(!w.dir.path().join("shop/web/ran").exists(), "the command did not run");
}

#[tokio::test]
async fn the_variables_of_loop_manager_win_over_env() {
    let top = r#"env = { LOOP_MANAGER_RUN = "mine", TRACKER_ACTOR = "mine" }"#;
    let w = world_of(top, "", "", r#"printf '%s %s' "$LOOP_MANAGER_RUN" "$TRACKER_ACTOR""#);
    let (run, out) = w.stdout().await;
    assert_eq!(out, format!("{run} loop-manager:{run}:1"));
}

/// Waits up to 10 s for `found`.
async fn wait_for<T>(mut found: impl FnMut() -> Option<T>) -> T {
    let started = Instant::now();
    loop {
        if let Some(value) = found() {
            return value;
        }
        assert!(started.elapsed() < Duration::from_secs(10), "it did not come");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn the_snapshot_and_the_effect_keep_the_reference() {
    // The command waits for the test to read its effect, which is gone once it ends.
    let script = r#"until [ -e go ]; do sleep 0.02; done; printf %s "$GH_TOKEN""#;
    let w = world_of("", "", "env = { GH_TOKEN = \"${CARGO_PKG_NAME}\" }", script);
    let run = w.start_check().await;
    let db = rusqlite::Connection::open(w.dir.path().join("state.db")).unwrap();
    let query = |sql: &str| db.query_row(sql, [&run], |r| r.get::<_, String>(0)).ok();
    let effect = wait_for(|| query("SELECT payload FROM effects WHERE key = 'exec:' || ?1")).await;
    let effect: Value = serde_json::from_str(&effect).unwrap();
    assert_eq!(effect["env"]["GH_TOKEN"], "${CARGO_PKG_NAME}", "{effect}");
    let config = query("SELECT config FROM runs WHERE id = ?1").unwrap();
    let config: Value = serde_json::from_str(&config).unwrap();
    assert_eq!(config["env"]["GH_TOKEN"]["value"], "${CARGO_PKG_NAME}", "{config}");
    std::fs::write(w.dir.path().join("shop/web/go"), "").unwrap();
    let (kind, data) = w.repeat_end(&run).await;
    assert_eq!(kind, "run.passed", "{data}");
    assert_eq!(data["outcome"]["data"]["stdout"], "loop-manager-testkit");
}

#[tokio::test]
async fn a_dollar_from_a_hook_reaches_the_command_as_it_is() {
    let addons = "[settings]\naddons = [{ name = \"policy\" }]\n[[addon]]\npath = \"policy\"\n";
    let manifest = "name = \"policy\"\nversion = \"0.1.0\"\n[[hook]]\non = \"run.preparing\"\n\
        command = [\"sh\", \"prepare.sh\"]\ntimeout = \"5s\"\n";
    let prepare = r#"echo '{"decision":"proceed","env":{"X":"a$b${Y}"}}'"#;
    let extra = [("policy/addon.toml", manifest), ("policy/prepare.sh", prepare)];
    let w = world_hooked("", "", "", r#"printf %s "$X""#, addons, &extra);
    assert_eq!(w.stdout().await.1, "a$b${Y}");
}

/// The `bin/` of a declared pack is ahead of `PATH` for the exec steps and the check scripts of its formulas, over
/// the daemon's `PATH` and over an `env.PATH`; the project root's own `bin/` is on no `PATH` (docs/design/config.md
/// "Pack").
#[tokio::test]
async fn the_bin_of_a_pack_is_ahead_of_the_path_of_its_exec_steps_and_check_scripts() {
    let project = "packs = [\"../kit\"]\n[rigs.web]\npath = \"web\"\n";
    // The command prints how the step before it ended, then the first and the last directory of its `PATH`.
    let greet =
        "#!/bin/sh\nprintf '%s %s %s' \"${LOOP_MANAGER_STEP_PLAIN_RESULT-first}\" \"${PATH%%:*}\" \"${PATH##*:}\"\n";
    let formula = "name = \"greet\"\n\
        [[step]]\nname = \"plain\"\nkind = \"exec\"\ncommand = [\"greet.sh\"]\ntimeout = \"5s\"\n\
        [[step]]\nname = \"custom\"\nkind = \"exec\"\ncommand = [\"greet.sh\"]\ntimeout = \"5s\"\n\
        env = { PATH = \"/custom\" }\nstep_check_script_path = \"check.sh\"\n";
    let own = "name = \"check\"\n[[step]]\nkind = \"exec\"\ntimeout = \"5s\"\n\
        command = [\"sh\", \"-c\", \"command -v greet.sh || echo none\"]\n";
    let files = [
        ("config.toml", ORCHESTRATOR),
        ("shop/loop-manager.toml", project),
        ("shop/formulas/check.toml", own),
        ("shop/bin/greet.sh", greet),
        ("kit/pack.toml", "name = \"kit\"\nversion = \"0.1.0\"\n"),
        ("kit/bin/greet.sh", greet),
        ("kit/formulas/greet.toml", formula),
        ("kit/formulas/check.sh", "#!/bin/sh\ngreet.sh > \"$0.out\"\n"),
    ];
    let w = world_with(&files, Arc::new(ExecHooks::default()));
    let bin = w.dir.path().join("kit/bin");
    let bin = bin.to_str().unwrap();
    let (run, out) = w.stdout_of("kit.greet").await;
    assert_eq!(out, format!("pass {bin} /custom"), "ahead of the step's `env.PATH`");
    let first = w.all("run.step_finished").into_iter().find(|d| d["run"]["id"] == run);
    let first = first.unwrap()["outcome"]["data"]["stdout"].clone();
    let daemon = std::env::var("PATH").unwrap();
    let last = daemon.rsplit(':').next().unwrap();
    assert_eq!(first, format!("first {bin} {last}"), "ahead of the daemon's `PATH`");
    let check = std::fs::read_to_string(w.dir.path().join("kit/formulas/check.sh.out")).unwrap();
    assert_eq!(
        check,
        format!("pass {bin} /custom"),
        "the check script finds the command too"
    );
    assert_eq!(
        w.stdout_of("check").await.1,
        "none\n",
        "the root's `bin/` is on no `PATH`"
    );
}
