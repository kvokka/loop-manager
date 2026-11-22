//! Runs (docs/design/contracts.md 6): the run state machine inside the single writer, the effects that
//! drive a harness through its [`Driver`], and the driver events fed back to the writer as messages.

mod await_step;
mod bead_events;
mod beads;
mod decide;
mod drain;
mod events;
mod exec;
mod exec_step;
mod finish;
mod formula_step;
mod gates;
mod hooks;
mod lineage;
mod machine;
mod manual;
mod msg;
mod permissions;
mod prompts;
mod queue;
mod read;
mod recover;
mod registry;
mod reload;
mod render;
mod repeat;
mod row;
mod service_proc;
mod services;
mod triggers;
mod writers;

use crate::{
    clock::Clock,
    config::Config,
    driver::{Driver, Failure, Normalized},
    history::{History, Record},
    journal::Journal,
    tracker::Tracker,
    writer::{self, CoreHandle, Error, Handler, Tx},
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, PoisonError},
};
use tokio::{sync::mpsc, task::JoinHandle};

pub use crate::config::MAX_ATTEMPTS;
pub use crate::process::Exit;
pub use finish::MAX_REMARKS;
pub use gates::{GateSpec, valid_key};
pub use hooks::{BundledHook, ExecHooks, HookRunner};
pub use msg::{Create, Decision, Msg, Outcome, Unsent, Verdict};
pub use read::{Reader, RunFilter};

type Drivers = Arc<BTreeMap<String, Arc<dyn Driver>>>;

/// Starts the writer with the run machine on `conn` (opened by `store::open`), one driver per harness and the
/// trackers of the projects that have one, by project name. The run records go to `history` after each commit.
#[expect(
    clippy::too_many_arguments,
    reason = "each part of the core comes from the daemon that starts it"
)]
pub fn start(
    conn: Connection,
    clock: Clock,
    config: Arc<Config>,
    drivers: Vec<Arc<dyn Driver>>,
    trackers: BTreeMap<String, Arc<dyn Tracker>>,
    hooks: Arc<dyn HookRunner>,
    journal: Journal,
    history: History,
) -> (CoreHandle<Msg>, JoinHandle<Result<(), Error>>) {
    let drivers: Drivers = Arc::new(drivers.into_iter().map(|d| (d.harness().to_owned(), d)).collect());
    let trackers = Arc::new(trackers);
    let (sink, mut events) = mpsc::unbounded_channel();
    let starts = exec::Starts::default();
    let (exec_drivers, exec_trackers, exec_starts, service_wait) =
        (drivers.clone(), trackers.clone(), starts.clone(), config.service_wait);
    let service_prefix = config.service_prefix.clone();
    let exec_history = history.clone();
    let executor = move |core| exec::RunExecutor {
        drivers: exec_drivers,
        trackers: exec_trackers,
        core,
        sink,
        hooks,
        running: exec_step::Running::default(),
        services: service_proc::Running::default(),
        service_wait,
        service_prefix,
        starts: exec_starts,
        history: exec_history,
        clock,
    };
    let (handle, task) = writer::start(
        conn,
        clock,
        RunHandler {
            config,
            generation: 1,
            stopping: None,
            drivers,
            trackers,
            history,
        },
        executor,
        journal,
    );
    let forward = handle.clone();
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            // A dead process is never started again; its record goes or is stopping, and either way its descendants
            // are swept by an effect.
            if matches!(event.event, Normalized::Exited { .. }) {
                starts
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&event.instance);
            }
            if forward.send(None, Msg::Driver(event)).await.is_err() {
                return;
            }
        }
    });
    (handle, task)
}

struct RunHandler {
    config: Arc<Config>,
    /// The generation of `config` in this daemon: 1 at start, one more on every reload.
    generation: u64,
    /// The `seq` and reason of `daemon.stopping` once the daemon stops: queued and parked runs wait from then on.
    stopping: Option<(i64, String)>,
    drivers: Drivers,
    trackers: beads::Trackers,
    history: History,
}

impl Handler for RunHandler {
    type Msg = Msg;

    fn handle(&mut self, tx: &mut Tx<'_>, msg: Msg) -> Result<Value, Error> {
        match msg {
            Msg::Create(create) => return self.create(tx, create),
            Msg::Finish { run, token, outcome } => return self.finish(tx, &run, &token, &outcome),
            Msg::Takeover { run, attempt } => return self.takeover(tx, &run, attempt),
            Msg::Continue { run, attempt, unsent } => return self.hand_back(tx, &run, attempt, unsent),
            Msg::Instruct { run, attempt, text } => return self.instruct(tx, &run, attempt, &text),
            Msg::Stop { run, attempt } => return self.stop(tx, &run, attempt),
            Msg::Permission {
                run,
                attempt,
                request,
                option,
            } => return self.permit(tx, &run, attempt, &request, option.as_deref()),
            Msg::Fire { project, rig, trigger } => return self.fire_by_hand(tx, &project, &rig, &trigger),
            Msg::SwitchTrigger {
                project,
                rig,
                trigger,
                enabled,
            } => return self.switch(tx, &project, &rig, &trigger, enabled),
            Msg::Cancel { run, reason } => self.cancel(tx, &run, &reason),
            Msg::Resolve { run, step, outcome } => self.resolve(tx, &run, &step, &outcome),
            Msg::CloseGate(gate) => self.close_gate(tx, &gate, "operator"),
            Msg::OpenGate { key } => self.open_gate(tx, &key, None),
            Msg::Decide { run, seq, decision } => self.decide(tx, &run, seq, decision),
            Msg::Instance { run, attempt, handle } => Self::instance(tx, &run, attempt, &handle),
            Msg::InstanceReady { id, harness, handle } => Self::instance_ready(tx, &id, &harness, &handle),
            Msg::InstanceFailed { id, failure } => Self::instance_failed(tx, &id, &failure),
            Msg::InstanceStopped { id, reason } => Self::instance_stopped(tx, &id, &reason),
            Msg::InstanceGone { handle } => self.instance_gone(tx, handle),
            Msg::InstanceHealth { id, degraded } => Self::instance_health(tx, &id, degraded.as_deref()),
            Msg::ServiceReady { id } => Self::service_ready(tx, &id),
            Msg::ServiceFailed { id, stderr, taken } => Self::service_failed(tx, &id, &stderr, taken),
            Msg::ServiceStopped { id, reason } => Self::service_stopped(tx, &id, &reason),
            Msg::Session { run, attempt, session } => self.session(tx, &run, attempt, &session),
            Msg::PromptReplayed {
                run,
                attempt,
                key,
                text,
            } => prompts::intent(tx, &run, attempt, 1, key, &text),
            Msg::PromptSent { run, attempt, key } => prompts::advance(tx, &run, attempt, key, "sent").map(drop),
            Msg::AttemptFailed { run, attempt, failure } => self.launch_failed(tx, &run, attempt, failure),
            Msg::Driver(event) => self.driver_event(tx, event),
            Msg::ExecDone {
                run,
                attempt,
                check,
                exit,
                stdout,
                stderr,
            } => self.exec_done(tx, &run, (attempt, check), exit, &stdout, &stderr),
            Msg::CheckRefused { run, attempt, text } => self.check_refused(tx, &run, attempt, &text),
            Msg::HookFailed { event, data } => Self::hook_failed(tx, &event, data),
            Msg::Tracked { payload, result } => self.tracked(tx, &payload, result),
            Msg::Lineage { payload, result } => self.lineage(tx, &payload, result),
            Msg::Ready {
                project,
                beads,
                changes,
                under,
            } => self
                .ready(tx, &project, &beads, &changes)
                .and_then(|()| self.drained(tx, &project, &under)),
            Msg::TrackerChanged(project) => self.sync_trackers(tx, Some(&project), "changed"),
            Msg::Reload(config) => return self.reload(tx, config),
            Msg::Stopping { reason } => return self.stopping(tx, reason),
            Msg::StopInstance { id } => Self::stop_by_operator(tx, &id),
        }?;
        Ok(Value::Null)
    }

    fn deadline(&mut self, tx: &mut Tx<'_>, id: &str) -> Result<(), Error> {
        match id.rsplit_once(':').unwrap_or((id, "")) {
            (gate, "gate") => self.gate_timer(tx, gate),
            (instance, "idle") => Self::instance_idle(tx, instance),
            (_, "trigger") => self.scheduled(tx, id),
            (_, "resync") => self.resync(tx),
            (run, what) => self.on_deadline(tx, run, what),
        }
    }

    fn emitted(&mut self, tx: &mut Tx<'_>, events: &[Value]) -> Result<(), Error> {
        self.dispatch(tx, events)?;
        self.match_triggers(tx, events)
    }

    fn committed(&mut self, records: Vec<Record>) {
        for record in records {
            self.history.append(record);
        }
    }

    fn started(&mut self, tx: &mut Tx<'_>) -> Result<(), Error> {
        self.recover(tx)?;
        // The resync at the start (docs/design/invariants.md): what the last daemon left behind is checked at once.
        Self::check_instances(tx)?;
        self.sync_trackers(tx, None, "start")?;
        tx.set_deadline(RESYNC, tx.now_ms() + RESYNC_MS)?;
        self.arm_schedules(tx)?;
        // Runs queued while the last daemon drained start now: nothing else would call the queue for them.
        self.schedule(tx)
    }
}

/// The deadline of the periodic resync: the trackers are read again and the instance processes checked against the
/// process table this often whatever their watchers say, so a lost signal costs at most this much
/// (docs/design/event-model.md).
const RESYNC: &str = "daemon:resync";
const RESYNC_MS: i64 = 5 * 60 * 1000;

impl RunHandler {
    fn resync(&mut self, tx: &mut Tx<'_>) -> Result<(), Error> {
        self.sync_trackers(tx, None, "resync")?;
        Self::check_instances(tx)?;
        // The history writer removes the files in its own task: the writer never waits for the disk.
        if let Some(days) = std::num::NonZeroU32::new(self.config.history.keep_days) {
            self.history
                .prune(std::time::Duration::from_secs(u64::from(days.get()) * 86_400));
        }
        tx.set_deadline(RESYNC, tx.now_ms() + RESYNC_MS)
    }
}

fn rejected(reason: impl Into<String>) -> Error {
    Error::Rejected(reason.into())
}

/// The message names a thing there is none of: a 404, where [`rejected`] is a thing that is but refuses.
fn not_found(reason: String) -> Error {
    Error::NotFound(reason)
}

/// A fresh token: 32 random bytes as hex (contracts 6.3).
pub fn new_token() -> Option<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).ok()?;
    Some(hex(&bytes))
}

/// The form the state keeps a token in.
fn token_sha256(token: &str) -> String {
    use sha2::Digest;
    hex(&sha2::Sha256::digest(token.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A failure as events and the attempts table carry it (contracts 1.3).
fn failure_json(f: &Failure) -> Value {
    json!({
        "kind": f.kind, "text": f.text, "scope": f.scope,
        "retry_at": f.retry_at_ms.map(crate::journal::rfc3339_ms),
        "retried_by_harness": f.retried_by_harness, "source": f.source, "raw": f.raw,
        "account": f.account, "provider_model": f.provider_model,
    })
}
