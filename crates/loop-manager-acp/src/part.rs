//! The process and session parts of an ACP v2 session (docs/design/adapters.md, "ACP v2 harnesses").

use loop_manager_contract::{
    config::{EffectiveConfig, Sourced},
    driver::{Failure, Fingerprint, ProcessPart, SessionPlan, child::config_error},
};
use serde_json::{Map, Value, json};
use std::path::Path;

/// The process part: `{command, args, env, workdir, run, attempt, token, actor, bead}`; `env` is the harness's
/// `env` with the agent's over it, references unresolved (they resolve at the spawn).
pub(crate) fn process(
    harness: &str,
    setup: (&Path, &[String], &Map<String, Value>),
    config: &EffectiveConfig,
    plan: &SessionPlan,
) -> Result<Value, Failure> {
    unsupported(harness, config).map_err(config_error)?;
    let (command, args, env) = setup;
    let mut env = env.clone();
    env.extend(config.env.iter().map(|(k, v)| (k.clone(), json!(v.value))));
    let (SessionPlan::New {
        workdir,
        run,
        attempt,
        token,
        actor,
        bead,
    }
    | SessionPlan::Resume {
        workdir,
        run,
        attempt,
        token,
        actor,
        bead,
        ..
    }) = plan;
    Ok(json!({
        "command": command, "args": args, "env": env, "workdir": workdir,
        "run": run, "attempt": attempt, "token": token, "actor": actor, "bead": bead,
    }))
}

/// What a harness known only by its config has no mechanism for: refused by key, so the agent's config says what to
/// drop (docs/design/config.md).
pub(crate) fn unsupported(harness: &str, config: &EffectiveConfig) -> Result<(), String> {
    let refused = [
        ("hooks", !config.hooks.is_empty()),
        ("skills", config.skills.values().any(|s| s.value.enabled)),
        ("subagents", !config.targets.is_empty()),
        ("native", config.native.is_some()),
        ("inherit_host", !config.inherits_host()),
    ];
    match refused.into_iter().find(|(_, set)| *set) {
        Some((key, _)) => Err(format!("`{key}`: the ACP harness `{harness}` has no mechanism for it")),
        None => Ok(()),
    }
}

/// The session part, in the shape of the ACP `session/new` parameters: `{cwd, mcpServers, systemPrompt}`, and the
/// `model` and `effort` the session's config options take.
pub(crate) fn session(config: &EffectiveConfig, plan: &SessionPlan) -> Result<Value, Failure> {
    let (SessionPlan::New { workdir, .. } | SessionPlan::Resume { workdir, .. }) = plan;
    let mut servers = Vec::new();
    for (name, server) in &config.mcp {
        let server = &server.value;
        // A service is a URL by now (the core binds it), its placeholders kept verbatim (contracts 4).
        let entry = match (&server.command, &server.url) {
            (Some(command), None) if !command.is_empty() => {
                let env = (server.env.iter()).map(|(k, v)| json!({ "name": k, "value": v }));
                json!({ "type": "stdio", "name": name, "command": command[0], "args": command[1..],
                    "env": env.collect::<Vec<_>>() })
            }
            (None, Some(url)) => {
                let headers = (server.headers.iter()).map(|(k, v)| json!({ "name": k, "value": v }));
                json!({ "type": "http", "name": name, "url": url, "headers": headers.collect::<Vec<_>>() })
            }
            _ => return Err(config_error(format!("MCP server `{name}` needs a command or a url"))),
        };
        servers.push(entry);
    }
    let text = |s: &Option<Sourced<String>>| s.as_ref().map(|s| s.value.clone());
    Ok(json!({
        "cwd": workdir,
        "mcpServers": servers,
        // The role goes after the agent's own instructions, as it would in the harness's own CLI.
        "systemPrompt": text(&config.system_prompt)
            .map(|t| json!({ "mode": "append", "content": [{ "type": "text", "text": t }] })),
        "model": text(&config.model),
        "effort": text(&config.effort),
    }))
}

/// What the session part `part` asks of the agent that its `initialize` answer (`capabilities`) did not advertise,
/// refused before `session/new`: a system prompt without `session.systemPrompt`, an MCP server of a transport not in
/// `session.mcp`.
pub(crate) fn offered(harness: &str, part: &Value, capabilities: &Value) -> Result<(), Failure> {
    if !part["systemPrompt"].is_null() && capabilities["session"]["systemPrompt"].is_null() {
        return Err(config_error(format!(
            "`system_prompt`: the agent of harness `{harness}` advertises no `session.systemPrompt`"
        )));
    }
    let servers = part["mcpServers"].as_array().into_iter().flatten();
    for server in servers {
        let transport = server["type"].as_str().unwrap_or_default();
        if capabilities["session"]["mcp"][transport].is_null() {
            return Err(config_error(format!(
                "MCP server `{}`: the agent of harness `{harness}` advertises no `{transport}` MCP transport",
                server["name"].as_str().unwrap_or_default()
            )));
        }
    }
    Ok(())
}

/// The process environment on the base of `child::base_env`: the daemon's `PATH`, `HOME`, `LANG` and
/// `HOST_NETWORK_ENV`, then `env`, the instance id and the socket.
pub(crate) fn base(env: &Value, id: &str, socket: Option<&Path>) -> Result<Vec<(String, String)>, Failure> {
    let vars = ["PATH", "HOME", "LANG"]
        .into_iter()
        .chain(loop_manager_contract::driver::HOST_NETWORK_ENV);
    loop_manager_contract::driver::child::base_env(vars, env, id, socket)
}

/// The attempt environment (contracts 4.6) on the [`base`].
pub(crate) fn env(p: &Value, id: &str, socket: Option<&Path>) -> Result<Vec<(String, String)>, Failure> {
    let mut env = base(&p["env"], id, socket)?;
    let text = |v: &Value| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string());
    env.extend([
        ("LOOP_MANAGER_RUN".to_owned(), text(&p["run"])),
        ("LOOP_MANAGER_ATTEMPT_TOKEN".to_owned(), text(&p["token"])),
    ]);
    // The tracker actor of the attempt, as the core planned it (contracts 5.1).
    env.extend(serde_json::from_value::<(String, String)>(p["actor"].clone()).ok());
    env.extend(
        p["bead"]
            .as_str()
            .map(|b| ("LOOP_MANAGER_BEAD".to_owned(), b.to_owned())),
    );
    Ok(env)
}

/// The account the gates use: a hash of the command, the names of the environment and the `${NAME}` references among
/// its values, so it says where a key comes from, never what it is, and a literal value (a flag, a path) leaves it
/// alone (docs/design/contracts.md 7).
pub(crate) fn account(harness: &str, p: &Value) -> String {
    let env = p["env"].as_object().into_iter().flatten().map(|(k, v)| {
        let reference = v.as_str().filter(|v| v.starts_with("${"));
        (k.clone(), json!(reference))
    });
    let source = json!({ "command": p["command"], "env": env.collect::<Map<_, _>>() });
    Fingerprint::of(harness, &ProcessPart(source)).0[..16].to_owned()
}

#[cfg(test)]
mod tests;
