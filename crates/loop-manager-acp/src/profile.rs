//! The profiles of the ACP driver (docs/design/adapters.md, "Profiles"): what the driver does for one ACP agent it
//! knows by name at the points of a session, beyond what it does for any agent. A harness of no profile gets the
//! generic one: what the config names, and a refusal of what the driver has no mechanism for.

pub(crate) mod claude;
pub(crate) mod codex;

use crate::part;
use agent_client_protocol::v2::schema::Notice;
use loop_manager_contract::{
    config::EffectiveConfig,
    driver::{Failure, SessionPlan},
};
use serde_json::{Map, Value};
use std::path::Path;

/// The command, the literal arguments and the environment of `[harness.<name>]`.
pub(crate) type Setup<'a> = (&'a Path, &'a [String], &'a Map<String, Value>);

/// A process environment: the variables in the order they are set, a later one over an earlier one.
pub(crate) type Env = Vec<(String, String)>;

/// The points of a session where a profile acts; each is a function the driver calls there.
pub(crate) struct Profile {
    /// The harness the profile serves.
    pub(crate) name: &'static str,
    /// The oldest version of the agent the profile supports, `(major, minor, patch)`.
    pub(crate) minimum: (u32, u32, u32),
    /// Whether the agent may insert a prompt into a turn in flight (`Driver::prompts_in_turn`).
    pub(crate) prompts_in_turn: bool,
    /// Whether the agent keeps the user messages of a session across its processes (`Driver::retains_prompts`).
    pub(crate) retains_prompts: bool,
    /// The process part of a session (`part::process` and what the profile delivers through the process); `state` is
    /// the daemon's state directory.
    pub(crate) process: fn(&str, Setup, &EffectiveConfig, &SessionPlan, &Path) -> Result<Value, Failure>,
    /// Adds to the session part (`part::session`) what the profile sends with `session/new` and `session/resume`.
    pub(crate) session: fn(&EffectiveConfig, &SessionPlan, &Path, &mut Value) -> Result<(), Failure>,
    /// The whole environment of the process of the process part, for instance `id`.
    pub(crate) env: fn(&Value, &str, Option<&Path>) -> Result<Env, Failure>,
    /// The variables of that environment that carry a credential: secrets to the protocol trace.
    pub(crate) credentials: &'static [&'static str],
    /// Before the spawn: the files the process of the process part reads.
    pub(crate) start: fn(&Value) -> std::io::Result<()>,
    /// After that process exited: what of those files goes with it.
    pub(crate) exited: fn(&Value),
    /// The values of the session part beyond its MCP servers that are secrets to the protocol trace.
    pub(crate) secrets: fn(&Value) -> Vec<String>,
    /// The failure of an error the agent answered or ended a turn with: its JSON-RPC code, message and data.
    pub(crate) classify: fn(i32, &str, Option<&Value>) -> Failure,
    /// The failure a `notice` of the session stands for, if any.
    pub(crate) notice: fn(&Notice) -> Option<Failure>,
    /// The account the gates use, for the harness and its process part.
    pub(crate) account: fn(&str, &Value) -> String,
    /// What an agent's config sets that the harness never takes, by key; said when the config loads.
    pub(crate) unsupported: fn(&str, &EffectiveConfig) -> Result<(), String>,
}

/// The ACP agents the driver knows by name.
pub(crate) const PROFILES: &[Profile] = &[
    Profile {
        name: "claude",
        minimum: claude::MINIMUM,
        // Claude Code queues a prompt sent during a turn and may take it in before the turn ends.
        prompts_in_turn: true,
        // Claude Code's transcript holds every prompt the adapter inserted, and a replay gives it back.
        retains_prompts: true,
        process: claude::process,
        session: claude::session,
        env: claude::env,
        credentials: &claude::CREDENTIALS,
        start: claude::start,
        exited: claude::exited,
        secrets: claude::secrets,
        classify: claude::classify,
        notice: claude::notice,
        account: claude::account,
        unsupported: claude::unsupported,
    },
    Profile {
        name: "codex",
        minimum: codex::MINIMUM,
        // codex-acp holds a prompt sent during a turn until the turn is idle.
        prompts_in_turn: false,
        // The thread's rollout holds every prompt the adapter inserted, and a replay gives it back.
        retains_prompts: true,
        process: codex::process,
        session: codex::session,
        env: codex::env,
        credentials: &codex::CREDENTIALS,
        start: codex::start,
        // The homes keep the session's rollout; the role files are shared by content.
        exited: |_| {},
        secrets: codex::secrets,
        classify: codex::classify,
        notice: |_| None,
        account: codex::account,
        unsupported: codex::unsupported,
    },
];

/// The profile of a harness known by its config alone.
pub(crate) const GENERIC: Profile = Profile {
    name: "",
    minimum: (0, 0, 0),
    prompts_in_turn: false,
    retains_prompts: false,
    process: |harness, setup, config, plan, _| part::process(harness, setup, config, plan),
    session: |_, _, _, _| Ok(()),
    env: part::env,
    credentials: &[],
    start: |_| Ok(()),
    exited: |_| {},
    secrets: |_| Vec::new(),
    classify: crate::session::classify,
    notice: |_| None,
    account: part::account,
    unsupported: part::unsupported,
};

impl Profile {
    /// The own profile of harness `name`, if it has one.
    pub(crate) fn own(name: &str) -> Option<&'static Self> {
        PROFILES.iter().find(|p| p.name == name)
    }

    /// The profile of harness `name`: its own, else the generic one.
    pub(crate) fn of(name: &str) -> &'static Self {
        Self::own(name).unwrap_or(&GENERIC)
    }
}

/// The generic profile, where nothing names another.
impl Default for &'static Profile {
    fn default() -> Self {
        &GENERIC
    }
}

#[cfg(test)]
mod tests;
