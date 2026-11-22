use super::claude;
use crate::*;
use agent_client_protocol::Channel;
use loop_manager_contract::driver::HOST_NETWORK_ENV;
use tokio::sync::mpsc;

fn sourced(value: Value) -> Value {
    json!({ "value": value, "from": { "layer": "agent" } })
}

/// An agent with a role, a hook, a skill, a subagent with a server and a hook of its own, a service and a stdio
/// server (docs/design/adapters.md, "Profiles").
fn typical(inherit: bool) -> EffectiveConfig {
    let target = json!({
        "harness": "claude", "description": sourced(json!("Checks the work.")),
        "system_prompt": sourced(json!("You check the work.")), "model": sourced(json!("haiku")),
        "effort": sourced(json!("low")), "inherit_host": null, "env": {}, "skills": {}, "addons": [],
        "mcp": { "probe": sourced(json!({ "command": ["probe-mcp"] })) },
        "hooks": { "after": sourced(json!({ "event": "PostToolUse", "matcher": "Bash", "command": "true" })) },
    });
    serde_json::from_value(json!({
        "harness": "claude", "model": null, "effort": null, "system_prompt": sourced(json!("<role prompt>")),
        "inherit_host": sourced(json!(inherit)), "env": {}, "addons": [],
        "mcp": {
            "loop-manager": sourced(json!({ "url": "http://127.0.0.1:41000/mcp",
                "headers": { "Authorization": "Bearer <attempt token>" } })),
            "mail": sourced(json!({ "command": ["/usr/local/bin/mail-mcp", "serve"], "env": { "MAIL_ROOM": "dev" } })),
        },
        "hooks": { "dcg": sourced(json!({ "event": "PreToolUse", "matcher": "Bash", "command": "dcg check",
            "timeout": "10s" })) },
        "skills": { "kit": sourced(json!({ "path": "/skills/kit" })) },
        "native": sourced(json!({ "permissions": { "allow": ["Bash(loop-manager finish *)"] } })),
        "targets": { "kit.qa": target },
    }))
    .unwrap()
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

fn driver(state: &Path) -> Acp {
    let options: toml::Table = toml::from_str("command = \"claude-agent-acp\"").unwrap();
    Acp::new("claude", &options, state, None, None).unwrap()
}

#[test]
fn an_isolated_session_sends_its_claude_options_in_meta() {
    let state = Path::new("/state");
    let split = driver(state).split(&typical(false), &plan()).unwrap();
    let files = "/state/acp/claude/r1-1";
    let host = Path::new(&std::env::var_os("HOME").unwrap()).join(".claude");
    let expected = json!({
        "cwd": "/work/rig",
        "mcpServers": [
            { "type": "http", "name": "loop-manager", "url": "http://127.0.0.1:41000/mcp",
              "headers": [{ "name": "Authorization", "value": "Bearer <attempt token>" }] },
            { "type": "stdio", "name": "mail", "command": "/usr/local/bin/mail-mcp", "args": ["serve"],
              "env": [{ "name": "MAIL_ROOM", "value": "dev" }] },
        ],
        "systemPrompt": { "mode": "append", "content": [{ "type": "text", "text": "<role prompt>" }] },
        "_meta": { "claudeCode": { "options": {
            "settingSources": ["project", "local"],
            "settings": {
                "claudeMdExcludes": [host.join("CLAUDE.md"), host.join("rules/**")],
                "permissions": { "allow": ["Bash(loop-manager finish *)"] },
                "hooks": { "PreToolUse": [{ "matcher": "Bash",
                    "hooks": [{ "type": "command", "command": "dcg check", "timeout": 10 }] }] },
            },
            "agents": { "kit.qa": {
                "description": "Checks the work.", "prompt": "You check the work.", "model": "haiku", "effort": "low",
                "mcpServers": [{ "probe": { "type": "stdio", "command": "probe-mcp", "args": [], "env": {} } }],
                "hooks": { "PostToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "true" }] }] },
            } },
            "plugins": [{ "type": "local", "path": format!("{files}/plugin") }],
        } } },
        "model": null, "effort": null, "run": "r1", "attempt": 1,
        "mode": "bypassPermissions",
        "home": format!("{files}/home"),
    });
    assert_eq!(split.session.0, expected);
    let p = &split.process.0;
    assert_eq!(
        (&p["home"], &p["files"], &p["skills"], &p["inherit_host"]),
        (
            &json!(format!("{files}/home")),
            &json!(files),
            &json!({ "kit": "/skills/kit" }),
            &json!(false)
        )
    );
}

#[test]
fn a_session_that_inherits_the_host_has_no_setting_sources_and_no_home() {
    let split = driver(Path::new("/state")).split(&typical(true), &plan()).unwrap();
    let options = &split.session.0["_meta"]["claudeCode"]["options"];
    assert!(options.get("settingSources").is_none(), "{options}");
    assert!(options["settings"].get("claudeMdExcludes").is_none(), "{options}");
    assert_eq!(split.session.0["home"], Value::Null);
    assert_eq!(split.process.0["home"], Value::Null);
    // An agent with nothing for claude beyond ACP sends no `_meta`; its mode is the native block's own.
    let mut bare = typical(true);
    (bare.hooks, bare.skills, bare.targets) = Default::default();
    bare.native = Some(serde_json::from_value(sourced(json!({ "permissions": { "defaultMode": "plan" } }))).unwrap());
    let split = driver(Path::new("/state")).split(&bare, &plan()).unwrap();
    assert!(
        split.session.0["_meta"]["claudeCode"]["options"]
            .get("settingSources")
            .is_none()
    );
    assert_eq!(split.session.0["mode"], "plan");
    bare.native = None;
    let split = driver(Path::new("/state")).split(&bare, &plan()).unwrap();
    assert!(split.session.0.get("_meta").is_none(), "{}", split.session.0);
}

fn value<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
    env.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

#[test]
fn the_process_of_an_isolated_session_gets_its_home_and_the_host_login_and_nothing_of_the_host() {
    let host = tempfile::tempdir().unwrap();
    let login = json!({ "claudeAiOauth": { "accessToken": "host-access-token" } });
    std::fs::write(host.path().join(".credentials.json"), login.to_string()).unwrap();
    let mut config = typical(false);
    config.native = Some(serde_json::from_value(sourced(json!({ "env": { "DISABLE_TELEMETRY": "0" } }))).unwrap());
    let split = driver(Path::new("/state")).split(&config, &plan()).unwrap();
    let env = claude::env_with(&split.process.0, "i1", None, host.path()).unwrap();
    let home = "/state/acp/claude/r1-1/home";
    for (key, expected) in [
        ("CLAUDE_CONFIG_DIR", Some(home)),
        ("CLAUDE_CODE_OAUTH_TOKEN", Some("host-access-token")),
        ("CLAUDE_AGENT_SDK_DISABLE_BUILTIN_AGENTS", Some("1")),
        ("CLAUDE_CODE_ENTRYPOINT", Some("loop-manager")),
        ("DISABLE_AUTOUPDATER", Some("1")),
        // The native block's `env` names it: claude gets its value from the settings.
        ("DISABLE_TELEMETRY", None),
        ("LOOP_MANAGER_RUN", Some("r1")),
        ("LOOP_MANAGER_ATTEMPT_TOKEN", Some("t0ken-of-the-attempt")),
        ("BD_ACTOR", Some("loop-manager:r1:1")),
        ("LOOP_MANAGER_BEAD", Some("shop-1")),
        (loop_manager_contract::orphans::INSTANCE_ENV, Some("i1")),
        ("CLAUDE_CODE_RETRY_WATCHDOG", None),
        ("CLAUDE_CODE_EXECUTABLE", None),
    ] {
        assert_eq!(value(&env, key), expected, "{key}");
    }
    assert_eq!(env.iter().filter(|(k, _)| k == "CLAUDE_CONFIG_DIR").count(), 1);
    for key in HOST_NETWORK_ENV {
        assert_eq!(value(&env, key), None, "{key}");
    }
    // A credential of its own: the host's login stays home.
    config.env.insert(
        "ANTHROPIC_API_KEY".into(),
        serde_json::from_value(sourced(json!("sk-literal"))).unwrap(),
    );
    let split = driver(Path::new("/state")).split(&config, &plan()).unwrap();
    let env = claude::env_with(&split.process.0, "i1", None, host.path()).unwrap();
    assert_eq!(value(&env, "CLAUDE_CODE_OAUTH_TOKEN"), None);
    assert_eq!(value(&env, "ANTHROPIC_API_KEY"), Some("sk-literal"));
}

#[test]
fn the_process_of_a_session_that_inherits_the_host_gets_the_host_settings() {
    let host = tempfile::tempdir().unwrap();
    let login = json!({ "claudeAiOauth": { "accessToken": "host-access-token" } });
    std::fs::write(host.path().join(".credentials.json"), login.to_string()).unwrap();
    let split = driver(Path::new("/state")).split(&typical(true), &plan()).unwrap();
    let env = claude::env_with(&split.process.0, "i1", None, host.path()).unwrap();
    for key in HOST_NETWORK_ENV.iter().chain(&["CLAUDE_CONFIG_DIR"]) {
        assert_eq!(value(&env, key), std::env::var(key).ok().as_deref(), "{key}");
    }
    assert_eq!(value(&env, "CLAUDE_CODE_OAUTH_TOKEN"), None);
    assert_eq!(value(&env, "CLAUDE_AGENT_SDK_DISABLE_BUILTIN_AGENTS"), Some("1"));
    assert_eq!(value(&env, "DISABLE_TELEMETRY"), Some("1"));
}

#[test]
fn the_native_env_and_the_subagents_server_secrets_are_secrets_to_the_trace() {
    let mut config = typical(false);
    config.native = Some(serde_json::from_value(sourced(json!({ "env": { "VENDOR_KEY": "vendor-secret" } }))).unwrap());
    let target = config.targets.get_mut("kit.qa").unwrap();
    let server = json!({ "url": "http://127.0.0.1:1/mcp", "headers": { "Authorization": "Bearer service-token" } });
    target
        .mcp
        .insert("svc".into(), serde_json::from_value(sourced(server)).unwrap());
    let split = driver(Path::new("/state")).split(&config, &plan()).unwrap();
    let mut secrets = claude::secrets(&split.session.0);
    secrets.sort();
    assert_eq!(secrets, ["Bearer service-token", "vendor-secret"]);
    // A harness of no profile has none beyond its MCP servers.
    assert!((super::GENERIC.secrets)(&split.session.0).is_empty());
}

#[test]
fn a_resume_takes_the_home_of_the_session() {
    let resume = SessionPlan::Resume {
        session: SessionRef(json!({ "session_id": "s1", "cwd": "/work/rig", "home": "/state/acp/claude/r1-1/home" })),
        workdir: PathBuf::from("/work/rig"),
        run: "r1".into(),
        attempt: 2,
        token: "t".into(),
        actor: None,
        bead: None,
    };
    let split = driver(Path::new("/state")).split(&typical(false), &resume).unwrap();
    let home = "/state/acp/claude/r1-1/home";
    assert_eq!(split.process.0["home"], home);
    assert_eq!(split.session.0["home"], home);
    // The plugin is the new attempt's.
    let plugins = &split.session.0["_meta"]["claudeCode"]["options"]["plugins"];
    assert_eq!(plugins[0]["path"], "/state/acp/claude/r1-2/plugin");
    let env = claude::env_with(&split.process.0, "i1", None, Path::new("/nonexistent")).unwrap();
    assert_eq!(value(&env, "CLAUDE_CONFIG_DIR"), Some(home));
}

/// The accounts of the gates are those the native claude driver had: the same hashes of the same sources.
#[test]
fn the_account_is_the_native_drivers() {
    let acp = driver(Path::new("/state"));
    let mut config = typical(false);
    let account = |config: &EffectiveConfig| acp.account(&acp.split(config, &plan()).unwrap().process).unwrap();
    // The host's login is the hash of the host's claude directory, which this machine decides.
    let source = ProcessPart(json!({ "host": claude::host_dir() }));
    let host = loop_manager_contract::driver::Fingerprint::of("claude", &source).0[..16].to_owned();
    assert_eq!(account(&config), host);
    assert_eq!(
        account(&typical(true)),
        host,
        "inherited or delivered, the host's login"
    );
    config.env.insert(
        "ANTHROPIC_API_KEY".into(),
        serde_json::from_value(sourced(json!("${SHOP_KEY}"))).unwrap(),
    );
    assert_eq!(account(&config), "db8f617dfdc23603");
    config.env.clear();
    config.env.insert(
        "CLAUDE_CODE_OAUTH_TOKEN".into(),
        serde_json::from_value(sourced(json!("${TOKEN}"))).unwrap(),
    );
    assert_eq!(account(&config), "978f64bdc883b6e8");
}

/// The errors of the adapter by code, message and data, and the kind each stands for.
fn errors() -> Vec<(i32, &'static str, Value, FailureKind)> {
    use FailureKind::*;
    let named = |error: &str, kind| (-32603, "x", json!({ "errorKind": error }), kind);
    let startup = |reason: &str, kind| (-32603, "x", json!({ "startupFailureReason": reason }), kind);
    let limit = |limit: Value, kind| {
        (
            -32603,
            "x",
            json!({ "errorKind": "rate_limit", "rateLimit": limit }),
            kind,
        )
    };
    let mut rows = vec![
        (-32000, "Please run /login", json!(null), AuthRequired),
        (-32602, "Invalid params", json!(null), ConfigError),
        named("authentication_failed", AuthRequired),
        named("oauth_org_not_allowed", AuthRequired),
        named("cloud_credential_error", AuthRequired),
        named("verification_required", AuthRequired),
        named("billing_error", Billing),
        named("account_on_hold", Billing),
        limit(
            json!({ "status": "rejected", "errorCode": "credits_required" }),
            Billing,
        ),
        limit(json!({ "status": "allowed_warning" }), RateLimited),
        named("overloaded", Overloaded),
        named("server_error", Overloaded),
        named("model_not_found", ConfigError),
        named("unknown", Unknown),
        named("max_output_tokens", Unknown),
        startup("session_held_by_background", TranscriptDivergence),
        startup("cwd_unavailable", ConfigError),
        startup("gateway_signin_required", AuthRequired),
        (-32603, "x", json!({ "cause": "exited" }), HarnessCrash),
        (-32603, "plain", json!(null), Unknown),
    ];
    let too_long = json!({ "errorKind": "invalid_request" });
    rows.extend([
        (
            -32603,
            "Prompt is too long: 210000 tokens",
            too_long.clone(),
            ContextOverflow,
        ),
        (-32603, "Bad request", too_long, Unknown),
        (
            -32603,
            "Claude Code stopped. Resume the session to continue.",
            json!(null),
            HarnessCrash,
        ),
    ]);
    rows
}

#[test]
fn the_failures_of_the_adapter_by_their_fields() {
    for (code, message, data, kind) in errors() {
        let failure = claude::classify(code, message, (!data.is_null()).then_some(&data));
        assert_eq!(failure.kind, kind, "{code} {message} {data}");
        assert_eq!(failure.scope, kind.default_scope(), "{data}");
        let retried = matches!(kind, FailureKind::RateLimited | FailureKind::Overloaded);
        assert_eq!(failure.retried_by_harness, retried, "{data}");
        let text = (kind == FailureKind::Unknown).then_some(message);
        assert_eq!(failure.text.as_deref(), text, "{data}");
        assert!(failure.raw.starts_with(&code.to_string()), "{}", failure.raw);
    }
    // A quota: rejected, or worded so; it is back at `resetsAt`, in seconds.
    let limit = json!({ "errorKind": "rate_limit", "rateLimit": { "status": "rejected", "resetsAt": 1_800_000_000 } });
    let failure = claude::classify(-32603, "x", Some(&limit));
    assert_eq!(
        (failure.kind, failure.retry_at_ms),
        (FailureKind::QuotaExhausted, Some(1_800_000_000_000))
    );
    assert!(!failure.retried_by_harness);
    let worded = json!({ "errorKind": "rate_limit" });
    let failure = claude::classify(-32603, "You've hit your limit · resets 5pm", Some(&worded));
    assert_eq!((failure.kind, failure.retry_at_ms), (FailureKind::QuotaExhausted, None));
}

#[test]
fn a_notice_of_an_mcp_server_that_did_not_start_is_a_tool_failure() {
    let notice = |title: &str| -> schema::Notice {
        serde_json::from_value(json!({ "severity": "warning", "title": title, "description": "Run /mcp." })).unwrap()
    };
    for title in [
        "MCP server \"mail\" failed to start",
        "MCP server \"mail\" needs authentication",
    ] {
        let failure = claude::notice(&notice(title)).unwrap();
        assert_eq!(failure.kind, FailureKind::ToolFailure);
        assert_eq!(failure.raw, format!("{title}: Run /mcp."));
    }
    assert!(claude::notice(&notice("Claude reported a warning")).is_none());
    assert!(claude::notice(&notice("MCP server \"mail\" is slow")).is_none());
    // The session of the claude profile tells it as it comes, and an error turn by the adapter's fields.
    let mut turn = Turn::new(Profile::of("claude"));
    turn.session = Some("s1".into());
    let update = |v: Value| -> schema::SessionUpdate { serde_json::from_value(v).unwrap() };
    let events = turn.update(
        "s1",
        &update(
            json!({ "sessionUpdate": "notice", "severity": "warning", "title": "MCP server \"mail\" failed to start" }),
        ),
        Instant::now(),
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Normalized::Failure(f) if f.kind == FailureKind::ToolFailure))
    );
    let _ = turn.update(
        "s1",
        &update(json!({ "sessionUpdate": "state_update", "state": "running" })),
        Instant::now(),
    );
    let idle = json!({ "sessionUpdate": "state_update", "state": "idle", "stopReason": "error",
        "error": { "code": -32603, "message": "Overloaded", "data": { "errorKind": "overloaded" } } });
    let events = turn.update("s1", &update(idle.clone()), Instant::now());
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Normalized::Failure(f) if f.kind == FailureKind::Overloaded))
    );
    // A harness of no profile knows neither.
    let mut generic = Turn::default();
    generic.session = Some("s1".into());
    let _ = generic.update(
        "s1",
        &update(json!({ "sessionUpdate": "state_update", "state": "running" })),
        Instant::now(),
    );
    let events = generic.update("s1", &update(idle), Instant::now());
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Normalized::Failure(f) if f.kind == FailureKind::Unknown))
    );
}

/// The config options the agent offers, by category.
fn offered() -> Vec<schema::SessionConfigOption> {
    let option = |id: &str, category: &str, values: &[&str]| -> schema::SessionConfigOption {
        serde_json::from_value(json!({ "configId": id, "name": id, "category": category, "type": "select",
            "currentValue": values[0], "options": values.iter().map(|v| json!({ "value": v, "name": v })).collect::<Vec<_>>() }))
        .unwrap()
    };
    vec![
        option("mode", "mode", &["default", "bypassPermissions", "plan"]),
        option("effort", "thought_level", &["default", "high"]),
        option("model", "model", &["default", "opus"]),
    ]
}

/// The claude driver with the instance `i1` connected to an agent in the test's process: it offers [`offered`] in
/// its answers to `session/new` and `session/resume` (none for session `gone`), tells each option set to the
/// receiver, and refuses a value it does not offer.
async fn connected() -> (Acp, InstanceHandle, mpsc::UnboundedReceiver<(String, String)>) {
    let driver = driver(&std::env::temp_dir());
    let (ours, theirs) = Channel::duplex();
    let (sets, received) = mpsc::unbounded_channel();
    tokio::spawn(
        Agent
            .v2()
            .on_receive_request(
                async |_: schema::InitializeRequest, responder: Responder<schema::InitializeResponse>, _| {
                    let me = schema::Implementation::new("agent", "1");
                    let session = schema::AgentCapabilities::new().session(schema::SessionCapabilities::new());
                    responder.respond(schema::InitializeResponse::new(ProtocolVersion::V2, me).capabilities(session))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async |_: schema::NewSessionRequest, responder: Responder<schema::NewSessionResponse>, _| {
                    responder.respond(schema::NewSessionResponse::new("s1").config_options(offered()))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async |request: schema::ResumeSessionRequest,
                       responder: Responder<schema::ResumeSessionResponse>,
                       _| {
                    match request.session_id.to_string().as_str() {
                        "gone" => {
                            responder.respond_with_error(schema::Error::session_not_found(request.session_id).into())
                        }
                        _ => responder.respond(schema::ResumeSessionResponse::new().config_options(offered())),
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: schema::SetSessionConfigOptionRequest,
                            responder: Responder<schema::SetSessionConfigOptionResponse>,
                            _| {
                    let value = serde_json::to_value(&request.value).unwrap();
                    let value = value["value"]
                        .as_str()
                        .or(value.as_str())
                        .unwrap_or_default()
                        .to_owned();
                    let id = request.config_id.to_string();
                    let _ = sets.send((id.clone(), value.clone()));
                    let options = serde_json::to_value(offered()).unwrap();
                    let known = (options.as_array().unwrap().iter()).any(|o| {
                        o["configId"] == id && o["options"].as_array().unwrap().iter().any(|v| v["value"] == value)
                    });
                    match known {
                        true => responder.respond(schema::SetSessionConfigOptionResponse::new(offered())),
                        false => responder.respond_with_error(schema::Error::invalid_params().into()),
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_to(theirs),
    );
    let (sink, _events) = tokio::sync::mpsc::unbounded_channel();
    let shared = Arc::new(Mutex::new(Shared {
        events: child::Events::new("i1", sink),
        turn: Turn::new(driver.profile),
        session: None,
        record: None,
        started: true,
    }));
    let (conn_tx, conn_rx) = oneshot::channel();
    let (close_tx, close_rx) = oneshot::channel();
    tokio::spawn(connect(shared.clone(), ours, conn_tx, close_rx));
    let conn = conn_rx.await.unwrap();
    initialize(&conn, "claude", Path::new("unused")).await.unwrap();
    let instance = Instance {
        conn,
        shared,
        close: Mutex::new(Some(close_tx)),
        capabilities: Value::Null,
        agent: Value::Null,
        trace: None,
    };
    lock(&driver.instances).insert("i1".into(), Arc::new(instance));
    let handle = InstanceHandle {
        id: "i1".into(),
        pid: 0,
        start_time: 0,
        pgid: 0,
    };
    (driver, handle, received)
}

fn part(mode: &str) -> Value {
    json!({ "cwd": "/work/rig", "mcpServers": [], "model": "opus", "effort": "high", "mode": mode,
        "home": "/state/acp/claude/r1-1/home" })
}

fn drained(received: &mut mpsc::UnboundedReceiver<(String, String)>) -> Vec<(String, String)> {
    std::iter::from_fn(|| received.try_recv().ok()).collect()
}

#[tokio::test]
async fn the_model_then_the_effort_then_the_mode_after_a_new_session_and_after_a_resume() {
    let (driver, handle, mut received) = connected().await;
    let session = driver.open(&handle, &part("bypassPermissions")).await.unwrap();
    assert_eq!(
        session.0,
        json!({ "session_id": "s1", "cwd": "/work/rig", "home": "/state/acp/claude/r1-1/home" })
    );
    let order = [("model", "opus"), ("effort", "high"), ("mode", "bypassPermissions")]
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .to_vec();
    assert_eq!(drained(&mut received), order);
    driver
        .resume(&handle, &session, &part("bypassPermissions"))
        .await
        .unwrap();
    assert_eq!(drained(&mut received), order);
    // A session the agent no longer has: no transcript to go on with.
    let gone = SessionRef(json!({ "session_id": "gone", "cwd": "/work/rig" }));
    let failure = driver.resume(&handle, &gone, &part("plan")).await.unwrap_err();
    assert_eq!(failure.kind, FailureKind::TranscriptDivergence);
}

#[tokio::test]
async fn a_mode_the_agent_refuses_is_a_config_error_naming_the_key() {
    let (driver, handle, _received) = connected().await;
    let failure = driver.open(&handle, &part("dontAsk")).await.unwrap_err();
    assert_eq!(failure.kind, FailureKind::ConfigError);
    assert!(failure.raw.starts_with("`mode` = `dontAsk`: -32602"), "{}", failure.raw);
}

#[tokio::test]
async fn the_plugin_is_there_before_the_spawn_and_gone_after_the_exit() {
    let state = tempfile::tempdir().unwrap();
    let rig = tempfile::tempdir().unwrap();
    let seen = rig.path().join("seen");
    let plugin = state.path().join("acp/claude/r1-1/plugin");
    // The agent looks for its skill in the plugin and exits without answering `initialize`.
    let script = format!(
        "test -L '{}/skills/kit' && test -f '{}/.claude-plugin/plugin.json' && touch '{}'",
        plugin.display(),
        plugin.display(),
        seen.display()
    );
    let mut options = toml::Table::new();
    options.insert("command".into(), "/bin/sh".into());
    options.insert("args".into(), toml::Value::Array(vec!["-c".into(), script.into()]));
    let driver = Acp::new("claude", &options, state.path(), None, None).unwrap();
    let plan = SessionPlan::New {
        workdir: rig.path().to_owned(),
        run: "r1".into(),
        attempt: 1,
        token: "t".into(),
        actor: None,
        bead: None,
    };
    let split = driver.split(&typical(false), &plan).unwrap();
    let (sink, _events) = tokio::sync::mpsc::unbounded_channel();
    assert!(driver.start("i1", &split.process.0, sink).await.is_err());
    let deadline = Instant::now() + Duration::from_secs(10);
    while plugin.exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(seen.exists(), "the agent did not find its plugin");
    assert!(!plugin.exists(), "the plugin outlived the process");
    let home = state.path().join("acp/claude/r1-1/home");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&home).unwrap().permissions().mode() & 0o777, 0o700);
}
