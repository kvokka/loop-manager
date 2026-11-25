//! The driver interface between the core and a harness adapter (docs/design/contracts.md 4). The adapter
//! translates and executes; it never retries a turn, parks or decides an outcome (docs/architecture.md,
//! docs/design/adapters.md).

use crate::{BoxFuture, config::EffectiveConfig, usd::Usd};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{path::PathBuf, time::Duration};
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

mod adapter;
pub mod chat;
pub mod child;
mod failure;
pub use adapter::{Adapter, Check, DoctorAgents, Setup, host_mcp_check, login_check};
pub use failure::{Failure, FailureKind, Scope, Source};

/// The part of the effective config that needs its own process; opaque to the core.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessPart(pub Value);

/// The part of the effective config that a session on a shared process carries; opaque to the core.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionPart(pub Value);

/// SHA-256 of the harness name and the canonical JSON of a [`ProcessPart`], hex: sessions whose process parts are
/// equal share an instance (contracts 8).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Fingerprint(pub String);

impl Fingerprint {
    pub fn of(harness: &str, process: &ProcessPart) -> Self {
        // `serde_json` maps keep their keys sorted, so the text is canonical.
        let digest = Sha256::new()
            .chain_update(harness.as_bytes())
            .chain_update([0])
            .chain_update(process.0.to_string().as_bytes())
            .finalize();
        Self(digest.iter().map(|b| format!("{b:02x}")).collect())
    }
}

/// The effective config split by the adapter into what needs its own process and what a session carries.
#[derive(Clone, Debug, PartialEq)]
pub struct Split {
    pub process: ProcessPart,
    pub session: SessionPart,
    pub fingerprint: Fingerprint,
}

impl Split {
    pub fn new(harness: &str, process: ProcessPart, session: SessionPart) -> Self {
        let fingerprint = Fingerprint::of(harness, &process);
        Self {
            process,
            session,
            fingerprint,
        }
    }
}

/// What the session of an attempt is; `run` and `attempt` name files in the state directory. The attempt environment:
/// `run`, `token` (`LOOP_MANAGER_RUN`, `LOOP_MANAGER_ATTEMPT_TOKEN`, contracts 6.3), `actor` (the project's tracker
/// variables, 5.1) and `issue` (`LOOP_MANAGER_ISSUE`, the run's issue when it has one).
#[derive(Clone, Debug, PartialEq)]
pub enum SessionPlan {
    New {
        workdir: PathBuf,
        run: String,
        attempt: u32,
        token: String,
        actor: Option<Actor>,
        issue: Option<String>,
    },
    Resume {
        session: SessionRef,
        workdir: PathBuf,
        run: String,
        attempt: u32,
        token: String,
        actor: Option<Actor>,
        issue: Option<String>,
    },
}

/// The variables a session gives the project's tracker (contracts 5.1): the names are the tracker's, the values the
/// core's but the harness and the model, which the adapter fills in.
#[derive(Clone, Debug, PartialEq)]
pub struct Actor {
    /// The actor variable and `loop-manager:<run>:<attempt>`.
    pub actor: (String, String),
    /// The variable of the agent of the step, and that agent.
    pub agent: Option<(String, String)>,
    /// The variable of the harness name.
    pub harness: Option<String>,
    /// The variable of the model; the session gets it only when its config sets a model.
    pub model: Option<String>,
}

impl Actor {
    /// The variables of a session of `harness` started with `config`.
    pub fn env(&self, harness: &str, config: &EffectiveConfig) -> Vec<(String, String)> {
        let model = config.model.as_ref().map(|m| m.value.clone());
        let harness = self.harness.clone().map(|name| (name, harness.to_owned()));
        [
            Some(self.actor.clone()),
            self.agent.clone(),
            harness,
            self.model.clone().zip(model),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

/// Everything the adapter needs to resume a session; the core stores it and hands it back.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionRef(pub Value);

/// A running harness process: `pid` and its kernel `start_time` identify the process (docs/design/event-model.md).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InstanceHandle {
    pub id: String,
    pub pid: i32,
    pub start_time: u64,
    pub pgid: i32,
}

/// The idempotency key of one prompt (contracts 6.4), carried as the harness's own message id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PromptKey(pub Uuid);

impl PromptKey {
    /// UUID v5 of `"<run>/<attempt>/<prompt>"` in the URL namespace under the `loop-manager:prompt:` scheme.
    pub fn new(run: &str, attempt: u32, prompt: u32) -> Self {
        let name = format!("loop-manager:prompt:{run}/{attempt}/{prompt}");
        Self(Uuid::new_v5(&Uuid::NAMESPACE_URL, name.as_bytes()))
    }
}

/// Whether a session's transcript exists and holds the prompt with a given key. Whether the prompt was answered
/// does not matter: a resumed session never has a turn in flight, so a delivered prompt is reconciled either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transcript {
    Missing,
    Present { delivered: bool },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Health {
    Ok,
    Degraded { text: String },
    Dead,
}

/// The daemon's network settings a harness process inheriting the host gets too (docs/design/config.md): proxies and
/// the certificates to trust.
pub const HOST_NETWORK_ENV: [&str; 11] = [
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub sessions_per_instance: usize,
    /// How long an instance with no sessions lives on before it is stopped (contracts 8).
    pub instance_idle: Duration,
    /// The oldest supported version of the harness binary, `(major, minor, patch)`, inclusive; no upper bound.
    pub min_version: (u32, u32, u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    Completed,
    Cancelled,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    Running,
    Idle,
}

/// Token quantities of requests, disjoint: no token is counted in two fields, so each takes its own rate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    /// Ordinary input: neither read from nor written to a cache.
    pub input: u64,
    pub cache_read: u64,
    /// Written to a cache that lives five minutes (the provider's short duration).
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    pub output: u64,
}

/// Whose requests a usage event counts (contracts 4.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Owner {
    /// The session's own agent: the agent of its step.
    Session,
    /// A subagent of the harness, by the name the harness gives its type.
    Subagent(String),
    /// Several agents of the session together, none named: the unattributed part.
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateLimit {
    pub utilization: f64,
    pub resets_at_ms: Option<i64>,
}

/// A normalized harness event (contracts 4.3): an input message of the core, not a journal event.
#[derive(Clone, Debug, PartialEq)]
pub enum Normalized {
    PromptAcked {
        key: PromptKey,
    },
    TurnStarted,
    TurnEnded {
        stop_reason: StopReason,
    },
    /// "Idle without background" is `Idle` with `background == false`; the adapter applies the harness rule.
    State {
        state: SessionState,
        background: bool,
    },
    /// The limit of the session's account, as the harness last told it.
    RateLimit(RateLimit),
    /// The tokens of one request a later `UsageTotal` of the same session and model includes: the core charges them
    /// at the table price until that total settles them (contracts 4.3).
    UsageEstimate {
        owner: Owner,
        provider: String,
        model: String,
        tokens: Tokens,
    },
    /// The running total of one usage counter of the session, as the harness keeps it (contracts 4.3): the core
    /// charges the increment over the total it recorded last for the same session and counter.
    UsageTotal {
        /// Which counter of the session: one per model, or one per thread of a harness with threads.
        counter: String,
        /// Whose the part of the increment is that no estimate of the session explains.
        owner: Owner,
        /// The provider the requests went to, as the harness tells it.
        provider: String,
        model: String,
        /// The service tier the requests went on, as the harness tells it: the price of that tier first.
        service_tier: Option<String>,
        tokens: Tokens,
        /// The harness's own running cost of the counter, an explicit zero included.
        cost: Option<Usd>,
    },
    Retrying {
        attempt: u32,
        delay: Duration,
        text: String,
    },
    /// Watchdog only; the adapter sends at most one per second per session.
    Activity {
        tool_running: bool,
    },
    /// The harness or the adapter denied the session a tool (nothing waits on it): one event per denial, with what
    /// the harness tells of it.
    PermissionDenied {
        tool: String,
        tool_use_id: Option<String>,
        input: Value,
        reason: Option<String>,
    },
    Failure(Failure),
    /// The instance process is gone.
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct DriverEvent {
    pub instance: String,
    /// `None` for instance-wide events.
    pub session: Option<SessionRef>,
    /// Monotonic per instance, from 1; 0 for an event the core makes itself (a dead instance's `Exited`).
    pub seq: u64,
    pub event: Normalized,
}

pub type EventSink = mpsc::UnboundedSender<DriverEvent>;

/// One harness, behind one object-safe trait. The core owns one driver per harness.
pub trait Driver: Send + Sync {
    /// The harness name the config uses for it, `[harness.<name>]`.
    fn harness(&self) -> &'static str;
    fn capabilities(&self, process: &ProcessPart) -> Capabilities;
    /// The account id the gates use (docs/design/contracts.md 7): a stable hash of the credential source, never the
    /// credential.
    fn account(&self, process: &ProcessPart) -> Option<String>;
    /// A config the harness cannot express is a `config_error` failure of the attempt.
    fn split(&self, config: &EffectiveConfig, plan: &SessionPlan) -> Result<Split, Failure>;

    /// `id` is the registry's: the handle carries it, and the process environment holds it as `LOOP_MANAGER_INSTANCE`.
    fn start_instance<'a>(
        &'a self,
        id: &'a str,
        process: &'a ProcessPart,
        sink: EventSink,
    ) -> BoxFuture<'a, Result<InstanceHandle, Failure>>;
    fn health<'a>(&'a self, instance: &'a InstanceHandle) -> BoxFuture<'a, Health>;
    fn stop_instance<'a>(&'a self, instance: &'a InstanceHandle, grace: Duration) -> BoxFuture<'a, ()>;

    fn start_session<'a>(
        &'a self,
        instance: &'a InstanceHandle,
        session: &'a SessionPart,
    ) -> BoxFuture<'a, Result<SessionRef, Failure>>;
    fn resume_session<'a>(
        &'a self,
        instance: &'a InstanceHandle,
        session: &'a SessionRef,
        part: &'a SessionPart,
    ) -> BoxFuture<'a, Result<(), Failure>>;
    fn send_prompt<'a>(
        &'a self,
        instance: &'a InstanceHandle,
        session: &'a SessionRef,
        text: &'a str,
        key: PromptKey,
    ) -> BoxFuture<'a, Result<(), Failure>>;
    fn cancel_turn<'a>(
        &'a self,
        instance: &'a InstanceHandle,
        session: &'a SessionRef,
    ) -> BoxFuture<'a, Result<(), Failure>>;
    /// Stops the commands the session left running after its turn (docs/design/contracts.md 6); the session stays and
    /// takes a next prompt. Work the agent may still wait for (a background subagent) is left alone. The harness
    /// reports the end of what was stopped, and the session comes to `State{Idle, background: false}` by the usual
    /// rule. Best effort.
    fn stop_background<'a>(&'a self, instance: &'a InstanceHandle, session: &'a SessionRef) -> BoxFuture<'a, ()>;
    /// Detaches the session from its instance; the transcript stays, so the session can be resumed later.
    fn close_session<'a>(&'a self, instance: &'a InstanceHandle, session: &'a SessionRef) -> BoxFuture<'a, ()>;

    fn transcript<'a>(
        &'a self,
        instance: &'a InstanceHandle,
        session: &'a SessionRef,
        key: PromptKey,
    ) -> BoxFuture<'a, Result<Transcript, Failure>>;
    /// Locates a session's readable conversation. May read the filesystem, so call on a blocking thread.
    fn chat_log(&self, session: &SessionRef) -> std::io::Result<chat::ChatLog>;
    /// The raw harness messages of a session (docs/design/event-model.md direct relay); a slow reader loses messages.
    fn live(&self, session: &SessionRef) -> Option<broadcast::Receiver<Value>>;
}

/// The last 4 KiB of `stderr`, read to its end (a descendant may hold it open past the exit: the caller bounds it).
pub async fn stderr_tail(mut stderr: tokio::process::ChildStderr) -> String {
    let (mut kept, mut buf) = (Vec::new(), [0; 4096]);
    while let Ok(n @ 1..) = tokio::io::AsyncReadExt::read(&mut stderr, &mut buf).await {
        kept.extend_from_slice(&buf[..n]);
        kept.drain(..kept.len().saturating_sub(buf.len()));
    }
    String::from_utf8_lossy(&kept).into_owned()
}
