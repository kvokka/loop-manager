//! The ACP driver with the `codex` profile driving the real `codex-acp` mise installs against the fake OpenAI
//! Responses API (docs/harness-updates.md): no model, no account.

mod contract;
mod models;
mod session;
mod subagents;

use loop_manager_acp::Acp;
use loop_manager_contract::{
    config::{EffectiveConfig, Layer, Sourced},
    driver::{
        Driver, DriverEvent, Failure, InstanceHandle, Normalized, PromptKey, SessionPlan, SessionRef, StopReason,
    },
};
use loop_manager_testkit::openai::{Provider, Reply};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;

/// The path of `tool` in the version `mise.toml` selects.
fn mise_which(tool: &str) -> PathBuf {
    let out = std::process::Command::new("mise")
        .args(["which", tool])
        .output()
        .expect("mise runs");
    assert!(
        out.status.success(),
        "mise which {tool}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
}

/// The driver of harness `codex` on the adapter mise installs, its state (and the protocol trace) under `dir`. The
/// adapter is a `#!/usr/bin/env node` script: the `node` of `mise.toml` goes first in its `PATH`.
fn driver(dir: &Path) -> Acp {
    let node = mise_which("node");
    let path = format!(
        "{}:{}",
        node.parent().expect("a directory of node").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut env = toml::Table::new();
    env.insert("PATH".into(), path.into());
    let mut options = toml::Table::new();
    let adapter = mise_which("codex-acp");
    options.insert("command".into(), adapter.to_string_lossy().into_owned().into());
    options.insert("env".into(), env.into());
    options.insert("trace".into(), true.into());
    Acp::new("codex", &options, &dir.join("state"), None, None).expect("the options of harness `codex`")
}

/// The config of an isolated session on the fake: the model provider `fake` of the native block without retries, and
/// a key that is no credential, so that nothing of the host's codex home is linked or read. The host forbids user
/// namespaces, so the codex sandbox cannot run here (docs/design/experiments.md): full access.
fn config(provider: &Provider) -> EffectiveConfig {
    let native = json!({ "model_provider": "fake", "sandbox_mode": "danger-full-access", "approval_policy": "never",
        "model_providers": { "fake": { "name": "fake", "base_url": provider.base_url(), "wire_api": "responses",
        "request_max_retries": 0, "stream_max_retries": 0 } } });
    EffectiveConfig {
        services: Default::default(),
        harness: "codex".into(),
        harness_from: Layer::Agent,
        scope: Default::default(),
        description: None,
        subagents: None,
        targets: Default::default(),
        model_given: None,
        model: Some(sourced("gpt-fake".to_owned())),
        effort: None,
        system_prompt: None,
        inherit_host: Some(sourced(false)),
        env: [("OPENAI_API_KEY".to_owned(), sourced("fake".to_owned()))].into(),
        mcp: Default::default(),
        hooks: Default::default(),
        skills: Default::default(),
        addons: Vec::new(),
        native: Some(sourced(native)),
    }
}

/// The native block of `config`, to change.
fn native(config: &mut EffectiveConfig) -> &mut serde_json::Value {
    &mut config.native.as_mut().expect("a native block").value
}

/// codex's own retries of a model request and of a stream, `n` each.
fn retries(config: &mut EffectiveConfig, n: u32) {
    let fake = &mut native(config)["model_providers"]["fake"];
    fake["request_max_retries"] = json!(n);
    fake["stream_max_retries"] = json!(n);
}

fn sourced<T>(value: T) -> Sourced<T> {
    Sourced {
        value,
        from: Layer::Agent,
    }
}

/// The plan of a new session of run `r1`, attempt 1, token `t`, in `rig`.
fn new_session(rig: PathBuf) -> SessionPlan {
    SessionPlan::New {
        workdir: rig,
        run: "r1".into(),
        attempt: 1,
        token: "t".into(),
        actor: None,
        bead: None,
    }
}

/// The plan of attempt `attempt` of run `r1` resuming `session` in `rig`, its token `token`.
fn resumed(session: SessionRef, rig: PathBuf, attempt: u32, token: &str) -> SessionPlan {
    SessionPlan::Resume {
        session,
        workdir: rig,
        run: "r1".into(),
        attempt,
        token: token.into(),
        actor: None,
        bead: None,
    }
}

/// One session of the adapter on the fake: its process, its session, and the events seen so far.
struct Live {
    provider: Arc<Provider>,
    driver: Acp,
    instance: InstanceHandle,
    session: SessionRef,
    events: mpsc::UnboundedReceiver<DriverEvent>,
    seen: Vec<Normalized>,
    rig: PathBuf,
}

impl Live {
    /// A new session on a fake of `plan`, `tweak` changing its config, and the prompt "Do the task." sent.
    async fn start(plan: Vec<Reply>, dir: &Path, tweak: impl FnOnce(&mut EffectiveConfig)) -> Self {
        let provider = Arc::new(Provider::start(plan).await);
        let rig = dir.join("rig");
        let live = Self::open(provider, dir, new_session(rig), tweak).await.unwrap();
        live.prompt(PromptKey::new("r1", 1, 1), "Do the task.").await;
        live
    }

    /// The process of `plan` started as instance `i<attempt>` in `dir` and its session opened, or resumed; `tweak`
    /// changes the config first. A session that does not open stops its process.
    async fn open(
        provider: Arc<Provider>,
        dir: &Path,
        plan: SessionPlan,
        tweak: impl FnOnce(&mut EffectiveConfig),
    ) -> Result<Self, Failure> {
        let driver = driver(dir);
        let rig = dir.join("rig");
        std::fs::create_dir_all(&rig).unwrap();
        let mut config = config(&provider);
        tweak(&mut config);
        let split = driver.split(&config, &plan).unwrap();
        let (sink, events) = mpsc::unbounded_channel();
        let (SessionPlan::New { attempt, .. } | SessionPlan::Resume { attempt, .. }) = &plan;
        let instance = driver
            .start_instance(&format!("i{attempt}"), &split.process, sink)
            .await
            .unwrap();
        let session = match &plan {
            SessionPlan::New { .. } => driver.start_session(&instance, &split.session).await,
            SessionPlan::Resume { session, .. } => {
                (driver.resume_session(&instance, session, &split.session).await).map(|()| session.clone())
            }
        };
        let session = match session {
            Ok(session) => session,
            Err(refused) => {
                driver.stop_instance(&instance, Duration::from_secs(4)).await;
                return Err(refused);
            }
        };
        Ok(Self {
            provider,
            driver,
            instance,
            session,
            events,
            seen: Vec::new(),
            rig,
        })
    }

    async fn prompt(&self, key: PromptKey, text: &str) {
        self.driver
            .send_prompt(&self.instance, &self.session, text, key)
            .await
            .unwrap();
    }

    /// Events until `until` holds for one, or `wait` passes; the matching event, if any. A permission request is
    /// declined, as the core's policy does under the LM.
    async fn until(&mut self, wait: Duration, until: impl Fn(&Normalized) -> bool) -> Option<Normalized> {
        let deadline = tokio::time::Instant::now() + wait;
        while let Ok(Some(event)) = tokio::time::timeout_at(deadline, self.events.recv()).await {
            if let Normalized::PermissionRequested { id, .. } = &event.event {
                self.driver.answer_permission(&self.instance, id, None).await;
            }
            self.seen.push(event.event.clone());
            if until(&event.event) {
                return Some(event.event);
            }
        }
        None
    }

    async fn ended(&mut self) -> StopReason {
        match self
            .until(Duration::from_secs(60), |e| matches!(e, Normalized::TurnEnded { .. }))
            .await
        {
            Some(Normalized::TurnEnded { stop_reason }) => stop_reason,
            _ => panic!("no turn end in {:?}", self.seen),
        }
    }

    fn failures(&self) -> Vec<&Failure> {
        self.seen
            .iter()
            .filter_map(|e| match e {
                Normalized::Failure(f) => Some(f),
                _ => None,
            })
            .collect()
    }

    /// The one failure of a failed turn.
    async fn failed(&mut self) -> Failure {
        assert_eq!(self.ended().await, StopReason::Failed, "{:?}", self.seen);
        let failures = self.failures();
        assert_eq!(failures.len(), 1, "{:?}", self.seen);
        failures[0].clone()
    }

    /// The pid a command of the plan wrote to `file` in the rig, once it is there (up to 30 s).
    async fn pid_in(&self, file: &str) -> i32 {
        for _ in 0..300 {
            let text = std::fs::read_to_string(self.rig.join(file)).unwrap_or_default();
            if let Ok(pid) = text.trim().parse() {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("no pid in {file}: {:?}", self.seen);
    }

    /// The bodies of the model requests the fake took, once there are `n` of them (up to 30 s).
    async fn requests(&self, n: usize) -> Vec<serde_json::Value> {
        for _ in 0..300 {
            let requests = self.provider.requests();
            if requests.len() >= n {
                return requests.into_iter().map(|r| r.body).collect();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("fewer than {n} model requests: {:?}", self.provider.requests());
    }

    async fn stop(mut self) {
        self.driver.stop_instance(&self.instance, Duration::from_secs(4)).await;
        self.until(Duration::from_secs(10), |e| matches!(e, Normalized::Exited { .. }))
            .await;
    }
}

/// Whether process `pid` is gone, within 5 s.
async fn gone(pid: i32) -> bool {
    for _ in 0..50 {
        if loop_manager_contract::orphans::start_time(pid).is_none() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}
