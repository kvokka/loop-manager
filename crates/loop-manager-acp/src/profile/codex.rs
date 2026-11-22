//! The profile of `codex-acp`, the ACP adapter of codex (docs/design/adapters.md, "Profiles"): what codex takes beyond
//! ACP goes in the environment of the adapter's process, which the codex app-server it starts inherits: the homes,
//! and the codex config as the JSON of `CODEX_CONFIG`; the permission mode is the session's config option `mode`.

mod roles;

use super::{Env, Setup};
use crate::{part, session};
use loop_manager_contract::{
    config::{EffectiveConfig, Hook},
    driver::{
        Check, DoctorAgents, Failure, FailureKind, Fingerprint, HOST_NETWORK_ENV, ProcessPart, SessionPlan, Source,
        child::config_error, host_mcp_check, login_check,
    },
};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// The oldest `codex-acp` the profile supports: the version its contract suite runs on.
pub(crate) const MINIMUM: (u32, u32, u32) = (2, 9, 0);

/// The variables that carry a credential; a session with one of them uses it, not the host's login.
pub(super) const CREDENTIALS: [&str; 2] = ["OPENAI_API_KEY", "CODEX_API_KEY"];

/// The native key of the approval policy, codex's own config key.
const APPROVAL: &str = "approval_policy";
/// The native key of the sandbox, codex's own config key.
const SANDBOX: &str = "sandbox_mode";
/// The approval policy when no config sets one: the harness decides every permission and nothing waits for a person
/// (decision 117).
const DEFAULT_APPROVAL: &str = "never";
/// The sandbox when no config sets one (decision 38); `doctor` warns about it.
const DEFAULT_SANDBOX: &str = "danger-full-access";

/// The `config.toml` of an isolated session's own codex home: the keys codex reads only when its app-server starts,
/// which it ignores in the config of a thread. With plugins off codex does not clone its curated plugins from GitHub
/// into the home at every start.
const STARTUP_CONFIG: &str = r"check_for_update_on_startup = false

[analytics]
enabled = false

[feedback]
enabled = false

[features]
plugins = false
";

/// The modes of the adapter by the sandbox they run commands in, with the approval policies each stands for: the
/// adapter sets the policy and the sandbox of every turn by the session's mode alone.
const MODES: [(&str, &[&str], &str); 3] = [
    ("danger-full-access", &["never"], "agent-full-access"),
    ("workspace-write", &["never", "on-request"], "workspace-write"),
    ("read-only", &["never", "on-request"], "read-only"),
];

/// The native keys the mode overrides on every turn.
const BY_MODE: [&str; 2] = ["default_permissions", "approvals_reviewer"];

/// The first words of an error that is no limit to wait out but a plan or credit matter.
const BILLING_WORDS: &[&str] = &[
    "Quota exceeded.",
    "To use Codex with your ChatGPT plan",
    "Your workspace is out of credits.",
    "You hit your spend cap",
];

/// The words of the OpenAI API for a model it does not serve (`model_not_found`), which the adapter passes on alone:
/// the status of a provider's error in the standard shape does not reach the driver.
const NO_MODEL: &str = "does not exist or you do not have access to it";

/// The role files of a session, their text by path.
type Roles = Map<String, Value>;

/// The code of an error of the adapter whose app-server died ("Connection to Codex was lost.").
const LOST: i32 = 1001;

/// The process part: the generic one with `home` (`CODEX_HOME`), `user_home` (`HOME` of an isolated session, else
/// null), `host` (the host's codex home a session's own home links from, else null), `host_key` (the variable of the
/// key of the host's model provider, else null), `skills` (by name, the targets' among them), `config` (the codex
/// config of `CODEX_CONFIG`), `roles` (the role files, their text by path) and `login` (whether the adapter logs in by
/// the session's own key).
pub(super) fn process(
    harness: &str,
    setup: Setup,
    config: &EffectiveConfig,
    plan: &SessionPlan,
    state: &Path,
) -> Result<Value, Failure> {
    process_with(harness, setup, config, plan, state, &host_home())
}

/// [`process`] with `host` the host's codex home.
pub(super) fn process_with(
    harness: &str,
    setup: Setup,
    config: &EffectiveConfig,
    plan: &SessionPlan,
    state: &Path,
    host: &Path,
) -> Result<Value, Failure> {
    unsupported(harness, config).map_err(config_error)?;
    let mut p = part::fields(setup, config, plan);
    let isolated = !config.inherits_host();
    let keyed = CREDENTIALS.iter().any(|k| !p["env"][k].is_null());
    let skills = skills(config);
    let home = own_home(config, plan, state);
    // An isolated session does not read the host's config: the host's provider comes in its own, unless it has a key.
    let provider = (!(isolated && keyed)).then(|| self::provider(host)).flatten();
    let (codex, roles) = codex_config(config, plan, state, provider.as_ref().filter(|_| isolated))?;
    // A home of its own links the host's: all of it for a session that inherits the host, the login for an isolated
    // one without a key.
    let linked = home.is_some() && (!isolated || !keyed);
    p["home"] = json!(home.as_deref().unwrap_or(host));
    p["user_home"] = json!(home.filter(|_| isolated).map(|h| h.with_file_name("home")));
    p["host"] = json!(linked.then_some(host));
    p["host_key"] = json!(provider.and_then(|(_, table)| Some(table.get("env_key")?.as_str()?.to_owned())));
    p["skills"] = json!(skills);
    p["config"] = Value::Object(codex);
    p["roles"] = Value::Object(roles);
    p["login"] = json!(isolated && keyed);
    Ok(p)
}

/// The directory of the generated files of the attempt: `<state>/acp/codex/<run>-<attempt>`.
fn files(state: &Path, plan: &SessionPlan) -> PathBuf {
    let (SessionPlan::New { run, attempt, .. } | SessionPlan::Resume { run, attempt, .. }) = plan;
    state.join("acp/codex").join(format!("{run}-{attempt}"))
}

/// The `CODEX_HOME` of a session that needs its own, an isolated one or one with skills (codex reads them from
/// `CODEX_HOME/skills` only): the one of the session resumed, else `codex-home` under the attempt's files; none for a
/// session that inherits the host and has no skills, which runs in the host's.
fn own_home(config: &EffectiveConfig, plan: &SessionPlan, state: &Path) -> Option<PathBuf> {
    if config.inherits_host() && skills(config).is_empty() {
        return None;
    }
    let resumed = match plan {
        SessionPlan::Resume { session, .. } => session.0["home"].as_str().map(PathBuf::from),
        SessionPlan::New { .. } => None,
    };
    Some(resumed.unwrap_or_else(|| files(state, plan).join("codex-home")))
}

/// The enabled skills of the session and of its subagent targets, by name: the session's first, the same name is the
/// same directory (a load error otherwise).
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

/// The codex config of the session and its role files. In order, a later key over an earlier one: `notify = []`; the
/// host's model `provider`, when given, without the keys that hold secrets (the adapter puts the provider in the
/// app-server's argv); the native block without the approval policy and the sandbox, which the mode
/// carries; the model and the effort; the attempt environment in `shell_environment_policy.set` of the block; the MCP
/// servers over the block's of the same name; the hooks added to the block's; the roles of the targets over the
/// block's `agents`. An empty key is not written.
fn codex_config(
    config: &EffectiveConfig,
    plan: &SessionPlan,
    state: &Path,
    provider: Option<&(String, toml::Value)>,
) -> Result<(Map<String, Value>, Roles), Failure> {
    let mut codex = Map::from_iter([("notify".to_owned(), json!([]))]);
    if let Some((name, provider)) = provider {
        let mut table = provider.as_table().cloned().unwrap_or_default();
        table.retain(|k, _| k != "http_headers" && k != "experimental_bearer_token");
        codex.insert("model_provider".into(), json!(name));
        codex.insert("model_providers".into(), json!({ name: table }));
    }
    let native = config.native.as_ref().and_then(|n| n.value.as_object());
    let block = native
        .into_iter()
        .flatten()
        .filter(|(k, _)| *k != APPROVAL && *k != SANDBOX);
    codex.extend(block.map(|(k, v)| (k.clone(), v.clone())));
    for (key, value) in [("model", &config.model), ("model_reasoning_effort", &config.effort)] {
        if let Some(value) = value {
            codex.insert(key.into(), json!(value.value));
        }
    }
    let policy = table(&mut codex, "shell_environment_policy")?;
    let set = table(policy, "set")?;
    set.extend(attempt(plan));
    let servers = mcp(config)?;
    if !servers.is_empty() {
        table(&mut codex, "mcp_servers")?.extend(servers);
    }
    if add_hooks(&mut codex, config.hooks.iter().map(|(name, h)| (name, &h.value)))? {
        // A hook the adapter hands codex as `-c hooks=` runs only with the bypass of hook trust.
        codex.insert("bypass_hook_trust".into(), json!(true));
    }
    let (agents, roles) = roles::of(config, state)?;
    if !agents.is_empty() {
        codex.insert("agents".into(), Value::Object(agents));
    }
    Ok((codex, roles))
}

/// The table under `key` of `into`, made when absent; a value of another type is a config error.
fn table<'a>(into: &'a mut Map<String, Value>, key: &str) -> Result<&'a mut Map<String, Value>, Failure> {
    match into.entry(key).or_insert_with(|| json!({})) {
        Value::Object(table) => Ok(table),
        _ => Err(config_error(format!("the codex native `{key}` is a table"))),
    }
}

/// The attempt environment and the tracker actor of the attempt, as the core planned it (contracts 5.1).
fn attempt(plan: &SessionPlan) -> Map<String, Value> {
    let (SessionPlan::New {
        run,
        token,
        actor,
        bead,
        ..
    }
    | SessionPlan::Resume {
        run,
        token,
        actor,
        bead,
        ..
    }) = plan;
    let mut vars = Map::from_iter([
        ("LOOP_MANAGER_RUN".to_owned(), json!(run)),
        ("LOOP_MANAGER_ATTEMPT_TOKEN".to_owned(), json!(token)),
    ]);
    vars.extend(actor.iter().map(|(name, actor)| (name.clone(), json!(actor))));
    vars.extend(bead.iter().map(|bead| ("LOOP_MANAGER_BEAD".to_owned(), json!(bead))));
    vars
}

/// The MCP servers of `config` in the shape of codex's own config, by name. Each is `required`: a server
/// loop-manager gives that does not start fails the start of the session before the first prompt (decision 119).
fn mcp(config: &EffectiveConfig) -> Result<Map<String, Value>, Failure> {
    let mut servers = Map::new();
    for (name, server) in &config.mcp {
        let server = &server.value;
        // A service is a URL by now (the core binds it); a stdio server runs per session whatever its scope.
        let mut entry = match (&server.command, &server.url) {
            (Some(command), None) if !command.is_empty() => {
                json!({ "command": command[0], "args": command[1..], "env": server.env })
            }
            (None, Some(url)) => json!({ "url": url, "http_headers": server.headers }),
            _ => return Err(config_error(format!("MCP server `{name}` needs a command or a url"))),
        };
        entry["required"] = json!(true);
        servers.insert(name.clone(), entry);
    }
    Ok(servers)
}

/// Adds the enabled `hooks` to the `hooks` table of `into`, in the shape of codex's config; whether there was one.
/// A hook runs with the environment of the process, which is the attempt's.
fn add_hooks<'a>(
    into: &mut Map<String, Value>,
    hooks: impl Iterator<Item = (&'a String, &'a Hook)>,
) -> Result<bool, Failure> {
    let mut added = false;
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
        match table(into, "hooks")?.entry(event.clone()).or_insert_with(|| json!([])) {
            Value::Array(list) => list.push(matcher),
            _ => {
                return Err(config_error(format!(
                    "the codex native `hooks.{event}` is a list of matchers"
                )));
            }
        }
        added = true;
    }
    Ok(added)
}

/// What the session part adds: no MCP servers in ACP (they go in the codex config, where codex starts them as it
/// does its own), no `model` and `effort` options (the codex config carries them: the adapter offers only the models
/// codex lists); and, outside ACP, the permission `mode`, the `home` of a session with its own `CODEX_HOME`, and
/// `secrets`, the values of the `env` and `headers` of its MCP servers.
pub(super) fn session(
    config: &EffectiveConfig,
    plan: &SessionPlan,
    state: &Path,
    part: &mut Value,
) -> Result<(), Failure> {
    session_with(config, plan, state, part, &host_home())
}

/// [`session`] with `host` the host's codex home, whose `sandbox_mode` a session that inherits the host takes.
pub(super) fn session_with(
    config: &EffectiveConfig,
    plan: &SessionPlan,
    state: &Path,
    part: &mut Value,
    host: &Path,
) -> Result<(), Failure> {
    let native = config.native.as_ref().and_then(|n| n.value.as_object());
    let inherited = (config.inherits_host().then(|| host_config(host)).flatten())
        .and_then(|c| c.get(SANDBOX)?.as_str().map(str::to_owned));
    part["mode"] = json!(mode(&config.harness, native, inherited.as_deref()).map_err(config_error)?);
    part["home"] = json!(own_home(config, plan, state));
    part["model"] = Value::Null;
    part["effort"] = Value::Null;
    part["mcpServers"] = json!([]);
    let secrets = (config.mcp.values()).flat_map(|s| s.value.env.values().chain(s.value.headers.values()));
    part["secrets"] = json!(secrets.collect::<Vec<_>>());
    Ok(())
}

/// The permission mode of a session: by the native `approval_policy` (else `never`) and `sandbox_mode`, else the
/// `inherited` one of the host's codex config, else `danger-full-access` (decision 38); the error names the key.
fn mode(harness: &str, native: Option<&Map<String, Value>>, inherited: Option<&str>) -> Result<&'static str, String> {
    let offered = format!(
        "the ACP harness `{harness}` offers `never` with `danger-full-access`, and `never` or `on-request` with \
         `workspace-write` or `read-only`"
    );
    let refused = || format!("`native.{harness}.{APPROVAL}`: {offered}");
    let approval = match native.and_then(|n| n.get(APPROVAL)) {
        None => DEFAULT_APPROVAL,
        Some(Value::String(approval)) => approval,
        Some(_) => return Err(refused()),
    };
    let (sandbox, key) = match native.and_then(|n| n.get(SANDBOX)) {
        None => (
            inherited.unwrap_or(DEFAULT_SANDBOX),
            "the host's codex `sandbox_mode`".to_owned(),
        ),
        Some(sandbox) => (
            sandbox.as_str().unwrap_or_default(),
            format!("`native.{harness}.{SANDBOX}`"),
        ),
    };
    match MODES.iter().find(|(s, _, _)| *s == sandbox) {
        None => Err(format!("{key}: {offered}")),
        Some((_, approvals, mode)) if approvals.contains(&approval) => Ok(mode),
        Some(_) => Err(refused()),
    }
}

/// What a codex agent's config names that the adapter never takes, by key: a native block that is no table, an
/// approval policy and a sandbox no mode stands for (a sandbox the block does not name is taken as the default one),
/// and the keys the mode overrides on every turn.
pub(super) fn unsupported(harness: &str, config: &EffectiveConfig) -> Result<(), String> {
    let native = match config.native.as_ref().map(|n| &n.value) {
        None => None,
        Some(Value::Object(native)) => Some(native),
        Some(_) => {
            return Err(format!(
                "`native.{harness}`: the codex native block is a table of codex config keys"
            ));
        }
    };
    if let Some(key) = BY_MODE.iter().find(|k| native.is_some_and(|n| n.contains_key(**k))) {
        return Err(format!(
            "`native.{harness}.{key}`: the ACP harness `{harness}` sets it by the permission mode on every turn"
        ));
    }
    mode(harness, native, None).map(drop)
}

/// The whole environment of the adapter's process for the process part `p` of instance `id`: the daemon's `PATH`,
/// `LANG`, `SHELL` (codex runs commands in a login shell), and for a session without an own `HOME` the daemon's
/// `HOME` and network settings; then `env` and the attempt; `CODEX_HOME`, `HOME` of an isolated session, the key of
/// the host's model provider from the daemon's environment, `CODEX_CONFIG`, and `DEFAULT_AUTH_REQUEST` for a session
/// the adapter logs in by its own key.
pub(super) fn env(p: &Value, id: &str, socket: Option<&Path>) -> Result<Env, Failure> {
    let inherits = p["user_home"].is_null();
    let daemon = inherits.then_some(HOST_NETWORK_ENV);
    let vars = ["PATH", "LANG", "SHELL"]
        .into_iter()
        .chain(inherits.then_some("HOME"))
        .chain(daemon.into_iter().flatten());
    let mut env = loop_manager_contract::driver::child::base_env(vars, &p["env"], id, socket)?;
    part::attempt(&mut env, p);
    let text = |v: &Value| v.as_str().map(str::to_owned);
    env.extend(text(&p["home"]).map(|h| ("CODEX_HOME".to_owned(), h)));
    env.extend(text(&p["user_home"]).map(|h| ("HOME".to_owned(), h)));
    env.extend(text(&p["host_key"]).and_then(|k| Some((k.clone(), std::env::var(k).ok()?))));
    env.push(("CODEX_CONFIG".to_owned(), p["config"].to_string()));
    if p["login"] == true {
        // The adapter logs codex in by the key of the environment, and codex writes it to the session's own home.
        env.push((
            "DEFAULT_AUTH_REQUEST".to_owned(),
            json!({ "methodId": "api-key" }).to_string(),
        ));
    }
    Ok(env)
}

/// Before the spawn: the role files; and for a session with its own homes, both (mode 0700), `config.toml` of an
/// isolated one ([`STARTUP_CONFIG`], mode 0600, rewritten at every start), the links of its skills
/// rebuilt (a link per skill to its directory; for a session that inherits the host, the host's skills too, its own
/// first), and the links to the host's files: `auth.json`, `config.toml` and `AGENTS.md` for a session that inherits
/// the host, `auth.json` for an isolated one without a key. Hidden entries of `skills` are codex's own: they stay.
pub(super) fn start(p: &Value) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, symlink};
    roles::write(p)?;
    let host = p["host"].as_str().map(Path::new);
    let inherits = p["user_home"].is_null();
    if inherits && host.is_none() {
        // The host's own home.
        return Ok(());
    }
    let private = |dir: &Path| std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir);
    let home = Path::new(p["home"].as_str().unwrap_or_default());
    private(home)?;
    if let Some(user_home) = p["user_home"].as_str() {
        private(Path::new(user_home))?;
        write_private(&home.join("config.toml"), STARTUP_CONFIG)?;
    }
    let shown = |name: &str| !name.starts_with('.');
    let dir = home.join("skills");
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        if shown(&entry.file_name().to_string_lossy()) && entry.file_type()?.is_symlink() {
            std::fs::remove_file(entry.path())?;
        }
    }
    let own = (p["skills"].as_object().into_iter().flatten())
        .map(|(name, path)| (name.clone(), PathBuf::from(path.as_str().unwrap_or_default())));
    let hosts = (host
        .filter(|_| inherits)
        .and_then(|h| std::fs::read_dir(h.join("skills")).ok()))
    .into_iter()
    .flatten()
    .flatten()
    .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
    .filter(|(name, _)| shown(name));
    for (name, path) in own.chain(hosts) {
        let link = dir.join(name);
        if link.symlink_metadata().is_err() {
            private(&dir)?;
            symlink(path, link)?;
        }
    }
    let Some(host) = host else { return Ok(()) };
    let files: &[&str] = match inherits {
        true => &["auth.json", "config.toml", "AGENTS.md"],
        false => &["auth.json"],
    };
    for file in files {
        let (target, link) = (host.join(file), home.join(file));
        if target.exists() && link.symlink_metadata().is_err() {
            symlink(target, link)?;
        }
    }
    Ok(())
}

/// `text` written to `path`, readable by its owner alone.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let mut file = (std::fs::OpenOptions::new().write(true).create(true).truncate(true))
        .mode(0o600)
        .open(path)?;
    file.write_all(text.as_bytes())
}

/// The values of the session part's `secrets`.
pub(super) fn secrets(part: &Value) -> Vec<String> {
    let all = part["secrets"].as_array().into_iter().flatten();
    all.filter_map(|s| s.as_str().map(str::to_owned)).collect()
}

/// The failure of an error of the adapter (docs/design/adapters.md, "Profiles"): the JSON-RPC code first, then the
/// words of codex the adapter passes on, as the message or, for a request that failed inside the adapter ("Internal
/// error"), as `data.details`; what none of them names is `unknown` with those words.
pub(super) fn classify(code: i32, message: &str, data: Option<&Value>) -> Failure {
    let generic = session::classify(code, message, data);
    if generic.kind != FailureKind::Unknown {
        return generic;
    }
    let words = data.and_then(|d| d["details"].as_str()).unwrap_or(message);
    let (kind, retried) = kind(code, words).unwrap_or((FailureKind::Unknown, false));
    let mut failure = Failure::new(kind, Source::Harness);
    failure.raw = generic.raw;
    failure.retried_by_harness = retried;
    failure.text = (kind == FailureKind::Unknown).then(|| words.to_owned());
    failure
}

/// The kind an error names, and whether codex retried before it gave up.
fn kind(code: i32, message: &str) -> Option<(FailureKind, bool)> {
    if code == LOST {
        return Some((FailureKind::HarnessCrash, false));
    }
    let worded = if message.contains("required MCP servers failed to initialize") {
        Some(FailureKind::ToolFailure)
    } else if BILLING_WORDS.iter().any(|w| message.starts_with(w)) {
        Some(FailureKind::Billing)
    } else if message.contains("hit your usage limit") {
        Some(FailureKind::QuotaExhausted)
    } else if message.contains("ran out of room in the model's context window") {
        Some(FailureKind::ContextOverflow)
    } else if message.starts_with("Selected model is at capacity") {
        Some(FailureKind::Overloaded)
    } else if message.contains(NO_MODEL) {
        Some(FailureKind::ConfigError)
    } else {
        None
    };
    if let Some(kind) = worded {
        return Some((kind, kind == FailureKind::Overloaded));
    }
    if let Some(status) = status(message, "exceeded retry limit, last status: ") {
        return Some((by_status(status)?, true));
    }
    let kind = by_status(status(message, "unexpected status ")?)?;
    // codex retries 5xx itself before it gives up.
    Some((kind, kind == FailureKind::Overloaded))
}

/// The HTTP status right after `marker` in `message`.
fn status(message: &str, marker: &str) -> Option<u16> {
    let rest = &message[message.find(marker)? + marker.len()..];
    let digits = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..digits].parse().ok()
}

/// The kind of a provider's HTTP status.
fn by_status(status: u16) -> Option<FailureKind> {
    Some(match status {
        429 => FailureKind::RateLimited,
        408 | 500..=504 | 529 => FailureKind::Overloaded,
        401 | 403 => FailureKind::AuthRequired,
        400 | 404 => FailureKind::ConfigError,
        _ => return None,
    })
}

/// The account the gates use: a hash of where the credential comes from, never the credential itself
/// (docs/design/contracts.md 7); the native codex driver's, so an account keeps its id and its gates.
pub(super) fn account(_harness: &str, p: &Value) -> String {
    let env = &p["env"];
    let source = match CREDENTIALS.iter().find(|k| !env[**k].is_null()) {
        Some(key) => json!({ *key: env[*key] }),
        // Inherited or linked, it is the host's login.
        None => json!({ "host": host_home() }),
    };
    Fingerprint::of("codex", &ProcessPart(source)).0[..16].to_owned()
}

/// The host's codex home: the daemon's `CODEX_HOME`, else `~/.codex`.
pub(super) fn host_home() -> PathBuf {
    match std::env::var_os("CODEX_HOME") {
        Some(dir) => dir.into(),
        None => Path::new(&std::env::var_os("HOME").unwrap_or_default()).join(".codex"),
    }
}

/// `config.toml` in `home`, if it is there and reads.
fn host_config(home: &Path) -> Option<toml::Table> {
    toml::from_str(&std::fs::read_to_string(home.join("config.toml")).ok()?).ok()
}

/// The model provider `config.toml` in `home` selects, by name, if the file defines it.
fn provider(home: &Path) -> Option<(String, toml::Value)> {
    let config = host_config(home)?;
    let name = config.get("model_provider")?.as_str()?;
    Some((name.to_owned(), config.get("model_providers")?.get(name)?.clone()))
}

/// Decision 38: a session runs with no sandbox unless the native block of its config, or the host's codex config in
/// `home` it inherits, sets one.
fn unconfined(config: &EffectiveConfig, home: &Path) -> bool {
    let native = config.native.as_ref().and_then(|n| n.value.as_object());
    let host_sets = config.inherits_host() && host_config(home).is_some_and(|c| c.contains_key(SANDBOX));
    !native.is_some_and(|n| n.contains_key(SANDBOX)) && !host_sets
}

/// The host checks of `loop-manager doctor`: whether a session can log in, the stdio MCP servers of the host's
/// codex config, and the agents that run with no sandbox.
pub(crate) fn doctor(agents: DoctorAgents) -> Vec<Check> {
    let host = host_home();
    vec![login(&host, agents), host_mcp(&host), sandbox(&host, agents)]
}

/// The host's login file or a key of the agent's own `env`, as for any harness; or, with a model provider in the
/// host config, the daemon's variable it names in `env_key`: the profile passes that one on. Such a provider logs
/// codex in without `auth.json`, so its absence says nothing: a provider with no `env_key` is not checked.
fn login(host: &Path, agents: DoctorAgents) -> Check {
    let check = |level, detail: String| Check::new("codex login", level, detail);
    let provider = provider(host);
    let key = (provider.as_ref()).and_then(|(_, p)| Some(p.get("env_key")?.as_str()?.to_owned()));
    match (provider.map(|(name, _)| name), key) {
        (_, Some(key)) if std::env::var_os(&key).is_some() => check("ok", format!("{key} is set")),
        (Some(name), Some(key)) => check(
            "warning",
            format!("{key}, the `env_key` of the host's model provider `{name}`, is not set for the daemon"),
        ),
        (Some(name), None) => check(
            "warning",
            format!("not checked: the host's model provider `{name}` has no `env_key`"),
        ),
        (None, _) => login_check("codex", &host.join("auth.json"), &CREDENTIALS, agents),
    }
}

/// The stdio MCP servers of the host's `config.toml`: each starts in every session that reads it.
fn host_mcp(host: &Path) -> Check {
    let config = host_config(host).unwrap_or_default();
    let servers = (config.get("mcp_servers").and_then(toml::Value::as_table))
        .into_iter()
        .flatten();
    let names: Vec<&str> = (servers.filter(|(_, s)| s.get("command").is_some()))
        .map(|(n, _)| n.as_str())
        .collect();
    host_mcp_check("codex", &names)
}

/// Decision 38: the codex sandbox needs user namespaces, which many hosts lack, so a codex agent whose config sets no
/// `sandbox_mode` runs in the mode `agent-full-access`; the warning names every such agent, per rig. A config that
/// does not assemble cannot be shown to set it, so its agent is named too.
fn sandbox(host: &Path, agents: DoctorAgents) -> Check {
    let open: Vec<&str> = (agents.iter())
        .filter(|(_, c)| c.as_ref().is_none_or(|c| unconfined(c, host)))
        .map(|(label, _)| label.as_str())
        .collect();
    match (agents.is_empty(), open.is_empty()) {
        (true, _) => Check::new("codex sandbox", "ok", "no codex agents"),
        (_, true) => Check::new("codex sandbox", "ok", "every codex agent sets sandbox_mode"),
        (_, false) => Check::new(
            "codex sandbox",
            "warning",
            format!(
                "sandbox_mode = \"danger-full-access\" (mode `agent-full-access`), commands not confined to the \
                 workdir: {}",
                open.join(", ")
            ),
        ),
    }
}

#[cfg(test)]
mod tests;
