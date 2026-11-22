use super::*;
use agent_client_protocol::v2::schema::NewSessionRequest;
use loop_manager_contract::config::{Layer, Mcp};
use std::path::PathBuf;

fn agent<T>(value: T) -> Sourced<T> {
    Sourced {
        value,
        from: Layer::Agent,
    }
}

fn config() -> EffectiveConfig {
    EffectiveConfig {
        services: Default::default(),
        harness: "fake".into(),
        harness_from: Layer::Agent,
        scope: Default::default(),
        description: None,
        subagents: None,
        targets: Default::default(),
        model_given: None,
        model: None,
        effort: None,
        system_prompt: None,
        inherit_host: None,
        env: Default::default(),
        mcp: Default::default(),
        hooks: Default::default(),
        skills: Default::default(),
        addons: Vec::new(),
        native: None,
    }
}

fn plan() -> SessionPlan {
    SessionPlan::New {
        workdir: PathBuf::from("/work/shop"),
        run: "run_1".into(),
        attempt: 2,
        token: "t".into(),
        actor: Some(("BD_ACTOR".into(), "loop-manager:run_1:2".into())),
        bead: Some("shop-1".into()),
    }
}

#[test]
fn the_agent_env_goes_over_the_harness_env_unresolved() {
    let mut config = config();
    config.env.insert("KEY".into(), agent("${AGENT_KEY}".into()));
    let harness = Map::from_iter([("KEY".into(), json!("h")), ("MODE".into(), json!("fast"))]);
    let p = process(
        "fake",
        (Path::new("/bin/agent"), &["acp".into()], &harness),
        &config,
        &plan(),
    )
    .unwrap();
    assert_eq!(p["env"], json!({ "KEY": "${AGENT_KEY}", "MODE": "fast" }));
    assert_eq!(
        (p["command"].as_str(), &p["args"]),
        (Some("/bin/agent"), &json!(["acp"]))
    );
    assert_eq!(p["workdir"], "/work/shop");
}

#[test]
fn a_setting_the_harness_has_no_mechanism_for_is_refused_by_its_key() {
    let mut config = config();
    config.native = Some(agent(json!({ "mode": "x" })));
    let error = process("fake", (Path::new("a"), &[], &Map::new()), &config, &plan()).unwrap_err();
    assert_eq!(error.kind, loop_manager_contract::driver::FailureKind::ConfigError);
    assert!(
        error.raw.starts_with("`native`: the ACP harness `fake`"),
        "{}",
        error.raw
    );
    config.native = None;
    config.inherit_host = Some(agent(false));
    let error = process("fake", (Path::new("a"), &[], &Map::new()), &config, &plan()).unwrap_err();
    assert!(error.raw.starts_with("`inherit_host`"), "{}", error.raw);
}

#[test]
fn the_session_part_is_the_session_new_request_with_servers_verbatim() {
    let mut config = config();
    let stdio: Mcp =
        serde_json::from_value(json!({ "command": ["/bin/probe", "--stdio"], "env": { "A": "1" } })).unwrap();
    let http: Mcp = serde_json::from_value(json!({ "url": "http://127.0.0.1:${LOOP_MANAGER_SERVICE_PORT:mail}/mcp",
        "headers": { "Authorization": "Bearer ${LOOP_MANAGER_SERVICE_TOKEN:mail}" } }))
    .unwrap();
    config.mcp.insert("probe".into(), agent(stdio));
    config.mcp.insert("mail".into(), agent(http));
    config.system_prompt = Some(agent("be brief".into()));
    let part = session(&config, &plan()).unwrap();
    let request: NewSessionRequest = serde_json::from_value(part.clone()).unwrap();
    assert_eq!(request.mcp_servers.len(), 2);
    assert_eq!(
        part["mcpServers"][0]["url"],
        "http://127.0.0.1:${LOOP_MANAGER_SERVICE_PORT:mail}/mcp"
    );
    assert_eq!(part["mcpServers"][1]["env"], json!([{ "name": "A", "value": "1" }]));
    assert_eq!(part["systemPrompt"]["content"][0]["text"], "be brief");
}

#[test]
fn an_mcp_server_of_a_transport_the_agent_did_not_advertise_is_refused_by_name() {
    let part = json!({ "mcpServers": [
        { "type": "stdio", "name": "probe", "command": "/bin/probe", "args": [], "env": [] },
        { "type": "http", "name": "mail", "url": "http://127.0.0.1:1/mcp", "headers": [] },
    ] });
    let both = json!({ "session": { "mcp": { "stdio": {}, "http": {} } } });
    assert!(offered("fake", &part, &both).is_ok());
    let error = offered("fake", &part, &json!({ "session": { "mcp": { "stdio": {} } } })).unwrap_err();
    assert_eq!(error.kind, loop_manager_contract::driver::FailureKind::ConfigError);
    assert_eq!(
        error.raw,
        "MCP server `mail`: the agent of harness `fake` advertises no `http` MCP transport"
    );
    let error = offered("fake", &part, &json!({ "session": {} })).unwrap_err();
    assert!(error.raw.starts_with("MCP server `probe`"), "{}", error.raw);
    assert!(offered("fake", &json!({ "mcpServers": [] }), &json!({ "session": {} })).is_ok());
}

#[test]
fn the_account_follows_the_names_and_references_of_the_env_not_its_literals() {
    let of = |env: Value| account("fake", &json!({ "command": "/bin/agent", "env": env }));
    let base = of(json!({ "KEY": "${SHOP_KEY}", "FLAG": "1" }));
    assert_eq!(base, of(json!({ "KEY": "${SHOP_KEY}", "FLAG": "2" })));
    assert_ne!(base, of(json!({ "KEY": "${OTHER_KEY}", "FLAG": "1" })));
    assert_ne!(base, of(json!({ "KEY": "${SHOP_KEY}" })));
}
