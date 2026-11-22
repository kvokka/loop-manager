//! The profile of `claude-agent-acp`, the ACP adapter of claude (docs/design/adapters.md, "Profiles"): what claude
//! takes beyond ACP goes in `_meta.claudeCode.options` of `session/new` and `session/resume` and in the environment of
//! the adapter's process, which the claude it starts inherits.

use super::{Env, Setup};
use crate::{part, session};
use agent_client_protocol::v2::schema::Notice;
use loop_manager_contract::{
    config::{EffectiveConfig, Hook, Sourced},
    driver::{
        Check, DoctorAgents, Failure, FailureKind, Fingerprint, ProcessPart, SessionPlan, Source, child::config_error,
        host_mcp_check, login_check,
    },
};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// The process part: the generic one with `files` (the directory of the attempt's generated files), `home` (the
/// claude config directory of an isolated session, else null), `skills` (the skill directories by name, the
/// targets' among them), `inherit_host`, and `defaults` (the variables loop-manager sets that neither `env` nor the
/// native block's `env` names).
#[expect(
    clippy::unnecessary_wraps,
    reason = "the signature of `Profile::process`, where the generic profile refuses"
)]
pub(super) fn process(
    _harness: &str,
    setup: Setup,
    config: &EffectiveConfig,
    plan: &SessionPlan,
    state: &Path,
) -> Result<Value, Failure> {
    let mut p = part::fields(setup, config, plan);
    let files = files(state, plan);
    let native = config.native.as_ref().map_or(&Value::Null, |n| &n.value["env"]);
    let defaults: Map<String, Value> = (DEFAULTS.iter())
        .filter(|(k, _)| p["env"].get(k).is_none() && native.get(k).is_none())
        .map(|(k, v)| ((*k).to_owned(), json!(v)))
        .collect();
    p["home"] = json!(home(config, plan, &files));
    p["files"] = json!(files);
    p["skills"] = json!(skills(config));
    p["inherit_host"] = json!(config.inherits_host());
    p["defaults"] = Value::Object(defaults);
    Ok(p)
}

/// What the session part adds to `session/new` and `session/resume`: `_meta.claudeCode.options` with the setting
/// sources of an isolated session, the settings (the native block and the hooks), the subagent definitions and the
/// plugin of the skills, each key only when it has something; and, outside ACP, the permission `mode` and the `home`
/// of the session.
pub(super) fn session(
    config: &EffectiveConfig,
    plan: &SessionPlan,
    state: &Path,
    part: &mut Value,
) -> Result<(), Failure> {
    let (settings, mode) = settings(config)?;
    let files = files(state, plan);
    let mut options = Map::new();
    // Host inheritance off keeps the rig's own settings, `CLAUDE.md` and `.mcp.json` (docs/design/config.md).
    if !config.inherits_host() {
        options.insert("settingSources".into(), json!(["project", "local"]));
    }
    if !settings.is_null() {
        options.insert("settings".into(), settings);
    }
    let agents = agents(config)?;
    if !agents.is_null() {
        options.insert("agents".into(), agents);
    }
    if !skills(config).is_empty() {
        options.insert(
            "plugins".into(),
            json!([{ "type": "local", "path": files.join("plugin") }]),
        );
    }
    if !options.is_empty() {
        part["_meta"] = json!({ "claudeCode": { "options": options } });
    }
    part["mode"] = json!(mode);
    part["home"] = json!(home(config, plan, &files));
    Ok(())
}

/// The directory of the generated files of the attempt: `<state>/acp/claude/<run>-<attempt>`.
fn files(state: &Path, plan: &SessionPlan) -> PathBuf {
    let (SessionPlan::New { run, attempt, .. } | SessionPlan::Resume { run, attempt, .. }) = plan;
    state.join("acp").join("claude").join(format!("{run}-{attempt}"))
}

/// The claude config directory of an isolated session: the one of the session resumed, else `home` under the
/// attempt's `files`; none for a session that inherits the host.
fn home(config: &EffectiveConfig, plan: &SessionPlan, files: &Path) -> Option<PathBuf> {
    if config.inherits_host() {
        return None;
    }
    let resumed = match plan {
        SessionPlan::Resume { session, .. } => session.0["home"].as_str().map(PathBuf::from),
        SessionPlan::New { .. } => None,
    };
    Some(resumed.unwrap_or_else(|| files.join("home")))
}

/// The enabled skills of the session and of its targets, by name, for the one plugin: the session's first, the same
/// name is the same directory (a load error otherwise).
fn skills(config: &EffectiveConfig) -> BTreeMap<String, PathBuf> {
    let mut skills = BTreeMap::new();
    for c in std::iter::once(config).chain(config.targets.values()) {
        for (name, s) in c.skills.iter().filter(|(_, s)| s.value.enabled) {
            if let Some(path) = &s.value.path {
                skills.entry(name.clone()).or_insert_with(|| path.clone());
            }
        }
    }
    skills
}

fn text(s: &Option<Sourced<String>>) -> Option<String> {
    s.as_ref().map(|s| s.value.clone())
}

/// The permission mode of a session whose config names none: the harness decides every permission and nothing waits
/// for a person (decision 117).
const DEFAULT_PERMISSION_MODE: &str = "bypassPermissions";

/// The settings: the native block (claude's own settings keys) with the enabled hooks added to its own, null when
/// empty; and the permission mode, which is a session config option and not a settings key: the block's
/// `permission_mode`, else its `permissions.defaultMode`, else the default. The adapter decides the mode a session
/// starts in from its own files, so the mode is always set.
fn settings(config: &EffectiveConfig) -> Result<(Value, String), Failure> {
    let mut settings = match config.native.as_ref().map(|n| &n.value) {
        None => json!({}),
        Some(native @ Value::Object(_)) => native.clone(),
        Some(_) => {
            return Err(config_error(
                "the claude native block is a table of claude settings".into(),
            ));
        }
    };
    let mode = match settings.as_object_mut().and_then(|s| s.remove("permission_mode")) {
        None => (settings["permissions"]["defaultMode"].as_str())
            .unwrap_or(DEFAULT_PERMISSION_MODE)
            .to_owned(),
        Some(Value::String(mode)) => mode,
        Some(_) => return Err(config_error("the claude native `permission_mode` is a string".into())),
    };
    // Without the host layer the own config directory and the setting sources drop the user memory, but claude also
    // reads `CLAUDE.md` up the rig's ancestors, and a rig under `HOME` reaches the host's (criterion 12).
    if let Some(home) = std::env::var_os("HOME").filter(|_| !config.inherits_host()) {
        let host = Path::new(&home).join(".claude");
        let excluded = [host.join("CLAUDE.md"), host.join("rules/**")];
        settings["claudeMdExcludes"] = json!(excluded);
    }
    add_hooks(&mut settings, config.hooks.iter().map(|(name, h)| (name, &h.value)))?;
    let settings = if settings == json!({}) { Value::Null } else { settings };
    Ok((settings, mode))
}

/// The MCP servers of `config` in the shape of claude's own config, by name.
fn servers(config: &EffectiveConfig) -> Result<Map<String, Value>, Failure> {
    let mut servers = Map::new();
    for (name, server) in &config.mcp {
        let server = &server.value;
        // A service is a URL by now (the core binds it); a stdio server runs per session whatever its scope.
        let entry = match (&server.command, &server.url) {
            (Some(command), None) if !command.is_empty() => {
                json!({ "type": "stdio", "command": command[0], "args": command[1..], "env": server.env })
            }
            (None, Some(url)) => json!({ "type": "http", "url": url, "headers": server.headers }),
            _ => return Err(config_error(format!("MCP server `{name}` needs a command or a url"))),
        };
        servers.insert(name.clone(), entry);
    }
    Ok(servers)
}

/// The subagent definitions: one per target, by its name. A target's server of a name the session has, and a hook
/// equal to one of the session's, are the session's: the subagent sees the server and runs the hook already.
fn agents(config: &EffectiveConfig) -> Result<Value, Failure> {
    let mut agents = Map::new();
    for (name, t) in &config.targets {
        let mut agent =
            json!({ "description": text(&t.description), "prompt": text(&t.system_prompt).unwrap_or_default() });
        for (key, value) in [("model", &t.model), ("effort", &t.effort)] {
            if let Some(value) = text(value) {
                agent[key] = json!(value);
            }
        }
        // claude takes the servers of a definition as a list of one-server maps, not as a map.
        let servers: Vec<_> = servers(t)?
            .into_iter()
            .filter(|(k, _)| !config.mcp.contains_key(k))
            .map(|(k, v)| json!({ k: v }))
            .collect();
        if !servers.is_empty() {
            agent["mcpServers"] = json!(servers);
        }
        let own = |h: &Hook| !config.hooks.values().any(|s| s.value == *h);
        add_hooks(
            &mut agent,
            t.hooks.iter().map(|(k, h)| (k, &h.value)).filter(|(_, h)| own(h)),
        )?;
        agents.insert(name.clone(), agent);
    }
    Ok(if agents.is_empty() {
        Value::Null
    } else {
        Value::Object(agents)
    })
}

/// Adds the enabled `hooks` to the `hooks` key of `into`, in the shape of claude's settings.
fn add_hooks<'a>(into: &mut Value, hooks: impl Iterator<Item = (&'a String, &'a Hook)>) -> Result<(), Failure> {
    for (name, hook) in hooks.filter(|(_, h)| h.enabled) {
        let (Some(event), Some(command)) = (&hook.event, &hook.command) else {
            return Err(config_error(format!("hook `{name}` needs an event and a command")));
        };
        let mut entry = json!({ "type": "command", "command": command });
        if let Some(timeout) = &hook.timeout {
            let timeout = loop_manager_contract::config::duration(timeout)
                .map_err(|e| config_error(format!("hook `{name}`: {e}")))?;
            entry["timeout"] = json!(timeout.as_secs().max(1));
        }
        let matcher = json!({ "matcher": hook.matcher.clone().unwrap_or_default(), "hooks": [entry] });
        let shape = || config_error(format!("the claude native `hooks.{event}` must be a list of matchers"));
        let hooks = &mut into["hooks"];
        if hooks.is_null() {
            *hooks = json!({});
        }
        let Value::Object(hooks) = hooks else {
            return Err(shape());
        };
        match hooks.entry(event.clone()).or_insert(Value::Null) {
            Value::Array(list) => list.push(matcher),
            slot @ Value::Null => *slot = json!([matcher]),
            _ => return Err(shape()),
        }
    }
    Ok(())
}

/// The variables loop-manager sets for claude unless the agent's `env` or the native block's `env` names them.
const DEFAULTS: [(&str, &str); 7] = [
    ("CLAUDE_CODE_ENTRYPOINT", "loop-manager"),
    ("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1"),
    ("DISABLE_TELEMETRY", "1"),
    ("DISABLE_ERROR_REPORTING", "1"),
    ("DISABLE_FEEDBACK_COMMAND", "1"),
    ("DISABLE_AUTOUPDATER", "1"),
    ("CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY", "1"),
];

/// The variables that carry a credential; a session with one of them uses it, not the host's login.
pub(super) const CREDENTIALS: [&str; 3] = ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN"];

/// The whole environment of the adapter's process for the process part `p` of instance `id`.
pub(super) fn env(p: &Value, id: &str, socket: Option<&Path>) -> Result<Env, Failure> {
    env_with(p, id, socket, &host_dir())
}

/// [`env`] with `host` the host's claude config directory: an isolated session without a credential of its own gets
/// the host's login from there (docs/design/config.md). The daemon's `PATH`, `HOME` and `LANG`, and for a session
/// that inherits the host its network settings and claude config directory; then `env`, the attempt, the defaults,
/// no built-in subagent types (decision 143), and for an isolated session its `home` and the host's token.
pub(super) fn env_with(p: &Value, id: &str, socket: Option<&Path>, host: &Path) -> Result<Env, Failure> {
    let inherits = p["inherit_host"] != false;
    let daemon = inherits.then_some(loop_manager_contract::driver::HOST_NETWORK_ENV);
    let vars = ["PATH", "HOME", "LANG"]
        .into_iter()
        .chain(daemon.into_iter().flatten())
        .chain(inherits.then_some("CLAUDE_CONFIG_DIR"));
    let mut env = loop_manager_contract::driver::child::base_env(vars, &p["env"], id, socket)?;
    part::attempt(&mut env, p);
    let defaults = p["defaults"].as_object().into_iter().flatten();
    env.extend(defaults.filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned()))));
    env.push(("CLAUDE_AGENT_SDK_DISABLE_BUILTIN_AGENTS".to_owned(), "1".to_owned()));
    if let Some(home) = p["home"].as_str().filter(|_| !inherits) {
        env.push(("CLAUDE_CONFIG_DIR".to_owned(), home.to_owned()));
        if !CREDENTIALS.iter().any(|k| !p["env"][k].is_null()) {
            env.extend(host_token(host).map(|t| ("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), t)));
        }
    }
    Ok(env)
}

/// The host's claude config directory: the daemon's `CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub(super) fn host_dir() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => dir.into(),
        None => Path::new(&std::env::var_os("HOME").unwrap_or_default()).join(".claude"),
    }
}

/// The subscription access token of the login in `host` (`claudeAiOauth.accessToken` of `.credentials.json`),
/// read at every spawn, so a token the host refreshed reaches the next process.
// shortcut: the file only — macOS keeps the login in the keychain; read it there once macOS is checked
// (docs/overview.md).
fn host_token(host: &Path) -> Option<String> {
    let text = std::fs::read_to_string(host.join(".credentials.json")).ok()?;
    let credentials: Value = serde_json::from_str(&text).ok()?;
    credentials["claudeAiOauth"]["accessToken"].as_str().map(str::to_owned)
}

/// The account the gates use: a hash of where the credential comes from, never the credential itself
/// (docs/design/contracts.md 7); the native claude driver's, so an account keeps its id.
pub(super) fn account(_harness: &str, p: &Value) -> String {
    let env = &p["env"];
    let source = match CREDENTIALS.iter().find(|k| !env[**k].is_null()) {
        Some(key) => json!({ *key: env[*key] }),
        // Inherited or delivered, it is the host's login.
        None => json!({ "host": host_dir() }),
    };
    let hash = Fingerprint::of("claude", &ProcessPart(source)).0;
    hash[..16].to_owned()
}

/// Before the spawn: the `home` of an isolated session, and the plugin of the skills, rebuilt from the process part
/// (a skill as a link to its directory). Both directories are the owner's alone.
pub(super) fn start(p: &Value) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, symlink};
    let private = |dir: &Path| std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir);
    if let Some(home) = p["home"].as_str() {
        private(Path::new(home))?;
    }
    let Some(skills) = p["skills"].as_object().filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    let plugin = plugin(p);
    let _ = std::fs::remove_dir_all(&plugin);
    private(&plugin.join(".claude-plugin"))?;
    private(&plugin.join("skills"))?;
    let manifest =
        json!({ "name": "loop-manager", "version": "0.0.0", "description": "Skills delivered by loop-manager" });
    std::fs::write(plugin.join(".claude-plugin/plugin.json"), manifest.to_string())?;
    for (name, path) in skills {
        symlink(path.as_str().unwrap_or_default(), plugin.join("skills").join(name))?;
    }
    Ok(())
}

/// After the process exited: the plugin goes with it; the `home` stays, the session's transcript is in it.
pub(super) fn exited(p: &Value) {
    let _ = std::fs::remove_dir_all(plugin(p));
}

fn plugin(p: &Value) -> PathBuf {
    Path::new(p["files"].as_str().unwrap_or_default()).join("plugin")
}

/// The values of the native block's `env` and of the `env` and `headers` of the subagents' MCP servers: a key may
/// be among them.
pub(super) fn secrets(part: &Value) -> Vec<String> {
    let options = &part["_meta"]["claudeCode"]["options"];
    let values = |v: &Value| -> Vec<String> {
        let all = v.as_object().into_iter().flatten();
        all.filter_map(|(_, v)| v.as_str().map(str::to_owned)).collect()
    };
    let mut out = values(&options["settings"]["env"]);
    let agents = options["agents"].as_object().into_iter().flatten();
    let servers = agents.flat_map(|(_, a)| a["mcpServers"].as_array().into_iter().flatten());
    for server in servers.flat_map(|s| s.as_object().into_iter().flatten().map(|(_, s)| s)) {
        out.extend(values(&server["env"]));
        out.extend(values(&server["headers"]));
    }
    out
}

/// The `errorKind` values of a failed turn that mean the account is not logged in.
const AUTH: &[&str] = &[
    "authentication_failed",
    "oauth_org_not_allowed",
    "cloud_credential_error",
    "verification_required",
];

/// The failure of an error of the adapter (docs/design/adapters.md, "Profiles"): the JSON-RPC code first, then
/// `data.errorKind`, `data.startupFailureReason`, `data.cause`, a narrow text rule; what none of them names is
/// `unknown` with the adapter's words.
pub(super) fn classify(code: i32, message: &str, data: Option<&Value>) -> Failure {
    let generic = session::classify(code, message, data);
    if generic.kind != FailureKind::Unknown {
        return generic;
    }
    let data = data.unwrap_or(&Value::Null);
    let (kind, retry_at_ms) = kind(message, data).unwrap_or((FailureKind::Unknown, None));
    let mut failure = Failure::new(kind, Source::Harness);
    failure.raw = generic.raw;
    failure.retry_at_ms = retry_at_ms;
    // claude retries 429 and 5xx itself before it gives up.
    failure.retried_by_harness = matches!(kind, FailureKind::RateLimited | FailureKind::Overloaded);
    failure.text = (kind == FailureKind::Unknown).then(|| message.to_owned());
    failure
}

/// The kind the data of an error names, and when a quota is back (ms since the epoch). The text rules read claude's
/// words, which the adapter puts after `Internal error: ` in a `-32603`.
fn kind(message: &str, data: &Value) -> Option<(FailureKind, Option<i64>)> {
    let message = message.strip_prefix("Internal error: ").unwrap_or(message);
    let limit = &data["rateLimit"];
    let named = match data["errorKind"].as_str() {
        Some(e) if AUTH.contains(&e) => Some(FailureKind::AuthRequired),
        Some("billing_error" | "account_on_hold") => Some(FailureKind::Billing),
        Some("rate_limit") if limit["errorCode"] == "credits_required" => Some(FailureKind::Billing),
        Some("rate_limit") if limit["status"] == "rejected" || message.starts_with("You've hit your") => {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "seconds since the epoch fit an i64 in ms"
            )]
            let back = limit["resetsAt"].as_f64().map(|s| (s * 1000.0) as i64);
            return Some((FailureKind::QuotaExhausted, back));
        }
        Some("rate_limit") => Some(FailureKind::RateLimited),
        Some("overloaded" | "server_error") => Some(FailureKind::Overloaded),
        Some("model_not_found") => Some(FailureKind::ConfigError),
        Some("invalid_request") if message.starts_with("Prompt is too long") => Some(FailureKind::ContextOverflow),
        _ => None,
    };
    let crashed = !data["cause"].is_null() || message.starts_with("Claude Code stopped");
    let found = named
        .or_else(|| data["startupFailureReason"].as_str().and_then(startup_kind))
        .or_else(|| crashed.then_some(FailureKind::HarnessCrash));
    found.map(|kind| (kind, None))
}

/// A claude that could not start; what may pass by itself names no kind.
fn startup_kind(reason: &str) -> Option<FailureKind> {
    Some(match reason {
        "gateway_signin_required" | "gateway_access_denied" => FailureKind::AuthRequired,
        "session_held_by_background" => FailureKind::TranscriptDivergence,
        "org_pin_api_key_conflict"
        | "provider_not_allowed"
        | "org_pin_mismatch"
        | "managed_settings_invalid"
        | "proxy_invalid"
        | "temp_dir_unusable"
        | "cwd_unavailable"
        | "shell_tool_missing"
        | "worktree_resume_refused"
        | "worktree_unverified"
        | "cli_version_too_old"
        | "bypass_root"
        | "org_config_refused"
        | "sandbox_required_unavailable"
        | "initialize_not_first"
        | "config_file_invalid" => FailureKind::ConfigError,
        _ => return None,
    })
}

/// An MCP server of the session that did not start, or waits for a login nobody gives, is a `tool_failure`
/// (decision 119); the adapter tells it only in the title of a notice.
pub(super) fn notice(notice: &Notice) -> Option<Failure> {
    let server = notice.title.strip_prefix("MCP server \"")?;
    if !(server.ends_with("\" failed to start") || server.ends_with("\" needs authentication")) {
        return None;
    }
    let mut failure = Failure::new(FailureKind::ToolFailure, Source::Harness);
    failure.raw = match &notice.description {
        Some(description) => format!("{}: {description}", notice.title),
        None => notice.title.clone(),
    };
    Some(failure)
}

/// The oldest `claude-agent-acp` the profile supports: the version its contract suite runs on.
pub(crate) const MINIMUM: (u32, u32, u32) = (0, 109, 1);

/// The host checks of `loop-manager doctor`: whether a session can log in, and the stdio MCP servers of the host's
/// settings.
pub(crate) fn doctor(agents: DoctorAgents) -> Vec<Check> {
    // Inherited or delivered, every session without a key of its own gets the host's login.
    let file = host_dir().join(".credentials.json");
    vec![
        login_check("claude", &file, &CREDENTIALS, agents),
        host_mcp(&claude_json()),
    ]
}

/// The user file of claude: in `$CLAUDE_CONFIG_DIR` when set, else `~/.claude.json`.
fn claude_json() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR").map_or_else(
        || PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".claude.json"),
        |d| PathBuf::from(d).join(".claude.json"),
    )
}

/// The stdio MCP servers of the host's user file (docs/design/config.md): with host inheritance on, each starts in
/// every session, and the shared MCP of `loop-manager` does not cover them.
fn host_mcp(file: &Path) -> Check {
    let user: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap_or_default()).unwrap_or_default();
    let projects = (user["projects"].as_object().into_iter().flatten()).map(|(_, p)| &p["mcpServers"]);
    let mut names: Vec<&str> = std::iter::once(&user["mcpServers"])
        .chain(projects)
        .filter_map(Value::as_object)
        .flatten()
        .filter(|(_, s)| !s["command"].is_null())
        .map(|(n, _)| n.as_str())
        .collect();
    names.sort_unstable();
    names.dedup();
    host_mcp_check("claude", &names)
}
