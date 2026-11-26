//! Configuration: the files of the orchestrator, projects, packs, agents, formulas, triggers and addons
//! (docs/guide/addons.md; docs/design/config.md), loaded and validated as a whole.

mod addon;
mod assemble;
mod check;
mod compose;
mod defaults;
mod dependency;
mod env;
mod files;
mod formula_file;
mod items;
mod load;
mod places;
mod pricing;
mod prompt;
mod retired;
pub(crate) mod schema;
mod service;
mod sources;
mod subagents;
mod variants;

use crate::{
    driver::Actor,
    template::{Filter, Inputs},
};
pub use assemble::{Attached, EffectiveConfig, Layer, Sourced};
pub use dependency::{Dependency, DependencyScope};
pub use env::{ExecEnv, env_literal, env_value};
pub use files::{AddonUse, Harness, Hook, Mcp, McpScope, Settings, Skill};
pub use formula_file::{Formula, Input, InputType, OnItemFailure, Step, StepKind};
pub use places::Scope;
pub use pricing::{Price, Pricing, Rates, Source as PriceSource, catalog};
pub use prompt::{Prompt, TEMPLATE};
pub use service::{Health, Service, ServiceScope};
pub use sources::{Checkouts, GitSource, commit_id, git_sources};
use std::{collections::BTreeMap, fmt, path::PathBuf, time::Duration};

/// The whole configuration of one daemon. Loading it is all or nothing: the caller keeps its previous `Config`
/// when [`load`] fails (docs/design/config.md).
#[derive(Clone, Debug)]
pub struct Config {
    /// Per-adapter selection of plugins whose previously trusted hooks can be renewed on update.
    pub plugin_hook_updates: BTreeMap<String, Vec<String>>,
    /// Runs started at once across every project (docs/design/contracts.md 6).
    pub max_runs: u32,
    /// Runs of one bead that may end `failed` before the core closes its gate (docs/design/config.md).
    pub bead_failed_runs: u32,
    /// The loopback address the API also listens on with a token (docs/api/frontends.md); off when absent.
    pub api_tcp: Option<std::net::SocketAddr>,
    /// The token of `api_tcp` for this daemon start: not from a file, the daemon sets it.
    pub api_token: String,
    /// The zone `cron` schedules are read in: `timezone` of the orchestrator config, else the system one.
    pub timezone: jiff::tz::TimeZone,
    pub harnesses: BTreeMap<String, Harness>,
    /// Layer 2 of docs/design/config.md.
    pub settings: Settings,
    /// The `[agents.<name>]` edits (no `base`), by agent reference; they may name an agent of any project.
    pub edits: BTreeMap<String, Settings>,
    /// The agent variants every project sees, by name; a project's own of the same name replaces one.
    pub variants: BTreeMap<String, Variant>,
    /// The adapters of the build: the short model names of each harness. The harnesses they name are the root's.
    pub adapters: &'static [&'static crate::driver::Adapter],
    pub addons: Vec<Addon>,
    /// The enabled bundled addons with their parameters, run after every declared addon.
    pub bundled: Vec<Bundled>,
    /// The thresholds of the watchdog (invariant 15): a reload applies to deadlines set from then on.
    pub watchdog: Watchdog,
    pub projects: Vec<Project>,
    /// The command prefix that serves a stdio MCP server over HTTP on `LOOP_MANAGER_SERVICE_PORT` (docs/design/config.md): a
    /// stdio server not of scope `session` runs behind it as a service. Empty: no bridge, stdio servers stay per
    /// session. [`load`] leaves it empty; the binary sets it.
    pub bridge: Vec<String>,
    /// Where the daemon keeps the data of addons: `${LOOP_MANAGER_ADDON_DATA}` of addon `p` is `<addon_data>/p`. `None`:
    /// [`load`] leaves the reference as it is; the binary sets it.
    pub addon_data: Option<PathBuf>,
    /// How long a service may take to pass its health check, and a launch waits for it (docs/design/config.md): 30 s.
    /// Not from a file: [`load`] sets it, a test shortens it.
    pub service_wait: Duration,
    /// Runtime prefix for service commands. The Linux daemon supplies its parent-death launcher.
    pub service_prefix: Vec<String>,
    /// The user's `[[pricing]]` over the shipped catalog: a reload applies to charges from then on.
    pub pricing: Pricing,
}

/// The thresholds of `[watchdog]` in the orchestrator file, in milliseconds (contracts 6.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Watchdog {
    pub silence_ms: i64,
    pub tool_silence_ms: i64,
    pub background_silence_ms: i64,
    pub start_timeout_ms: i64,
    pub queue_timeout_ms: i64,
    pub park_timeout_ms: i64,
}

/// Ceiling of the core: attempts per run, not counting those that parked on an account gate (invariant 16).
pub const MAX_ATTEMPTS: u32 = 20;

/// The addons bundled with loop-manager (docs/design/config.md): name, the events they hook, and whether they are on with no
/// `[builtin.<name>]` table. `worktree` has no table: it is on in a project with a formula or a `drain` step of
/// `worktree = true`.
pub const BUNDLED: [(&str, &[&str], bool); 2] = [
    (
        "default-policy",
        &["run.attempt_failed", "run.turn_ended_without_outcome"],
        true,
    ),
    (
        "worktree",
        &[
            "run.preparing",
            "run.finishing",
            "run.passed",
            "run.failed",
            "run.cancelled",
        ],
        false,
    ),
];

/// An enabled bundled addon; `params` is its `[builtin.<name>]` table without `enabled`.
#[derive(Clone, Debug)]
pub struct Bundled {
    pub name: &'static str,
    pub on: &'static [&'static str],
    pub params: serde_json::Value,
}

/// The facts of a run's place the core gives the bundled `worktree` as its hook parameters `{"run": <Place>}`, never
/// in an event (docs/design/config.md "Bundled addons"): the core knows the lineage and the config, the addon git.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Place {
    /// The directory the run's base is in: its place's own, or the workdir of the nearest ancestor of a step on it.
    pub dir: PathBuf,
    /// The ancestors of a step whose lazy worktree in the repository of `dir` is the run's base, nearest first: the
    /// first is the base, made from the worktree of the first of the others that exists, else from the HEAD of
    /// `dir`. Empty: the base is `dir` itself.
    pub lazy: Vec<String>,
    /// The run gets a worktree of its own: its formula has `worktree = true`, or it is a child of a `drain` step
    /// with `worktree = true`.
    pub own: bool,
    /// The run continues the session of another (`continue_from`) and works where that one did.
    pub continued: bool,
    /// The directories of the other places where the run had children, when its formula has `worktree = true`:
    /// its lazy worktrees are there.
    pub rigs: Vec<PathBuf>,
    /// The title of the run's bead when it is a child of a `drain` step with `worktree = true`: its branch merges
    /// into its base on its final `pass`, under this title.
    pub merge: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Project {
    pub name: String,
    /// The directory of `loop-manager.toml`: the config files, packs and tracker of the project live here.
    pub dir: PathBuf,
    /// The root's name and the root: a place like a rig, without its triggers and settings (docs/design/config.md).
    pub root: (String, Rig),
    /// Layer 3.
    pub settings: Settings,
    pub edits: BTreeMap<String, Settings>,
    pub variants: BTreeMap<String, Variant>,
    /// The addons its own file declares, then those its packs declare that neither it nor the orchestrator does.
    pub addons: Vec<Addon>,
    /// The bundled addons on for its runs: the orchestrator's, with the project's `[builtin.<name>]` tables.
    pub bundled: Vec<Bundled>,
    pub rigs: BTreeMap<String, Rig>,
    /// The project root pack (named after the project, holding the project triggers) first.
    pub packs: Vec<Pack>,
    /// The prompt of each agent its formulas' `ai` steps name, by the pack agent the step names.
    pub prompts: BTreeMap<Name, Prompt>,
    pub tracker: TrackerLink,
    /// `[exec]` of the orchestrator and the project: the environment of a check script of a step that is no exec
    /// step (an exec step's own is in its kind).
    pub exec: ExecEnv,
}

/// How the project's sessions and commands reach its tracker (docs/design/config.md). The core knows no tracker
/// (decision 43): the integration that makes trackers fills this in after loading; empty until then.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackerLink {
    /// The project has a tracker.
    pub present: bool,
    /// Variables every session and command of the project gets, tracker or not, under every layer: they name the
    /// tracker explicitly, so a tool's own search never climbs to another one.
    pub env: BTreeMap<String, String>,
    /// The variable that carries the attempt's tracker actor, `loop-manager:<run>:<attempt>` (contracts 5.1), into a session
    /// or a command.
    pub actor: Option<String>,
    /// The variable that gives the beads a session or command files on a rig that rig's prefix
    /// (docs/design/config.md).
    pub prefix: Option<String>,
    /// The variables that tell the tracker the agent of the step, the harness and the model of a session.
    pub agent: Option<String>,
    pub harness: Option<String>,
    pub model: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Rig {
    pub path: PathBuf,
    /// The prefix of the ids of its beads (docs/design/config.md); none for the root, whose beads carry the
    /// tracker's own.
    pub prefix: Option<String>,
    pub vars: BTreeMap<String, String>,
    /// Pack triggers that run on this rig.
    pub triggers: Vec<Name>,
    /// Layer 4.
    pub settings: Settings,
    pub edits: BTreeMap<String, Settings>,
    /// The variant that runs in place of a pack agent the formulas name, on this rig.
    pub agents: BTreeMap<Name, String>,
    /// The most runs that work in the rig's own directory at once (docs/design/config.md "Project"); none for the root.
    pub max_writers: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct Pack {
    pub name: String,
    pub root: PathBuf,
    /// Its `bin/` directory, when it has one: ahead of `PATH` for the exec steps and check scripts of its formulas.
    /// `None` for the project root pack.
    pub bin: Option<PathBuf>,
    /// `None` for the project root pack.
    pub version: Option<String>,
    pub requires_addons: Vec<String>,
    pub agents: BTreeMap<String, Agent>,
    pub formulas: BTreeMap<String, Formula>,
    pub triggers: Vec<Trigger>,
    /// `[settings]` of `pack.toml`: the launch settings of the runs of its formulas.
    pub settings: Settings,
}

#[derive(Clone, Debug)]
pub struct Agent {
    /// Its directory in the pack: `agent.toml`, the role prompt, the prompt template and the files it names.
    pub dir: PathBuf,
    /// The file of its prompt template in `dir`: `template` of `agent.toml`, else `prompt.template.md`.
    pub template: String,
    /// The harness `agent.toml` names, part of the agent layer.
    pub harness: Option<String>,
    /// Where it works; an agent of the root takes no rig's layer, edits or variant.
    pub scope: Scope,
    /// The agent layer but its harness; its `system_prompt` is the role prompt file (docs/design/config.md) when the
    /// agent has one.
    pub settings: Settings,
    /// Runs of this agent started at once in its project; no limit but the orchestrator's when absent.
    pub max_runs: Option<u32>,
}

/// A pack agent under another name with settings of its own (`[agents.<name>]`): the role is the base agent's,
/// the limit, gates and events are the variant's.
#[derive(Clone, Debug)]
pub struct Variant {
    /// The pack agent, as written; it resolves in the project of the run.
    pub base: Name,
    pub settings: Settings,
    /// Its own limit; the base agent's when absent.
    pub max_runs: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct Trigger {
    pub name: String,
    pub source: Source,
    pub run: Name,
    pub inputs: Inputs,
    /// Set only on a project trigger; a pack trigger gets its rigs from [`Rig::triggers`].
    pub rig: Option<String>,
}

#[derive(Clone, Debug)]
pub enum Source {
    Event {
        on: String,
        filter: Filter,
    },
    Cron(crate::cron::Cron),
    /// At most one firing per interval.
    Every(Duration),
}

#[derive(Clone, Debug)]
pub struct Addon {
    pub dependencies: BTreeMap<String, Dependency>,
    pub name: String,
    /// The pack whose `pack.toml` declares it for its project; `None` when the orchestrator file or the project's
    /// does.
    pub pack: Option<String>,
    pub version: String,
    /// The harnesses it is for; any when `None`.
    pub harnesses: Option<Vec<String>>,
    pub root: PathBuf,
    /// Layer 5 contribution to the agents that attach the addon.
    pub agent: Settings,
    /// The text its attached agents get after their system prompt.
    pub prompt: Option<String>,
    /// Its `bin/` directory, when it has one: ahead of `PATH` for its attached agents.
    pub bin: Option<PathBuf>,
    pub hooks: Vec<OrchestratorHook>,
    /// `[params]` of `addon.toml`: each parameter's default, which gives its type.
    pub params: serde_json::Map<String, serde_json::Value>,
}

/// A command on a core event (docs/design/contracts.md 3).
#[derive(Clone, Debug)]
pub struct OrchestratorHook {
    pub on: String,
    pub command: Vec<String>,
    pub timeout: Option<Duration>,
}

/// An item of a pack: an agent, formula or trigger, always resolved to its pack.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Name {
    pub pack: String,
    pub name: String,
}

impl Project {
    /// The formula a run of this project names; a bare name resolves as written in the project root pack
    /// (docs/design/config.md).
    pub fn formula(&self, reference: &str) -> Result<(Name, &Formula), String> {
        let own = self.packs.first().map(|p| p.name.as_str()).unwrap_or_default();
        let has = |p: &Pack, n: &str| p.formulas.contains_key(n);
        let name = check::resolve(&self.packs, own, &load::reference(reference), has, "formula")?;
        let pack = self.packs.iter().find(|p| p.name == name.pack);
        let formula = pack
            .and_then(|p| p.formulas.get(&name.name))
            .ok_or(format!("no formula `{name}`"))?;
        Ok((name, formula))
    }
}

impl Config {
    /// The tracker actor of attempt `attempt` of run `run` in `project` (contracts 5.1): the variable and
    /// `loop-manager:<run>:<attempt>`; none when the project has no actor variable.
    pub fn actor(&self, project: &str, run: &str, attempt: u32) -> Option<(String, String)> {
        let p = self.projects.iter().find(|p| p.name == project)?;
        Some((p.tracker.actor.clone()?, format!("loop-manager:{run}:{attempt}")))
    }

    /// The tracker variables of a session of attempt `attempt` of run `run` in `project`, at a step of `agent`
    /// (contracts 5.1); none when the project has no actor variable.
    pub fn session_actor(&self, project: &str, run: &str, attempt: u32, agent: &str) -> Option<Actor> {
        let tracker = &self.projects.iter().find(|p| p.name == project)?.tracker;
        Some(Actor {
            actor: self.actor(project, run, attempt)?,
            agent: tracker.agent.clone().map(|name| (name, agent.to_owned())),
            harness: tracker.harness.clone(),
            model: tracker.model.clone(),
        })
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.pack, self.name)
    }
}

/// One problem found while loading, with the file it is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    pub file: PathBuf,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.file.display(), self.message)
    }
}

/// Loads the orchestrator file (absent means empty), the projects it lists and everything they use, for a build
/// with `adapters` (see [`Config::adapters`]) and the git sources downloaded to `checkouts`. Every error found is
/// returned; a config with any error is never returned in part.
pub fn load_for(
    orchestrator_file: &std::path::Path,
    adapters: &'static [&'static crate::driver::Adapter],
    checkouts: Checkouts,
) -> Result<Config, Vec<ConfigError>> {
    let loader = load::Loader {
        adapters,
        checkouts,
        ..load::Loader::default()
    };
    loader.run(orchestrator_file)
}

/// [`load_for`] a build with no adapters, as tests of the core on a scripted harness load: every agent names its
/// harness, and no model name is expanded.
pub fn load(orchestrator_file: &std::path::Path) -> Result<Config, Vec<ConfigError>> {
    load_for(orchestrator_file, &[], Checkouts::new())
}

/// The environment variable an exec step gets input `name` in: `note-text` → `LOOP_MANAGER_INPUT_NOTE_TEXT`.
pub fn input_variable(name: &str) -> String {
    format!("LOOP_MANAGER_INPUT_{}", upper(name))
}

/// The name part of an environment variable: `note-text` → `NOTE_TEXT`.
pub fn upper(name: &str) -> String {
    let upper = name.chars().map(|c| {
        if c.is_ascii_alphanumeric() {
            c.to_ascii_uppercase()
        } else {
            '_'
        }
    });
    upper.collect()
}

/// A duration with one unit: `"500ms"`, `"30s"`, `"45m"`, `"2h"` (contracts conventions).
pub fn duration(text: &str) -> Result<Duration, String> {
    let split = text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let n: u64 = number
        .parse()
        .map_err(|_| format!("`{text}` is not a duration like \"30s\""))?;
    let seconds = match unit {
        "ms" => return Ok(Duration::from_millis(n)),
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => return Err(format!("`{text}`: the unit is one of ms, s, m, h")),
    };
    n.checked_mul(seconds)
        .map(Duration::from_secs)
        .ok_or_else(|| format!("`{text}` is too long"))
}
