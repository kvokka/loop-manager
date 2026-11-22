//! The profiles of the ACP driver (docs/design/adapters.md, "Profiles"): what the driver does for one ACP agent it
//! knows by name at the points of a session, beyond what it does for any agent. A harness of no profile gets the
//! generic one: what the config names, and a refusal of what the driver has no mechanism for.

mod claude;

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
}

/// The ACP agents the driver knows by name.
pub(crate) const PROFILES: &[Profile] = &[Profile {
    name: "claude",
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
}];

/// The profile of a harness known by its config alone.
pub(crate) const GENERIC: Profile = Profile {
    name: "",
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
};

impl Profile {
    /// The profile of harness `name`: its own, else the generic one.
    pub(crate) fn of(name: &str) -> &'static Self {
        PROFILES.iter().find(|p| p.name == name).unwrap_or(&GENERIC)
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
