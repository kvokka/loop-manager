//! The messages of the run machine and the values they carry.

use super::{Exit, GateSpec};
use crate::{
    driver::{DriverEvent, Failure, InstanceHandle, PromptKey, Replay, SessionRef},
    tracker::{Bead, Change, TrackerError},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, time::Duration};

/// A message to the run machine.
pub enum Msg {
    /// Queues a run; the reply is `{"run": "<id>"}`.
    Create(Create),
    Cancel {
        run: String,
        reason: String,
    },
    /// The outcome of the await step `step` of `run`, given from outside; the reply is `null`.
    Resolve {
        run: String,
        step: String,
        outcome: Outcome,
    },
    /// Fires `trigger` (`<pack>.<name>`, or the name of a project trigger) on a rig by hand, as its schedule would;
    /// the reply is `{"run": "<id>"}` or `{"skipped": <reason>, "detail": <text>}`.
    Fire {
        project: String,
        rig: String,
        trigger: String,
    },
    /// The operator switches `trigger` on a rig off or back on: off, neither its schedule nor its events fire it, a
    /// hand firing still does; the switch outlives restarts and reloads.
    SwitchTrigger {
        project: String,
        rig: String,
        trigger: String,
        enabled: bool,
    },
    /// The operator closes a gate (docs/design/contracts.md 7): automation never removes this closure (invariant 18).
    CloseGate(GateSpec),
    /// The operator opens a gate: every closure of `key` goes.
    OpenGate {
        key: String,
    },
    /// A decision on the deciding event `seq` the run waits for (contracts 3.2).
    Decide {
        run: String,
        seq: i64,
        decision: Decision,
    },
    /// The registry's instance `id` runs.
    InstanceReady {
        id: String,
        harness: String,
        handle: InstanceHandle,
    },
    /// The registry's instance `id` could not start.
    InstanceFailed {
        id: String,
        failure: Failure,
    },
    /// The health check found the registry's instance `id` degraded (`Some` reason) or well again (`None`).
    InstanceHealth {
        id: String,
        degraded: Option<String>,
    },
    /// The periodic resync found the process of a registry instance gone (docs/design/event-model.md).
    InstanceGone {
        handle: InstanceHandle,
    },
    /// The stop of the registry's instance `id` is done; `reason` is the one of the stop (contracts 1.3).
    InstanceStopped {
        id: String,
        reason: String,
    },
    /// The service `id` accepts connections.
    ServiceReady {
        id: String,
    },
    /// The process of the service `id` ended, or never accepted connections; the end of its stderr. `taken`: it did
    /// not start, someone else listens on its port.
    ServiceFailed {
        id: String,
        stderr: String,
        taken: bool,
    },
    /// The stop of the service `id` is done.
    ServiceStopped {
        id: String,
        reason: String,
    },
    /// The instance of an attempt runs.
    Instance {
        run: String,
        attempt: u32,
        handle: InstanceHandle,
    },
    /// The executor started or resumed the session of an attempt.
    Session {
        run: String,
        attempt: u32,
        session: SessionRef,
    },
    /// The launch of an attempt resumed its session to check the delivery of the last prompt of the attempt before:
    /// `replay` is what the harness gave of the session. The reply is `{"send": {"key", "text"}}`, the prompt the
    /// launch sends first, or `{"send": null}`: nothing goes, the delivery is disputed (contracts 6.4).
    Resumed {
        run: String,
        attempt: u32,
        replay: Replay,
    },
    /// `send_prompt` of the attempt returned for prompt `key`.
    PromptSent {
        run: String,
        attempt: u32,
        key: PromptKey,
    },
    /// A driver call of an attempt failed.
    AttemptFailed {
        run: String,
        attempt: u32,
        failure: Failure,
    },
    Driver(DriverEvent),
    /// The command of an exec step, or a check script, ended; `stdout` and `stderr` are their last 4 KiB.
    ExecDone {
        run: String,
        attempt: u32,
        /// The command was the check script of the run's step (docs/design/config.md).
        check: bool,
        exit: Exit,
        stdout: String,
        stderr: String,
    },
    /// The check script of an attempt was not started: its `env` names a variable the daemon does not have; `text`
    /// names the key and the variable.
    CheckRefused {
        run: String,
        attempt: u32,
        text: String,
    },
    /// A hook on `event` failed: the core reports it as `addon.hook_failed` with `data` (contracts 1.3).
    HookFailed {
        event: Value,
        data: Value,
    },
    /// The result of the tracker call of the effect `payload`.
    Tracked {
        payload: Value,
        result: Result<Option<Bead>, TrackerError>,
    },
    /// The bead of the read effect `payload` with its comments and parents, for the prompt that shows them.
    Lineage {
        payload: Value,
        result: Result<crate::tracker::Lineage, TrackerError>,
    },
    /// The writes to the tracker of `project` after its cursor, its ready set and, for the root bead of each run in
    /// a drain step when the read was asked, the ready beads under it at any depth.
    Ready {
        project: String,
        beads: Vec<Bead>,
        changes: Vec<Change>,
        under: BTreeMap<String, Vec<String>>,
    },
    /// The tracker of the project changed: its ready set is read again.
    TrackerChanged(String),
    /// The config file was read again; the reply is `{config_generation, restarted_instances}`.
    Reload(std::sync::Arc<crate::config::Config>),
    /// The daemon is going to stop (`reason`: `signal`, `drain` or `restart`): no run starts from now on, and
    /// `daemon.stopping` is emitted once; the reply is `{"seq", "reason"}` of the first one.
    Stopping {
        reason: String,
    },
    /// The operator stops the idle instance `id` (`instance.stopped{reason: "operator"}`); refused while it starts
    /// or has sessions.
    StopInstance {
        id: String,
    },
    /// The finisher of an attempt; the reply is `{"answer": "accepted"|"stale_attempt"|"run_terminal"|"invalid"}`
    /// (contracts 6.3).
    Finish {
        run: String,
        token: String,
        outcome: Outcome,
    },
    /// The operator takes `run` over at `attempt`, the attempt the client saw: its orchestration stands until
    /// [`Msg::Continue`]; the reply is `{"answer": "accepted"|"stale_attempt"|"run_terminal"}` (contracts 6.1).
    Takeover {
        run: String,
        attempt: u32,
    },
    /// The operator hands `run` back at `attempt`: fresh budgets, then the deferred event. With the operator's input
    /// the harness has not taken in, `unsent` chooses what becomes of it. The reply is `{"answer":
    /// "accepted"|"returning"|"stale_attempt"|"run_terminal"|"not_manual"}`, or `{"answer": "unsent_input", "unsent":
    /// [{key, text, state}]}` with no choice made (contracts 6.1).
    Continue {
        run: String,
        attempt: u32,
        unsent: Option<Unsent>,
    },
    /// The operator's prompt `text` to the session of `run` at `attempt`; a run under loop-manager is taken over first.
    /// The reply is `{"answer": "accepted", "key", "state": "queued"|"intent"}` or `{"answer":
    /// "stale_attempt"|"run_terminal"|"no_live_session"}`; an empty text or one over 64 KiB is rejected (contracts
    /// 6.1).
    Instruct {
        run: String,
        attempt: u32,
        text: String,
    },
    /// The operator stops the turn of the session of `run` at `attempt`, or the command of its exec step; a run under
    /// loop-manager is taken over first. The reply is `{"answer": "accepted"|"stale_attempt"|"run_terminal"|
    /// "nothing_to_stop"}` (contracts 6.1).
    Stop {
        run: String,
        attempt: u32,
    },
    /// The operator's choice on the disputed delivery of a prompt of `run`, seen at `attempt`. The reply is
    /// `{"answer": "accepted"|"stale_attempt"|"run_terminal"|"not_disputed"}`, or `{"answer": "already_resolved",
    /// "choice"}` with the choice made before and not applied yet (contracts 6.4).
    Delivery {
        run: String,
        attempt: u32,
        choice: Choice,
    },
    /// The operator answers the permission request `request` of the session of `run` at `attempt`: `option` selects
    /// one of its options, `None` declines it. The reply is `{"answer": "accepted"|"stale_attempt"|"run_terminal"|
    /// "already_answered"|"unknown_request"|"unknown_option"}` (contracts 6.1).
    Permission {
        run: String,
        attempt: u32,
        request: String,
        option: Option<String>,
    },
}

/// What a Continue does with the operator's input the harness has not taken in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unsent {
    /// Hands the run back once the harness has taken every input in.
    Send,
    /// Takes back the input loop-manager still holds and hands the run back now.
    Withdraw,
}

/// The operator's choice on a disputed delivery (contracts 6.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Choice {
    /// The harness got the prompt: the reconcile prompt goes.
    Delivered,
    /// The harness did not: the prompt goes again with its key.
    Resend,
}

/// The outcome an agent declares (contracts 1.3 `Outcome`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    pub result: Verdict,
    pub summary: String,
    pub data: Option<Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
}

/// A run to create: by hand (`correlation` and `parent_run` absent, depth 0), by a trigger, or from an agent session
/// (`parent_run` and its attempt `token`: the core takes the correlation and the depth from the parent).
pub struct Create {
    pub project: String,
    /// `None`: the rig of the prefix of the bead id an input names, else the root (docs/design/contracts.md 6).
    pub rig: Option<String>,
    /// A formula reference as a project writes it: `name` or `<pack>.<name>`.
    pub formula: String,
    pub inputs: serde_json::Map<String, Value>,
    pub correlation: Option<String>,
    pub depth: u32,
    pub parent_run: Option<String>,
    /// The attempt token of the session of `parent_run` that starts this run; checked as the finisher's is.
    pub token: Option<String>,
    /// A terminal run whose last session this run continues (docs/design/contracts.md 6); never implicit.
    pub continue_from: Option<String>,
}

/// The decisions of contracts 3.2 the run machine applies; one that comes while the run is manual waits in the state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Decision {
    /// Every hook on the event finished without a decision: the "no addon decision" outcome, at once (3.3).
    Abstain,
    /// On `run.preparing`: the session starts in `workdir`, with `env` changed (`None` removes a variable) and
    /// `prompt_append` after the prompt.
    Proceed {
        workdir: Option<std::path::PathBuf>,
        env: BTreeMap<String, Option<crate::config::Sourced<String>>>,
        prompt_append: Option<String>,
    },
    Deny {
        reason: String,
    },
    /// On `run.attempt_failed` (as `NewSession` and `Fail`): `gate`, if any, is closed in the same transaction.
    Resume {
        after: Duration,
        gate: Option<GateSpec>,
    },
    NewSession {
        after: Duration,
        gate: Option<GateSpec>,
    },
    /// On `run.attempt_failed`: close `gate` and wait in `parked` until the run's keys are open, then start the
    /// attempt `then` (`resume` or `new_session`).
    Park {
        gate: GateSpec,
        then: String,
    },
    Fail {
        reason: String,
        gate: Option<GateSpec>,
    },
    /// On `run.turn_ended_without_outcome`: `message` goes as the next prompt of the attempt.
    Continue {
        message: String,
    },
    Accept,
    Veto {
        reason: String,
    },
    /// On `run.finishing`: `message` goes as the next prompt of the attempt; the declared outcome is cleared.
    Remark {
        message: String,
    },
}
