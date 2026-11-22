use super::*;
use crate::{Acp, Driver, ProcessPart, SessionRef};
use loop_manager_contract::driver::FailureKind::*;

fn sourced(value: Value) -> Value {
    json!({ "value": value, "from": { "layer": "agent" } })
}

fn native(value: Value) -> loop_manager_contract::config::Sourced<Value> {
    serde_json::from_value(sourced(value)).unwrap()
}

/// An agent with a role, a model and an effort, a hook, a skill, a subagent, a service and a stdio server
/// (docs/design/adapters.md, "Profiles").
fn typical(inherit: bool) -> EffectiveConfig {
    let target = json!({
        "harness": "codex", "description": sourced(json!("Checks the work.")),
        "system_prompt": sourced(json!("You check the work.")), "model": sourced(json!("gpt-6-luna")),
        "effort": sourced(json!("low")), "inherit_host": null, "env": {}, "skills": {}, "addons": [], "mcp": {},
        "hooks": {},
    });
    serde_json::from_value(json!({
        "harness": "codex", "model": sourced(json!("gpt-6.1-terra")), "effort": sourced(json!("high")),
        "system_prompt": sourced(json!("<role prompt>")), "inherit_host": sourced(json!(inherit)), "env": {},
        "addons": [],
        "mcp": {
            "loop-manager": sourced(json!({ "url": "http://127.0.0.1:41000/mcp",
                "headers": { "Authorization": "Bearer <attempt token>" } })),
            "mail": sourced(json!({ "command": ["mail-mcp", "serve"], "env": { "MAIL_ROOM": "dev" } })),
        },
        "hooks": { "dcg": sourced(json!({ "event": "PreToolUse", "matcher": "Bash", "command": "dcg check",
            "timeout": "10s" })) },
        "skills": { "kit": sourced(json!({ "path": "/skills/kit" })) },
        "native": sourced(json!({ "sandbox_mode": "danger-full-access", "approval_policy": "never" })),
        "targets": { "kit.qa": target },
    }))
    .unwrap()
}

/// `typical` without skills.
fn bare(inherit: bool) -> EffectiveConfig {
    let mut config = typical(inherit);
    config.skills.clear();
    config
}

fn plan() -> SessionPlan {
    SessionPlan::New {
        workdir: PathBuf::from("/work/rig"),
        run: "r1".into(),
        attempt: 1,
        token: "t0ken-of-the-attempt".into(),
        actor: Some(("BD_ACTOR".into(), "loop-manager:r1:1".into())),
        bead: Some("shop-1".into()),
    }
}

/// A resume of the session of `plan` in attempt 2, its `CODEX_HOME` `home`.
fn resume(home: Option<&str>) -> SessionPlan {
    let mut session = json!({ "session_id": "s1", "cwd": "/work/rig" });
    if let Some(home) = home {
        session["home"] = json!(home);
    }
    SessionPlan::Resume {
        session: SessionRef(session),
        workdir: PathBuf::from("/work/rig"),
        run: "r1".into(),
        attempt: 2,
        token: "t2".into(),
        actor: None,
        bead: None,
    }
}

/// The variable of the key of the host's provider: one the test runner sets.
const HOST_KEY: &str = "CARGO_MANIFEST_DIR";

/// A host codex home whose config selects the provider `corp`, with keys that hold secrets.
fn host() -> tempfile::TempDir {
    let host = tempfile::tempdir().unwrap();
    let config = format!(
        "model_provider = \"corp\"\n[model_providers.corp]\nname = \"corp\"\nbase_url = \"https://llm.corp/v1\"\n\
         env_key = \"{HOST_KEY}\"\nexperimental_bearer_token = \"bearer-secret\"\nhttp_headers = {{ X-Key = \"header-secret\" }}\n"
    );
    std::fs::write(host.path().join("config.toml"), config).unwrap();
    host
}

/// The process and session parts of `config` in `state` with the host's codex home `host`.
fn split_in(config: &EffectiveConfig, plan: &SessionPlan, state: &Path, host: &Path) -> (Value, Value) {
    let setup = (Path::new("codex-acp"), &[][..], &Map::new());
    let p = process_with("codex", setup, config, plan, state, host).unwrap();
    let mut s = part::session(config, plan).unwrap();
    session_with(config, plan, state, &mut s, host).unwrap();
    (p, s)
}

fn split(config: &EffectiveConfig, plan: &SessionPlan, host: &Path) -> (Value, Value) {
    split_in(config, plan, Path::new("/state"), host)
}

fn value<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
    env.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// The role file of `kit.qa` in `p`: its path and its text as a table.
fn role(p: &Value) -> (String, toml::Table) {
    let roles = p["roles"].as_object().unwrap();
    assert_eq!(roles.len(), 1, "{roles:?}");
    let (path, text) = roles.iter().next().unwrap();
    (path.clone(), toml::from_str(text.as_str().unwrap()).unwrap())
}

#[test]
fn an_isolated_session_gets_its_codex_config_and_homes_in_the_environment() {
    let host = host();
    let (p, s) = split(&typical(false), &plan(), host.path());
    let (path, text) = role(&p);
    assert!(
        path.starts_with("/state/acp/codex/roles/") && path.ends_with(".toml"),
        "{path}"
    );
    let expected: toml::Table = toml::from_str(
        "developer_instructions = \"You check the work.\"\nmodel = \"gpt-6-luna\"\nmodel_reasoning_effort = \"low\"\n\
         [[skills.config]]\npath = \"/skills/kit/SKILL.md\"\nenabled = false\n",
    )
    .unwrap();
    assert_eq!(text, expected);
    let config = json!({
        "notify": [],
        "model_provider": "corp",
        "model_providers": { "corp": { "name": "corp", "base_url": "https://llm.corp/v1", "env_key": HOST_KEY } },
        "model": "gpt-6.1-terra",
        "model_reasoning_effort": "high",
        "shell_environment_policy": { "set": {
            "LOOP_MANAGER_RUN": "r1", "LOOP_MANAGER_ATTEMPT_TOKEN": "t0ken-of-the-attempt",
            "BD_ACTOR": "loop-manager:r1:1", "LOOP_MANAGER_BEAD": "shop-1" } },
        "mcp_servers": {
            "loop-manager": { "url": "http://127.0.0.1:41000/mcp",
                "http_headers": { "Authorization": "Bearer <attempt token>" }, "required": true },
            "mail": { "command": "mail-mcp", "args": ["serve"], "env": { "MAIL_ROOM": "dev" }, "required": true },
        },
        "hooks": { "PreToolUse": [{ "matcher": "Bash",
            "hooks": [{ "type": "command", "command": "dcg check", "timeout": 10 }] }] },
        "bypass_hook_trust": true,
        "agents": { "kit.qa": { "description": "Checks the work.", "config_file": path } },
    });
    assert_eq!(p["config"], config);
    let files = "/state/acp/codex/r1-1";
    let fields = ["home", "user_home", "host", "host_key", "skills", "login"].map(|k| p[k].clone());
    let expected = [
        json!(format!("{files}/codex-home")),
        json!(format!("{files}/home")),
        json!(host.path()),
        json!(HOST_KEY),
        json!({ "kit": "/skills/kit" }),
        json!(false),
    ];
    assert_eq!(fields, expected);
    let env = super::env(&p, "i1", None).unwrap();
    for (key, expected) in [
        ("CODEX_HOME", Some(format!("{files}/codex-home"))),
        ("HOME", Some(format!("{files}/home"))),
        (HOST_KEY, std::env::var(HOST_KEY).ok()),
        ("LOOP_MANAGER_RUN", Some("r1".into())),
        ("LOOP_MANAGER_ATTEMPT_TOKEN", Some("t0ken-of-the-attempt".into())),
        ("BD_ACTOR", Some("loop-manager:r1:1".into())),
        ("LOOP_MANAGER_BEAD", Some("shop-1".into())),
        (loop_manager_contract::orphans::INSTANCE_ENV, Some("i1".into())),
        ("SHELL", std::env::var("SHELL").ok()),
        ("DEFAULT_AUTH_REQUEST", None),
    ] {
        assert_eq!(value(&env, key), expected.as_deref(), "{key}");
    }
    assert_eq!(env.iter().filter(|(k, _)| k == "HOME").count(), 1);
    let delivered: Value = serde_json::from_str(value(&env, "CODEX_CONFIG").unwrap()).unwrap();
    assert_eq!(delivered, config);
    for key in HOST_NETWORK_ENV {
        assert_eq!(value(&env, key), None, "{key}");
    }
    // The session part: no MCP servers, model or effort in ACP; the mode, the home and the servers' secrets beside.
    let expected = json!({
        "cwd": "/work/rig", "mcpServers": [],
        "systemPrompt": { "mode": "append", "content": [{ "type": "text", "text": "<role prompt>" }] },
        "model": null, "effort": null, "run": "r1", "attempt": 1,
        "mode": "agent-full-access", "home": format!("{files}/codex-home"),
        "secrets": ["Bearer <attempt token>", "dev"],
    });
    assert_eq!(s, expected);
    assert_eq!(secrets(&s), ["Bearer <attempt token>", "dev"]);
}

#[test]
fn a_session_that_inherits_the_host_runs_in_the_hosts_home_and_reads_its_provider_itself() {
    let host = host();
    let (p, s) = split(&bare(true), &plan(), host.path());
    let fields = ["home", "user_home", "host", "host_key", "login"].map(|k| p[k].clone());
    let expected = [
        json!(host.path()),
        json!(null),
        json!(null),
        json!(HOST_KEY),
        json!(false),
    ];
    assert_eq!(fields, expected);
    assert!(p["config"].get("model_provider").is_none(), "{}", p["config"]);
    assert!(p["config"].get("model_providers").is_none(), "{}", p["config"]);
    assert_eq!(s["home"], Value::Null);
    let env = super::env(&p, "i1", None).unwrap();
    assert_eq!(value(&env, "CODEX_HOME"), host.path().to_str());
    for key in HOST_NETWORK_ENV.iter().chain(&["HOME"]) {
        assert_eq!(value(&env, key), std::env::var(key).ok().as_deref(), "{key}");
    }
    assert_eq!(value(&env, HOST_KEY), std::env::var(HOST_KEY).ok().as_deref());
    // With skills it gets a home of its own that links the host's, and keeps the host's `HOME`.
    let (p, s) = split(&typical(true), &plan(), host.path());
    let home = "/state/acp/codex/r1-1/codex-home";
    let fields = ["home", "user_home", "host"].map(|k| p[k].clone());
    assert_eq!(fields, [json!(home), json!(null), json!(host.path())]);
    assert_eq!(s["home"], home);
    let env = super::env(&p, "i1", None).unwrap();
    assert_eq!(value(&env, "CODEX_HOME"), Some(home));
    assert_eq!(value(&env, "HOME"), std::env::var("HOME").ok().as_deref());
    assert!(p["config"].get("model_provider").is_none(), "{}", p["config"]);
}

#[test]
fn only_an_isolated_session_with_a_key_of_its_own_logs_in_by_it() {
    let host = host();
    let with = |inherit: bool, key: Option<&str>| {
        let mut config = typical(inherit);
        if let Some(key) = key {
            config.env.insert(
                key.into(),
                serde_json::from_value(sourced(json!("sk-literal"))).unwrap(),
            );
        }
        let (p, _) = split(&config, &plan(), host.path());
        let env = super::env(&p, "i1", None).unwrap();
        (p, value(&env, "DEFAULT_AUTH_REQUEST").map(str::to_owned))
    };
    for key in CREDENTIALS {
        let (p, login) = with(false, Some(key));
        assert_eq!(login.as_deref(), Some(r#"{"methodId":"api-key"}"#), "{key}");
        // Its key, not the host's login: no link to the host's `auth.json`, which codex would write through, and no
        // provider of the host.
        let fields = ["host", "host_key"].map(|k| p[k].clone());
        assert_eq!(fields, [json!(null), json!(null)], "{key}");
        assert!(p["config"].get("model_provider").is_none(), "{}", p["config"]);
    }
    assert_eq!(with(false, None).1, None);
    assert_eq!(
        with(true, Some("OPENAI_API_KEY")).1,
        None,
        "the host's files are not the session's"
    );
    assert_eq!(with(false, Some("OTHER_KEY")).1, None);
}

#[test]
fn the_hosts_provider_comes_without_its_secrets_and_under_the_native_block() {
    let host = host();
    let (p, _) = split(&typical(false), &plan(), host.path());
    let text = p["config"].to_string();
    assert!(!text.contains("secret"), "{text}");
    assert!(p["config"]["model_providers"]["corp"].get("http_headers").is_none());
    assert!(
        p["config"]["model_providers"]["corp"]
            .get("experimental_bearer_token")
            .is_none()
    );
    // The native block's own provider wins, as its keys win over the host's.
    let mut config = typical(false);
    config.native = Some(native(json!({ "model_provider": "fake",
        "model_providers": { "fake": { "base_url": "http://127.0.0.1:1/v1" } } })));
    let (p, _) = split(&config, &plan(), host.path());
    assert_eq!(p["config"]["model_provider"], "fake");
    assert_eq!(
        p["config"]["model_providers"],
        json!({ "fake": { "base_url": "http://127.0.0.1:1/v1" } })
    );
    // No host config, no provider and no key.
    let empty = tempfile::tempdir().unwrap();
    let (p, _) = split(&typical(false), &plan(), empty.path());
    assert_eq!(p["host_key"], Value::Null);
    assert!(p["config"].get("model_provider").is_none());
}

#[test]
fn the_native_block_goes_under_what_loop_manager_sets_and_merges_by_table() {
    let mut config = typical(false);
    config.native = Some(native(json!({
        "sandbox_mode": "workspace-write", "approval_policy": "on-request", "model": "gpt-other",
        "analytics": { "enabled": true }, "web_search": "live",
        "shell_environment_policy": { "inherit": "core", "set": { "VENDOR": "1", "LOOP_MANAGER_RUN": "stale" } },
        "mcp_servers": { "mail": { "command": "old-mail" }, "docs": { "url": "http://docs" } },
        "hooks": { "PreToolUse": [{ "matcher": "Edit", "hooks": [] }] },
        "agents": { "reviewer": { "description": "Reviews." } },
    })));
    let empty = tempfile::tempdir().unwrap();
    let (p, s) = split(&config, &plan(), empty.path());
    let c = &p["config"];
    assert!(
        c.get("sandbox_mode").is_none() && c.get("approval_policy").is_none(),
        "the mode carries them: {c}"
    );
    assert_eq!(s["mode"], "workspace-write");
    assert_eq!(
        (&c["model"], &c["analytics"], &c["web_search"]),
        (&json!("gpt-6.1-terra"), &json!({ "enabled": true }), &json!("live"))
    );
    let policy = &c["shell_environment_policy"];
    assert_eq!(policy["inherit"], "core");
    assert_eq!(
        (&policy["set"]["VENDOR"], &policy["set"]["LOOP_MANAGER_RUN"]),
        (&json!("1"), &json!("r1"))
    );
    assert_eq!(c["mcp_servers"]["mail"]["command"], "mail-mcp");
    assert_eq!(c["mcp_servers"]["docs"], json!({ "url": "http://docs" }));
    let matchers: Vec<&Value> = c["hooks"]["PreToolUse"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| &m["matcher"])
        .collect();
    assert_eq!(matchers, [&json!("Edit"), &json!("Bash")]);
    assert_eq!(c["agents"]["reviewer"], json!({ "description": "Reviews." }));
    assert_eq!(c["agents"]["kit.qa"]["description"], "Checks the work.");
    // Nothing to add, nothing written: no servers, hooks, bypass or agents of an agent without them.
    let mut plain = bare(false);
    (plain.mcp, plain.hooks, plain.targets, plain.native) = Default::default();
    let (p, _) = split(&plain, &plan(), empty.path());
    for key in ["mcp_servers", "hooks", "bypass_hook_trust", "agents"] {
        assert!(p["config"].get(key).is_none(), "{key}: {}", p["config"]);
    }
    assert_eq!(p["roles"], json!({}));
    // A native key loop-manager adds to that is no table is a config error.
    config.native = Some(native(json!({ "shell_environment_policy": "all" })));
    let setup = (Path::new("codex-acp"), &[][..], &Map::new());
    let failed = process_with("codex", setup, &config, &plan(), Path::new("/state"), empty.path()).unwrap_err();
    assert_eq!(failed.kind, ConfigError);
}

#[test]
fn the_mode_by_the_approval_policy_and_the_sandbox() {
    let ok = |native: Value, inherited: Option<&str>| mode("codex", native.as_object(), inherited);
    for (native, inherited, expected) in [
        (json!({}), None, "agent-full-access"),
        (
            json!({ "approval_policy": "never", "sandbox_mode": "danger-full-access" }),
            None,
            "agent-full-access",
        ),
        (json!({ "sandbox_mode": "workspace-write" }), None, "workspace-write"),
        (
            json!({ "approval_policy": "on-request", "sandbox_mode": "workspace-write" }),
            None,
            "workspace-write",
        ),
        (json!({ "sandbox_mode": "read-only" }), None, "read-only"),
        (
            json!({ "approval_policy": "on-request", "sandbox_mode": "read-only" }),
            None,
            "read-only",
        ),
        // The host's sandbox of a session that inherits it, unless the block names one.
        (json!({}), Some("workspace-write"), "workspace-write"),
        (
            json!({ "approval_policy": "on-request" }),
            Some("read-only"),
            "read-only",
        ),
        (
            json!({ "sandbox_mode": "danger-full-access" }),
            Some("read-only"),
            "agent-full-access",
        ),
    ] {
        assert_eq!(ok(native.clone(), inherited), Ok(expected), "{native} {inherited:?}");
    }
    assert_eq!(mode("codex", None, None), Ok("agent-full-access"));
    let approval = "`native.codex.approval_policy`: the ACP harness `codex` offers `never` with `danger-full-access`, \
                    and `never` or `on-request` with `workspace-write` or `read-only`";
    for native in [
        json!({ "approval_policy": "on-request", "sandbox_mode": "danger-full-access" }),
        json!({ "approval_policy": "on-request" }),
        json!({ "approval_policy": "untrusted", "sandbox_mode": "read-only" }),
        json!({ "approval_policy": "on-failure", "sandbox_mode": "workspace-write" }),
        json!({ "approval_policy": { "granular": { "sandbox_approval": true } }, "sandbox_mode": "read-only" }),
    ] {
        assert_eq!(ok(native.clone(), None).unwrap_err(), approval, "{native}");
    }
    let sandbox = ok(json!({ "sandbox_mode": "full" }), None).unwrap_err();
    assert!(sandbox.starts_with("`native.codex.sandbox_mode`: "), "{sandbox}");
    let host = ok(json!({}), Some("full")).unwrap_err();
    assert!(host.starts_with("the host's codex `sandbox_mode`: "), "{host}");
}

#[test]
fn what_no_mode_stands_for_is_refused_by_its_key_when_the_config_loads_and_at_the_attempt() {
    let mut config = typical(true);
    assert_eq!(crate::unsupported("codex", &config), Ok(()));
    for (block, key) in [
        (json!("never"), "`native.codex`: "),
        (
            json!({ "approval_policy": "untrusted" }),
            "`native.codex.approval_policy`: ",
        ),
        (
            json!({ "approval_policy": 1, "sandbox_mode": "read-only" }),
            "`native.codex.approval_policy`: ",
        ),
        (
            json!({ "approval_policy": "on-request" }),
            "`native.codex.approval_policy`: ",
        ),
        (
            json!({ "approval_policy": "on-request", "sandbox_mode": "danger-full-access" }),
            "`native.codex.approval_policy`: ",
        ),
        (json!({ "sandbox_mode": "none" }), "`native.codex.sandbox_mode`: "),
        (
            json!({ "default_permissions": "dev" }),
            "`native.codex.default_permissions`: ",
        ),
        (
            json!({ "approvals_reviewer": "auto_review" }),
            "`native.codex.approvals_reviewer`: ",
        ),
    ] {
        config.native = Some(native(block.clone()));
        let refused = crate::unsupported("codex", &config).unwrap_err();
        assert!(refused.starts_with(key), "{block}: {refused}");
        let setup = (Path::new("codex-acp"), &[][..], &Map::new());
        let failed = process_with(
            "codex",
            setup,
            &config,
            &plan(),
            Path::new("/state"),
            Path::new("/none"),
        );
        assert_eq!(failed.unwrap_err().kind, ConfigError, "{block}");
    }
    for block in [
        json!({ "approval_policy": "on-request", "sandbox_mode": "workspace-write" }),
        json!({ "approval_policy": "never", "sandbox_mode": "read-only" }),
        json!({ "model_provider": "fake" }),
    ] {
        config.native = Some(native(block.clone()));
        assert_eq!(crate::unsupported("codex", &config), Ok(()), "{block}");
    }
    // A sandbox of the host no mode stands for fails the attempt of a session that inherits it.
    let host = tempfile::tempdir().unwrap();
    std::fs::write(host.path().join("config.toml"), "sandbox_mode = \"full\"\n").unwrap();
    config.native = None;
    let mut s = part::session(&config, &plan()).unwrap();
    let failed = session_with(&config, &plan(), Path::new("/state"), &mut s, host.path()).unwrap_err();
    assert_eq!(failed.kind, ConfigError);
    assert!(failed.raw.contains("the host's codex `sandbox_mode`"), "{}", failed.raw);
    // An isolated one does not read it.
    config.inherit_host = serde_json::from_value(sourced(json!(false))).unwrap();
    session_with(&config, &plan(), Path::new("/state"), &mut s, host.path()).unwrap();
    assert_eq!(s["mode"], "agent-full-access");
}

#[test]
fn a_resume_takes_the_homes_of_the_session() {
    let host = host();
    let home = "/state/acp/codex/r1-1/codex-home";
    let (p, s) = split(&typical(false), &resume(Some(home)), host.path());
    assert_eq!(
        (&p["home"], &p["user_home"], &s["home"]),
        (&json!(home), &json!("/state/acp/codex/r1-1/home"), &json!(home))
    );
    let env = super::env(&p, "i1", None).unwrap();
    assert_eq!(value(&env, "CODEX_HOME"), Some(home));
    assert_eq!(value(&env, "HOME"), Some("/state/acp/codex/r1-1/home"));
    assert_eq!(
        p["config"]["shell_environment_policy"]["set"]["LOOP_MANAGER_ATTEMPT_TOKEN"],
        "t2"
    );
    let (inheriting, s) = split(&typical(true), &resume(Some(home)), host.path());
    assert_eq!(
        (&inheriting["home"], &inheriting["user_home"], &s["home"]),
        (&json!(home), &json!(null), &json!(home))
    );
    // A session that ran in the host's home resumes there.
    let (p, s) = split(&bare(true), &resume(None), host.path());
    assert_eq!((&p["home"], &s["home"]), (&json!(host.path()), &json!(null)));
}

/// A host codex home with a login, instructions and skills, codex's own cache among them.
fn full_host() -> tempfile::TempDir {
    let host = host();
    for file in ["auth.json", "AGENTS.md"] {
        std::fs::write(host.path().join(file), "").unwrap();
    }
    for dir in ["skills/notes", "skills/kit", "skills/.system"] {
        std::fs::create_dir_all(host.path().join(dir)).unwrap();
    }
    host
}

fn skill(path: &Path) -> loop_manager_contract::config::Sourced<loop_manager_contract::config::Skill> {
    serde_json::from_value(sourced(json!({ "path": path }))).unwrap()
}

/// A new session of run `run`.
fn plan_of(run: &str) -> SessionPlan {
    SessionPlan::New {
        workdir: "/w".into(),
        run: run.into(),
        attempt: 1,
        token: "t".into(),
        actor: None,
        bead: None,
    }
}

#[test]
fn the_start_makes_the_homes_the_links_and_the_role_files_and_a_second_rebuilds_the_skills() {
    use std::os::unix::fs::PermissionsExt;
    let (host, state, kit, mail) = (
        full_host(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let mut config = typical(false);
    config.skills.insert("kit".into(), skill(kit.path()));
    let (p, _) = split_in(&config, &plan(), state.path(), host.path());
    start(&p).unwrap();
    let files = state.path().join("acp/codex/r1-1");
    let home = files.join("codex-home");
    for dir in [&home, &files.join("home"), &state.path().join("acp/codex/roles")] {
        assert_eq!(
            std::fs::metadata(dir).unwrap().permissions().mode() & 0o777,
            0o700,
            "{}",
            dir.display()
        );
    }
    assert_eq!(std::fs::read_link(home.join("skills/kit")).unwrap(), kit.path());
    assert_eq!(
        std::fs::read_link(home.join("auth.json")).unwrap(),
        host.path().join("auth.json")
    );
    for absent in ["AGENTS.md", "skills/notes"] {
        assert!(
            home.join(absent).symlink_metadata().is_err(),
            "an isolated home links no {absent}"
        );
    }
    // Its own `config.toml`, no link to the host's: the keys codex reads only when its app-server starts.
    let own = home.join("config.toml");
    let meta = std::fs::symlink_metadata(&own).unwrap();
    assert!(meta.is_file(), "a file of its own");
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    let written: toml::Table = toml::from_str(&std::fs::read_to_string(&own).unwrap()).unwrap();
    let expected: toml::Table = toml::from_str(
        "check_for_update_on_startup = false\n[analytics]\nenabled = false\n[feedback]\nenabled = false\n\
         [features]\nplugins = false\n",
    )
    .unwrap();
    assert_eq!(written, expected);
    let (path, text) = role(&p);
    let written: toml::Table = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(written, text);
    let leftovers = std::fs::read_dir(Path::new(&path).parent().unwrap()).unwrap().count();
    assert_eq!(leftovers, 1, "no temporary file stays");
    // The skills change, codex made its own cache, its config changed: the links follow, the cache stays.
    std::fs::create_dir_all(home.join("skills/.system")).unwrap();
    std::fs::write(&own, "[features]\nplugins = true\n").unwrap();
    config.skills.clear();
    config.skills.insert("mail".into(), skill(mail.path()));
    let (p, _) = split_in(&config, &plan(), state.path(), host.path());
    start(&p).unwrap();
    assert_eq!(std::fs::read_link(home.join("skills/mail")).unwrap(), mail.path());
    assert!(
        home.join("skills/kit").symlink_metadata().is_err(),
        "the dropped skill's link is gone"
    );
    assert!(home.join("skills/.system").is_dir());
    // A start writes it again over what was there.
    assert_eq!(std::fs::read_to_string(&own).unwrap(), STARTUP_CONFIG);
}

#[test]
fn the_start_links_the_hosts_files_and_skills_only_for_a_session_that_inherits_the_host() {
    let (host, state, kit) = (full_host(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    // A session that inherits the host: its home links the host's files and skills, its own skill first.
    let host_text = std::fs::read_to_string(host.path().join("config.toml")).unwrap();
    let mut config = typical(true);
    config.skills.insert("kit".into(), skill(kit.path()));
    let (p, _) = split_in(&config, &plan_of("r2"), state.path(), host.path());
    start(&p).unwrap();
    let home = state.path().join("acp/codex/r2-1/codex-home");
    for (link, target) in [
        ("auth.json", host.path().join("auth.json")),
        ("config.toml", host.path().join("config.toml")),
        ("AGENTS.md", host.path().join("AGENTS.md")),
        ("skills/kit", kit.path().to_owned()),
        ("skills/notes", host.path().join("skills/notes")),
    ] {
        assert_eq!(std::fs::read_link(home.join(link)).unwrap(), target, "{link}");
    }
    assert!(
        home.join("skills/.system").symlink_metadata().is_err(),
        "codex makes its own"
    );
    assert_eq!(
        std::fs::read_to_string(host.path().join("config.toml")).unwrap(),
        host_text,
        "the host's config.toml is linked as it is, never written"
    );
    assert!(
        state.path().join("acp/codex/r2-1/home").symlink_metadata().is_err(),
        "HOME stays the host's"
    );
    // An isolated session with a key of its own links nothing of the host.
    let mut config = typical(false);
    config.env.insert(
        "OPENAI_API_KEY".into(),
        serde_json::from_value(sourced(json!("sk"))).unwrap(),
    );
    let plan = plan_of("r3");
    let (p, _) = split_in(&config, &plan, state.path(), host.path());
    start(&p).unwrap();
    assert!(
        state
            .path()
            .join("acp/codex/r3-1/codex-home/auth.json")
            .symlink_metadata()
            .is_err()
    );
    // A session in the host's own home touches nothing there.
    let (p, _) = split_in(&bare(true), &plan, state.path(), host.path());
    start(&p).unwrap();
    let mut skills: Vec<_> = std::fs::read_dir(host.path().join("skills"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    skills.sort();
    assert_eq!(skills, [".system", "kit", "notes"]);
    assert_eq!(
        std::fs::read_to_string(host.path().join("config.toml")).unwrap(),
        host_text
    );
}

/// The accounts of the gates are those the native codex driver had: the same hashes of the same sources.
#[test]
fn the_account_is_the_native_drivers() {
    let acp = Acp::new(
        "codex",
        &toml::from_str("command = \"codex-acp\"").unwrap(),
        Path::new("/state"),
        None,
        None,
    )
    .unwrap();
    let account = |config: &EffectiveConfig| acp.account(&acp.split(config, &plan()).unwrap().process).unwrap();
    let mut config = typical(false);
    config.env.insert(
        "OPENAI_API_KEY".into(),
        serde_json::from_value(sourced(json!("${SHOP_KEY}"))).unwrap(),
    );
    assert_eq!(account(&config), "48279d768dc410d3");
    config.env.clear();
    config.env.insert(
        "CODEX_API_KEY".into(),
        serde_json::from_value(sourced(json!("${CODEX_KEY}"))).unwrap(),
    );
    assert_eq!(account(&config), "46ca438b65e6f521");
    // The host's login is the hash of the host's codex home, which this machine decides.
    let host = Fingerprint::of("codex", &ProcessPart(json!({ "host": host_home() }))).0[..16].to_owned();
    assert_eq!(account(&typical(false)), host);
    assert_eq!(account(&typical(true)), host, "inherited or linked, the host's login");
}

#[test]
fn the_failures_of_the_adapter_by_their_code_and_words() {
    let rows = [
        (-32000, "Authentication required", AuthRequired, false),
        (-32602, "Invalid params", ConfigError, false),
        (1001, "Connection to Codex was lost.", HarnessCrash, false),
        (
            -32603,
            "Quota exceeded. Check your plan and billing details.",
            Billing,
            false,
        ),
        (
            -32603,
            "To use Codex with your ChatGPT plan, upgrade to Plus",
            Billing,
            false,
        ),
        (-32603, "Your workspace is out of credits. Add credits.", Billing, false),
        (-32603, "You hit your spend cap for this month.", Billing, false),
        (
            -32603,
            "You\u{2019}ve hit your usage limit. Visit https://chatgpt.com/settings/usage to purchase more credits or try \
             again at 1:35 PM.",
            QuotaExhausted,
            false,
        ),
        (
            -32603,
            "Codex ran out of room in the model's context window.",
            ContextOverflow,
            false,
        ),
        (
            -32603,
            "Selected model is at capacity. Please try a different model.",
            Overloaded,
            true,
        ),
        (
            -32603,
            "exceeded retry limit, last status: 429 Too Many Requests",
            RateLimited,
            true,
        ),
        (
            -32603,
            "exceeded retry limit, last status: 503 Service Unavailable",
            Overloaded,
            true,
        ),
        (-32603, "exceeded retry limit, last status: 418", Unknown, false),
        (
            -32603,
            "unexpected status 429 Too Many Requests: slow down",
            RateLimited,
            false,
        ),
        (-32603, "unexpected status 408", Overloaded, true),
        (-32603, "unexpected status 500", Overloaded, true),
        (-32603, "unexpected status 504", Overloaded, true),
        (-32603, "unexpected status 529", Overloaded, true),
        (-32603, "unexpected status 505", Unknown, false),
        (-32603, "unexpected status 401 Unauthorized", AuthRequired, false),
        (-32603, "unexpected status 403", AuthRequired, false),
        (-32603, "unexpected status 400 Bad Request", ConfigError, false),
        (
            -32603,
            "unexpected status 404 Not Found: no model gpt-fake",
            ConfigError,
            false,
        ),
        (
            -32603,
            "The model `gpt-fake` does not exist or you do not have access to it.",
            ConfigError,
            false,
        ),
        (-32603, "unexpected status 402", Unknown, false),
        (-32603, "unexpected status", Unknown, false),
        (-32603, "something else went wrong", Unknown, false),
        (-32002, "Resource not found", Unknown, false),
    ];
    for (code, message, kind, retried) in rows {
        let failure = classify(code, message, None);
        assert_eq!(failure.kind, kind, "{code} {message}");
        assert_eq!(failure.scope, kind.default_scope(), "{message}");
        assert_eq!(failure.retried_by_harness, retried, "{message}");
        assert_eq!(failure.retry_at_ms, None, "{message}");
        assert_eq!(
            failure.text.as_deref(),
            (kind == Unknown).then_some(message),
            "{message}"
        );
        assert!(failure.raw.starts_with(&format!("{code}: ")), "{}", failure.raw);
    }
    // The words decide only after the code.
    assert_eq!(classify(-32000, "unexpected status 429", None).kind, AuthRequired);
}

/// A request that failed inside the adapter is "Internal error" with codex's words in `data.details`.
#[test]
fn the_words_of_a_request_that_failed_inside_the_adapter_are_its_details() {
    let details = "error creating thread: Fatal error: Failed to initialize session: required MCP servers failed to \
                   initialize: tools: No such file or directory (os error 2)";
    let failure = classify(-32603, "Internal error", Some(&json!({ "details": details })));
    assert_eq!(failure.kind, ToolFailure, "{}", failure.raw);
    assert!(failure.raw.contains("tools: No such file"), "{}", failure.raw);
    let failure = classify(-32603, "Internal error", Some(&json!({ "details": "boom" })));
    assert_eq!((failure.kind, failure.text.as_deref()), (Unknown, Some("boom")));
}

#[test]
fn the_driver_takes_the_codex_profile_and_refuses_the_keys_of_the_native_one() {
    let options: toml::Table = toml::from_str("command = \"codex-acp\"").unwrap();
    let driver = Acp::new("codex", &options, Path::new("/state"), None, None).unwrap();
    assert_eq!(driver.profile.name, "codex");
    let mut config = typical(false);
    config.env.insert(
        "OPENAI_API_KEY".into(),
        serde_json::from_value(sourced(json!("sk"))).unwrap(),
    );
    let split = driver.split(&config, &plan()).unwrap();
    assert_eq!(
        (&split.session.0["mode"], &split.session.0["mcpServers"]),
        (&json!("agent-full-access"), &json!([]))
    );
    assert_eq!(split.process.0["login"], true);
    for key in ["max_sessions", "instance_idle_s"] {
        let options: toml::Table = toml::from_str(&format!("command = \"codex-acp\"\n{key} = 4")).unwrap();
        let refused = Acp::new("codex", &options, Path::new("/state"), None, None)
            .err()
            .unwrap();
        assert_eq!(
            refused,
            format!("`{key}`: the key is gone: an ACP harness runs one process per session")
        );
    }
    let options: toml::Table = toml::from_str("command = \"claude-agent-acp\"\nmax_sessions = 4").unwrap();
    let refused = Acp::new("claude", &options, Path::new("/state"), None, None).err();
    assert_eq!(
        refused.as_deref(),
        Some("unknown key `max_sessions`"),
        "no key of claude's"
    );
    // The adapter of the build: the driver on the resolved binary, of the codex profile's minimum.
    let setup = loop_manager_contract::driver::Setup {
        binary: "/bin/codex-acp".into(),
        path: None,
        options: &toml::Table::new(),
        state: Path::new("/state"),
        socket: None,
        history: None,
    };
    let built = (crate::CODEX.new)(&setup).unwrap();
    assert_eq!(built.harness(), "codex");
    assert_eq!(built.capabilities(&ProcessPart(Value::Null)).min_version, (2, 9, 0));
    // Both adapters of the build are profiled: their `[harness.<name>]` takes the keys of an ACP harness.
    assert!(crate::profiled("codex") && crate::profiled("claude"));
}

#[test]
fn the_sandbox_warning_names_the_agents_without_one_and_the_mode() {
    let host = tempfile::tempdir().unwrap();
    let mut confined = typical(false);
    confined.native = Some(native(json!({ "sandbox_mode": "read-only" })));
    let mut open = typical(true);
    open.native = None;
    let agents = [
        ("shop/dev".to_owned(), Some(open)),
        ("shop/qa".to_owned(), Some(confined)),
        ("shop/broken".to_owned(), None),
    ];
    let check = sandbox(host.path(), &agents);
    assert_eq!(check.level, "warning");
    assert!(
        check.detail.contains("`agent-full-access`") && check.detail.ends_with("shop/dev, shop/broken"),
        "{}",
        check.detail
    );
    std::fs::write(host.path().join("config.toml"), "sandbox_mode = \"workspace-write\"\n").unwrap();
    assert_eq!(
        sandbox(host.path(), &agents[..2]).level,
        "ok",
        "the inherited host config sets one"
    );
}
