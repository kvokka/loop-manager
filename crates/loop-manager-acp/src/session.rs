//! What the session updates of an ACP v2 agent mean to the core (docs/design/adapters.md, "ACP v2 harnesses"): the
//! state of the one session of an instance, turned into normalized events.

use crate::profile::Profile;
use agent_client_protocol::schema::MaybeUndefined;
use agent_client_protocol::v2::schema::{
    self, BackgroundWork, IdleStateUpdate, SessionUpdate, StateUpdate, StopReason as AcpStop, ToolCallStatus,
};
use loop_manager_contract::driver::{Failure, FailureKind, Normalized, PromptKey, SessionState, Source, StopReason};
use serde_json::Value;
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

/// The session of an instance as the updates tell it.
#[derive(Default)]
pub(crate) struct Turn {
    /// The agent's id of the session loop-manager opened: only its state drives the attempt.
    pub(crate) session: Option<String>,
    /// The prompt sent and not yet acknowledged.
    pending: Option<PromptKey>,
    in_turn: bool,
    /// The tool calls of the session not yet finished.
    tools: HashSet<String>,
    /// When activity was last told, and whether a tool ran then.
    last_activity: Option<(Instant, bool)>,
    /// The terminals and tool calls of the session that advertise `capabilities.cancel` now.
    cancellable: HashSet<Work>,
    /// The background work of the session's last `idle`.
    background: Vec<Work>,
    /// What the errors and notices of the agent mean.
    profile: &'static Profile,
}

/// A piece of a session's work the client can cancel by its id: `terminal/cancel` or `tool_call/cancel`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Work {
    Terminal(String),
    ToolCall(String),
}

/// Whether an update says its subject can be cancelled: `None` when it says nothing about it.
fn cancel<T>(capabilities: &MaybeUndefined<T>, cancel: impl Fn(&T) -> bool) -> Option<bool> {
    match capabilities {
        MaybeUndefined::Undefined => None,
        MaybeUndefined::Null => Some(false),
        MaybeUndefined::Value(c) => Some(cancel(c)),
    }
}

impl Turn {
    /// The session of an instance of the agent of `profile`, before it is opened.
    pub(crate) fn new(profile: &'static Profile) -> Self {
        Self {
            profile,
            ..Self::default()
        }
    }

    /// A prompt goes out with `key`.
    pub(crate) fn sent(&mut self, key: PromptKey) {
        self.pending = Some(key);
    }

    /// The agent took the prompt with `key` (its answer, or the first sign of the turn it started), once.
    pub(crate) fn acked(&mut self, key: PromptKey) -> Vec<Normalized> {
        match self.pending == Some(key) {
            true => self
                .pending
                .take()
                .map(|key| Normalized::PromptAcked { key })
                .into_iter()
                .collect(),
            false => Vec::new(),
        }
    }

    pub(crate) fn update(&mut self, session: &str, update: &SessionUpdate, now: Instant) -> Vec<Normalized> {
        if self.session.as_deref() != Some(session) {
            // A child session (a subagent): activity of the attempt, never its state.
            return self.activity(now);
        }
        match update {
            SessionUpdate::StateUpdate(StateUpdate::Running(running)) => {
                let background = running.background.as_ref().is_some_and(|b| !b.is_empty());
                let mut out = Vec::new();
                if !self.in_turn {
                    self.in_turn = true;
                    out.extend(self.pending.take().map(|key| Normalized::PromptAcked { key }));
                    out.push(Normalized::TurnStarted);
                }
                out.push(Normalized::State {
                    state: SessionState::Running,
                    background,
                });
                out
            }
            SessionUpdate::StateUpdate(StateUpdate::Idle(idle)) => self.idle(idle),
            SessionUpdate::ToolCallUpdate(call) => {
                let id = call.tool_call_id.to_string();
                let finished = matches!(
                    call.status,
                    MaybeUndefined::Value(
                        ToolCallStatus::Completed | ToolCallStatus::Failed | ToolCallStatus::Cancelled
                    )
                );
                match call.status.clone().take() {
                    Some(ToolCallStatus::Pending | ToolCallStatus::InProgress) => drop(self.tools.insert(id.clone())),
                    Some(_) => drop(self.tools.remove(&id)),
                    None => {}
                }
                let can = match finished {
                    true => Some(false),
                    false => cancel(&call.capabilities, |c| c.cancel.is_some()),
                };
                self.can_cancel(Work::ToolCall(id), can);
                self.activity(now)
            }
            SessionUpdate::Notice(notice) => {
                let mut out = self.activity(now);
                out.extend((self.profile.notice)(notice).map(Normalized::Failure));
                out
            }
            SessionUpdate::TerminalUpdate(terminal) => {
                let can = match terminal.exit_status {
                    MaybeUndefined::Value(_) => Some(false),
                    _ => cancel(&terminal.capabilities, |c| c.cancel.is_some()),
                };
                self.can_cancel(Work::Terminal(terminal.terminal_id.to_string()), can);
                self.activity(now)
            }
            _ => self.activity(now),
        }
    }

    fn can_cancel(&mut self, work: Work, can: Option<bool>) {
        match can {
            Some(true) => drop(self.cancellable.insert(work)),
            Some(false) => drop(self.cancellable.remove(&work)),
            None => {}
        }
    }

    /// The background work of the last `idle` that can be cancelled now: what `stop_background` cancels; a subagent
    /// or other work is left to end by itself.
    pub(crate) fn to_cancel(&self) -> Vec<Work> {
        (self.background.iter())
            .filter(|w| self.cancellable.contains(*w))
            .cloned()
            .collect()
    }

    /// At most one `activity` a second (docs/design/contracts.md 4.3).
    fn activity(&mut self, now: Instant) -> Vec<Normalized> {
        // A tool starting or ending is told at once; the same news is told once a second.
        let tool_running = !self.tools.is_empty();
        if self
            .last_activity
            .is_some_and(|(t, running)| running == tool_running && now.duration_since(t) < Duration::from_secs(1))
        {
            return Vec::new();
        }
        self.last_activity = Some((now, tool_running));
        vec![Normalized::Activity { tool_running }]
    }

    fn idle(&mut self, idle: &IdleStateUpdate) -> Vec<Normalized> {
        let mut out = Vec::new();
        if let Some(usage) = &idle.usage {
            out.push(Normalized::Usage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                rate_limit: None,
            });
        }
        if std::mem::take(&mut self.in_turn) {
            self.tools.clear();
            let stop_reason = match stop(idle.stop_reason.as_ref(), self.profile.classify) {
                Ok(reason) => reason,
                Err(failure) => {
                    out.push(Normalized::Failure(failure));
                    StopReason::Failed
                }
            };
            out.push(Normalized::TurnEnded { stop_reason });
        }
        let listed = idle.background.iter().flatten();
        self.background = (listed.clone())
            .filter_map(|w| match w {
                BackgroundWork::Terminal(t) => Some(Work::Terminal(t.terminal_id.to_string())),
                BackgroundWork::ToolCall(c) => Some(Work::ToolCall(c.tool_call_id.to_string())),
                _ => None,
            })
            .collect();
        let background = listed.count() > 0;
        out.push(Normalized::State {
            state: SessionState::Idle,
            background,
        });
        out
    }

    /// The process is gone: an open turn fails as `harness_crash` whatever the exit code; then the instance is gone.
    pub(crate) fn exited(&mut self, code: Option<i32>, signal: Option<i32>, stderr: &str) -> Vec<Normalized> {
        let mut out = Vec::new();
        if std::mem::take(&mut self.in_turn) || self.pending.take().is_some() {
            let mut failure = Failure::new(FailureKind::HarnessCrash, Source::Adapter);
            failure.raw = format!("the agent exited in a turn (code {code:?}, signal {signal:?}): {stderr}");
            out.push(Normalized::Failure(failure));
            out.push(Normalized::TurnEnded {
                stop_reason: StopReason::Failed,
            });
        }
        out.push(Normalized::Exited { code, signal });
        out
    }
}

/// The stop reason of a turn, or the failure it stands for; `classify` tells what an `error` means.
fn stop(reason: Option<&AcpStop>, classify: fn(i32, &str, Option<&Value>) -> Failure) -> Result<StopReason, Failure> {
    let failure = |kind, raw: String| {
        let mut failure = Failure::new(kind, Source::Harness);
        failure.raw = raw;
        failure
    };
    match reason {
        None | Some(AcpStop::EndTurn | AcpStop::Other(_)) => Ok(StopReason::Completed),
        Some(AcpStop::Cancelled) => Ok(StopReason::Cancelled),
        Some(AcpStop::MaxTokens) => Err(failure(FailureKind::ContextOverflow, "max_tokens".into())),
        Some(AcpStop::MaxTurnRequests) => Err(failure(FailureKind::RunawayTurn, "max_turn_requests".into())),
        Some(AcpStop::Refusal) => Err(failure(FailureKind::Unknown, "refusal".into())),
        Some(AcpStop::Error(error)) => Err(match &error.error {
            Some(error) => classify(error.code.into(), &error.message, error.data.as_ref()),
            None => failure(FailureKind::Unknown, "error".into()),
        }),
        Some(_) => Ok(StopReason::Completed),
    }
}

/// A failure of the agent by its JSON-RPC error code (decision 118): what no code tells apart is `unknown`, with the
/// agent's words kept.
pub(crate) fn classify(code: i32, message: &str, data: Option<&Value>) -> Failure {
    let kind = match code {
        // `auth_required` of the ACP schema.
        -32000 => FailureKind::AuthRequired,
        -32602 => FailureKind::ConfigError,
        _ => FailureKind::Unknown,
    };
    let mut failure = Failure::new(kind, Source::Harness);
    failure.raw = match data {
        Some(data) => format!("{code}: {message}: {data}"),
        None => format!("{code}: {message}"),
    };
    failure.text = (kind == FailureKind::Unknown).then(|| message.to_owned());
    failure
}

/// The `permission_denied` event of a request loop-manager declined: no person answers a run's permission request
/// (decision 116).
pub(crate) fn denied(request: &schema::RequestPermissionRequest) -> Normalized {
    let subject = serde_json::to_value(&request.subject).unwrap_or_default();
    // A tool call's details are under `toolCall`.
    let call = match subject["toolCall"].is_null() {
        true => &subject,
        false => &subject["toolCall"],
    };
    Normalized::PermissionDenied {
        tool: request.title.clone(),
        tool_use_id: call["toolCallId"].as_str().map(str::to_owned),
        input: call["rawInput"].clone(),
        reason: Some("loop-manager answers no permission request of a run".into()),
    }
}

#[cfg(test)]
mod tests;
