//! The command groups of the CLI: `run`, `gate`, `trigger` and `instance`, each with the API calls behind it.

use super::{encode, follow, print, run_path};
use crate::client::{Client, ClientError};
use clap::Subcommand;
use serde_json::{Map, Value, json};
use std::process::ExitCode;

#[derive(Subcommand)]
pub enum RunCommand {
    /// Start a run of a formula; prints its id. Inside an agent session (`LOOP_MANAGER_RUN`, `LOOP_MANAGER_ATTEMPT_TOKEN`) the run is
    /// a child of the session's run: same correlation, one level deeper. Options go after the positionals.
    #[command(allow_missing_positional = true)]
    Start {
        project: String,
        /// Optional: without it the run works in the project root, or on the rig of the bead its task names (its
        /// id prefix).
        rig: Option<String>,
        formula: String,
        /// An input as `name=value`: text, which an integer or a boolean input takes as its number or `true`/`false`
        /// and a task input as the task or the id of the bead it takes up; a JSON object or array (a task input's
        /// `{"next_ready": true}`) is taken as JSON.
        #[arg(long = "input", value_name = "NAME=VALUE", value_parser = name_value)]
        inputs: Vec<String>,
        /// A terminal run whose last session this run continues.
        #[arg(long)]
        continue_from: Option<String>,
        /// The idempotency key: a repeat with the same key starts nothing new.
        #[arg(long)]
        key: Option<String>,
    },
    /// List runs, newest first.
    List {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        state: Option<String>,
        /// Only the runs whose `parent_run` is this one.
        #[arg(long)]
        parent: Option<String>,
        /// Only the runs waiting in an `await` step for an outcome.
        #[arg(long)]
        awaiting: bool,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Show a run.
    Show { id: String },
    /// Follow a run live until it ends: its events from `--from` (default: the oldest held). Exits 0 however the run
    /// ends; `loop-manager run wait` gives the outcome.
    Watch {
        id: String,
        /// Print the records of the run's history from record `--from` (default 1), then follow them to the run's end.
        #[arg(long)]
        history: bool,
        #[arg(long)]
        from: Option<i64>,
    },
    /// Wait until every named run ends, then print each: state, class and reason of a failure, the finisher's summary
    /// and data. Exits 0 when all passed, 1 when not, 124 when `--timeout` ran out first.
    Wait {
        #[arg(required = true)]
        ids: Vec<String>,
        /// How long to wait at most: `90s`, `30m`, `2h`.
        #[arg(long, value_parser = loop_manager_core::config::duration)]
        timeout: Option<std::time::Duration>,
    },
    /// Cancel a run, and the live runs its sessions started.
    Cancel {
        id: String,
        #[arg(long)]
        reason: Option<String>,
        /// The idempotency key: a repeat with the same key changes nothing.
        #[arg(long = "key")]
        idempotency: Option<String>,
    },
    /// Give the outcome of the `await` step a run waits on. Refused when the run does not wait on that step.
    Resolve {
        id: String,
        step: String,
        result: super::Verdict,
        #[arg(long, default_value = "")]
        summary: String,
        /// JSON, or `@<file>` holding it.
        #[arg(long)]
        data: Option<String>,
        /// The idempotency key: a repeat with the same key changes nothing. A bridge that may send an outcome twice
        /// sets it: without one, a late repeat could resolve the step once more after a round back to it.
        #[arg(long = "key")]
        idempotency: Option<String>,
    },
    /// Take a run over: its orchestration stands until `run continue` while its work in flight goes on. Prints
    /// `accepted`, or `stale_attempt` when the run is at another attempt than `--attempt`, or `run_terminal`.
    Takeover(Control),
    /// Hand a run taken over back: its budgets start afresh and what it stood still for goes on, in the same attempt.
    /// Prints `accepted`, `stale_attempt`, `run_terminal` or `not_manual`; while an instruction waits to go in,
    /// `unsent_input` with it unless `--unsent` says what to do, and `returning` with `--unsent send`.
    Continue {
        #[command(flatten)]
        control: Control,
        /// What becomes of the instructions the harness has not taken in: `send` hands the run back once they are
        /// in, `withdraw` takes back those loop-manager still holds and hands it back now.
        #[arg(long, value_enum)]
        unsent: Option<UnsentChoice>,
    },
    /// Send an instruction, a prompt of the operator, to the run's session; a run under loop-manager is taken over
    /// first. Prints `accepted` with the prompt's key and `queued` (held for the end of the turn) or `intent` (sent
    /// now), or `stale_attempt`, `run_terminal` or `no_live_session` (no AI session lives: nothing was sent).
    Say {
        #[command(flatten)]
        control: Control,
        text: String,
    },
    /// Stop the turn in flight of the run's session, whose process lives on, or the command of its exec step; a run
    /// under loop-manager is taken over first. Prints `accepted`, or `stale_attempt`, `run_terminal` or
    /// `nothing_to_stop` (no turn is in flight and no command runs).
    Stop(Control),
    /// Answer a permission request the run's session waits on (`permissions` in `run show`). Prints `accepted`, or
    /// `stale_attempt`, `run_terminal`, `already_answered`, `unknown_request` or `unknown_option`.
    Permit {
        #[command(flatten)]
        control: Control,
        /// The request's `id`.
        #[arg(long)]
        request: String,
        /// The `id` of the option to select.
        #[arg(long, required_unless_present = "decline", conflicts_with = "decline")]
        option: Option<String>,
        /// Decline the request.
        #[arg(long)]
        decline: bool,
    },
}

/// What a Continue does with the instructions the harness has not taken in.
#[derive(Clone, Copy, clap::ValueEnum, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnsentChoice {
    Send,
    Withdraw,
}

/// A takeover, a Continue, an instruction, a Stop or an answer to a permission request of a run.
#[derive(clap::Args)]
pub struct Control {
    id: String,
    /// The attempt of the run as last seen (`attempt` in `run show`).
    #[arg(long)]
    attempt: u32,
    /// The idempotency key: a repeat with the same key gets the first answer.
    #[arg(long = "key")]
    idempotency: Option<String>,
}

#[derive(Subcommand)]
pub enum GateCommand {
    /// List the closed gates.
    List,
    /// Close a gate as the operator: only the operator opens it again.
    Close {
        /// `orchestrator`, `project:<p>`, `rig:<p>/<r>`, `agent:<a>`, `account:<h>/<id>`, ...
        gate: String,
        /// UTC, `YYYY-MM-DDTHH:MM:SSZ`; absent: until opened by hand.
        #[arg(long)]
        until: Option<String>,
        #[arg(long, default_value = "")]
        reason: String,
        /// The idempotency key: a repeat with the same key changes nothing.
        #[arg(long = "key")]
        idempotency: Option<String>,
    },
    /// Open a gate.
    Open {
        gate: String,
        /// The idempotency key: a repeat with the same key changes nothing.
        #[arg(long = "key")]
        idempotency: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum TriggerCommand {
    /// List the triggers on their rigs with their next firing.
    List,
    /// Fire a trigger on a rig as its schedule would: prints the run it started, or why it skipped.
    Fire(OnRig),
    /// Switch a trigger on a rig back on.
    Enable(OnRig),
    /// Switch a trigger on a rig off: neither its schedule nor its events fire it, `fire` still does.
    Disable(OnRig),
}

/// A trigger on a rig.
#[derive(clap::Args)]
pub struct OnRig {
    /// `<pack>.<name>`, or the name of a project trigger.
    trigger: String,
    #[arg(long)]
    project: String,
    #[arg(long)]
    rig: String,
    /// The idempotency key: a repeat with the same key changes nothing.
    #[arg(long = "key")]
    idempotency: Option<String>,
}

/// Runs a `run` command; exits 3 when a run start found nothing ready to take.
pub(super) async fn run(client: &Client, json: bool, command: RunCommand) -> Result<ExitCode, ClientError> {
    match command {
        RunCommand::Start {
            project,
            rig,
            formula,
            inputs,
            continue_from,
            key,
        } => {
            let inputs: Map<String, Value> = inputs.iter().map(|i| input(i)).collect();
            // The session the command runs in, as the finisher finds it.
            let parent = match (
                std::env::var("LOOP_MANAGER_RUN"),
                std::env::var("LOOP_MANAGER_ATTEMPT_TOKEN"),
            ) {
                (Ok(run), Ok(token)) => json!({ "run": run, "token": token }),
                _ => Value::Null,
            };
            let body = json!({ "project": project, "rig": rig, "formula": formula, "inputs": inputs,
                "continue_from": continue_from, "parent": parent });
            let reply = client.request("POST", "/v1/runs", Some(&body), key.as_deref()).await?;
            match reply["run"].as_str() {
                _ if json => print(json, &reply),
                Some(run) => println!("{run}"),
                None => eprintln!("loop-manager: nothing ready: no ready bead to take, no run started"),
            }
            for (bead, why) in reply["passed_over"].as_object().into_iter().flatten().filter(|_| !json) {
                eprintln!("loop-manager: passed over {bead}: {}", why.as_str().unwrap_or_default());
            }
            if reply["nothing_ready"] == true {
                return Ok(ExitCode::from(3));
            }
        }
        RunCommand::List {
            project,
            state,
            parent,
            awaiting,
            limit,
        } => {
            let query: Vec<String> = [
                ("project", project),
                ("state", state),
                ("parent", parent),
                ("awaiting", awaiting.then(|| "true".to_owned())),
                ("limit", limit.map(|l| l.to_string())),
            ]
            .into_iter()
            .filter_map(|(k, v)| Some(format!("{k}={}", encode(&v?))))
            .collect();
            let runs = client
                .request("GET", &format!("/v1/runs?{}", query.join("&")), None, None)
                .await?;
            print(json, &runs);
        }
        RunCommand::Show { id } => print(json, &client.request("GET", &run_path(&id, ""), None, None).await?),
        RunCommand::Watch { id, from, history } => {
            let query = from.map(|f| format!("?from={f}")).unwrap_or_default();
            let stream = if history { "/history" } else { "/live" };
            follow(client.stream(&run_path(&id, &format!("{stream}{query}"))).await?, json).await?;
        }
        RunCommand::Wait { ids, timeout } => return super::wait::wait(client, json, ids, timeout).await,
        RunCommand::Cancel {
            id,
            reason,
            idempotency,
        } => {
            let body = json!({ "reason": reason.unwrap_or_default() });
            let path = run_path(&id, "/cancel");
            let reply = client.request("POST", &path, Some(&body), idempotency.as_deref());
            print(json, &reply.await?);
        }
        RunCommand::Resolve {
            id,
            step,
            result,
            summary,
            data,
            idempotency,
        } => return resolve(client, json, (&id, step, idempotency), (result, summary, data)).await,
        RunCommand::Takeover(control) => return self::control(client, json, "/takeover", control, json!({})).await,
        RunCommand::Continue { control, unsent } => {
            return self::control(client, json, "/continue", control, json!({ "unsent": unsent })).await;
        }
        RunCommand::Say { control, text } => {
            return self::control(client, json, "/instruct", control, json!({ "text": text })).await;
        }
        RunCommand::Stop(control) => return self::control(client, json, "/stop", control, json!({})).await,
        RunCommand::Permit {
            control,
            request,
            option,
            decline: _,
        } => {
            let body = json!({ "request": request, "option": option });
            return self::control(client, json, "/permission", control, body).await;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `run resolve`: the outcome of the await step `step` of run `id`.
async fn resolve(
    client: &Client,
    json: bool,
    (id, step, idempotency): (&str, String, Option<String>),
    (result, summary, data): (super::Verdict, String, Option<String>),
) -> Result<ExitCode, ClientError> {
    let data = match super::finish::data_arg(data) {
        Ok(data) => data,
        Err(e) => return Ok(super::fail(json, "usage", &e)),
    };
    let body = json!({ "step": step, "outcome": super::finish::outcome(result, summary, data) });
    let path = run_path(id, "/resolve");
    let reply = client.request("POST", &path, Some(&body), idempotency.as_deref());
    print(json, &reply.await?);
    Ok(ExitCode::SUCCESS)
}

/// One command of the operator, `body` with the attempt added: exits 0 when it was accepted or the run is returning,
/// 1 when it was refused.
async fn control(
    client: &Client,
    json: bool,
    action: &str,
    control: Control,
    mut body: Value,
) -> Result<ExitCode, ClientError> {
    body["attempt"] = json!(control.attempt);
    let path = run_path(&control.id, action);
    let reply = client.request("POST", &path, Some(&body), control.idempotency.as_deref());
    let reply = reply.await?;
    print(json, &reply);
    Ok(if matches!(reply["answer"].as_str(), Some("accepted" | "returning")) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

pub(super) async fn gate(client: &Client, json: bool, command: GateCommand) -> Result<(), ClientError> {
    let (path, body, idempotency) = match command {
        GateCommand::List => {
            let gates = client.request("GET", "/v1/gates", None, None).await?;
            print(json, &gates);
            return Ok(());
        }
        GateCommand::Close {
            gate,
            until,
            reason,
            idempotency,
        } => (
            "/v1/gates",
            json!({ "key": gate, "until": until, "reason": reason }),
            idempotency,
        ),
        GateCommand::Open { gate, idempotency } => ("/v1/gates/open", json!({ "key": gate }), idempotency),
    };
    print(
        json,
        &client
            .request("POST", path, Some(&body), idempotency.as_deref())
            .await?,
    );
    Ok(())
}

pub(super) async fn trigger(client: &Client, json: bool, command: TriggerCommand) -> Result<(), ClientError> {
    let (path, on) = match command {
        TriggerCommand::List => {
            print(json, &client.request("GET", "/v1/triggers", None, None).await?);
            return Ok(());
        }
        TriggerCommand::Fire(on) => ("/v1/triggers/fire", on),
        TriggerCommand::Enable(on) => ("/v1/triggers/enable", on),
        TriggerCommand::Disable(on) => ("/v1/triggers/disable", on),
    };
    let body = json!({ "project": on.project, "rig": on.rig, "trigger": on.trigger });
    let reply = client
        .request("POST", path, Some(&body), on.idempotency.as_deref())
        .await?;
    print(json, &reply);
    Ok(())
}

#[derive(Subcommand)]
pub enum InstanceCommand {
    /// List the harness instances.
    List,
    /// Stop an idle instance.
    Stop {
        id: String,
        /// The idempotency key: a repeat with the same key changes nothing.
        #[arg(long = "key")]
        idempotency: Option<String>,
    },
}

pub(super) async fn instance(client: &Client, json: bool, command: InstanceCommand) -> Result<(), ClientError> {
    let reply = match command {
        InstanceCommand::List => client.request("GET", "/v1/instances", None, None).await?,
        InstanceCommand::Stop { id, idempotency } => {
            let path = format!("/v1/instances/{}/stop", encode(&id));
            client.request("POST", &path, None, idempotency.as_deref()).await?
        }
    };
    print(json, &reply);
    Ok(())
}

/// An `--input` with its `=`: a bare name is a mistake, not an empty value.
fn name_value(text: &str) -> Result<String, String> {
    match text.split_once('=') {
        Some((name, _)) if !name.is_empty() => Ok(text.to_owned()),
        _ => Err(format!("`{text}` is not NAME=VALUE")),
    }
}

/// `name=value`, the value as text unless it is a JSON object or array: the core reads text by the input's type, so
/// `version=1.10` stays `1.10` and `name=null` is the text `null`.
fn input(text: &str) -> (String, Value) {
    let (name, value) = text.split_once('=').unwrap_or((text, ""));
    let parsed = serde_json::from_str(value)
        .ok()
        .filter(|v: &Value| v.is_object() || v.is_array());
    let value = parsed.unwrap_or_else(|| Value::String(value.to_owned()));
    (name.to_owned(), value)
}
