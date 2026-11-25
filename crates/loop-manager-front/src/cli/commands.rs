//! The command groups of the CLI: `run`, `gate`, `trigger` and `instance`, each with the API calls behind it, and the
//! spend metrics of `spend`.

use super::{encode, follow, print, run_path};
use crate::client::{Client, ClientError};
use clap::Subcommand;
use loop_manager_core::usd::Usd;
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
        /// The run's spend limit in USD (a decimal such as `2.5`), in place of its formula's `budget`.
        #[arg(long, value_name = "USD")]
        budget: Option<String>,
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
    /// Show what a run and its subtree spent, live or ended: the total, a row per agent, or per agent, provider and
    /// model.
    Spend {
        id: String,
        #[arg(long, default_value = "agent", value_parser = ["total", "agent", "model"])]
        detail: String,
        /// Print an aligned table, costs in USD to four decimals, in place of TOON or JSON.
        #[arg(long, conflicts_with = "json")]
        table: bool,
    },
    /// Follow a run live until it ends: its events from `--from` (default: the oldest held) and the raw messages of
    /// its session from now on. Exits 0 however the run ends; `loop-manager run wait` gives the outcome.
    Watch {
        id: String,
        /// Show messages, tool calls and results from the beginning of the current session, then follow live.
        #[arg(long, conflicts_with = "from")]
        chat: bool,
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
            budget,
            key,
        } => {
            let inputs: Map<String, Value> = inputs.iter().map(|i| input(i)).collect();
            let body = json!({ "project": project, "rig": rig, "formula": formula, "inputs": inputs,
                "continue_from": continue_from, "budget_usd": budget, "parent": session() });
            let reply = client.request("POST", "/v1/runs", Some(&body), key.as_deref()).await?;
            match reply["run"].as_str() {
                _ if json => print(json, &reply),
                Some(run) => println!("{run}"),
                None => eprintln!("loop-manager: nothing ready: no ready bead to take, no run started"),
            }
            for (id, why) in reply["passed_over"].as_object().into_iter().flatten().filter(|_| !json) {
                eprintln!("loop-manager: passed over {id}: {}", why.as_str().unwrap_or_default());
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
        RunCommand::Spend { id, detail, table } => spend(client, json, &id, &detail, table).await?,
        RunCommand::Watch { id, chat: true, .. } => {
            return super::chat::follow(client.stream(&run_path(&id, "/live?chat=true")).await?, json).await;
        }
        RunCommand::Watch { id, from, .. } => {
            let query = from.map(|f| format!("?from={f}")).unwrap_or_default();
            follow(client.stream(&run_path(&id, &format!("/live{query}"))).await?, json).await?;
        }
        RunCommand::Wait { ids, timeout } => return super::wait::wait(client, json, ids, timeout).await,
        RunCommand::Cancel {
            id,
            reason,
            idempotency,
        } => {
            let body = json!({ "reason": reason.unwrap_or_default() });
            let path = run_path(&id, "/cancel");
            print(
                json,
                &client
                    .request("POST", &path, Some(&body), idempotency.as_deref())
                    .await?,
            );
        }
        RunCommand::Resolve {
            id,
            step,
            result,
            summary,
            data,
            idempotency,
        } => {
            let data = match super::finish::data_arg(data) {
                Ok(data) => data,
                Err(e) => return Ok(super::fail(json, "usage", &e)),
            };
            let body = json!({ "step": step, "outcome": super::finish::outcome(result, summary, data) });
            let path = run_path(&id, "/resolve");
            let reply = client.request("POST", &path, Some(&body), idempotency.as_deref());
            print(json, &reply.await?);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Prints the spend report of run `id` at `detail`: as a table with `table`.
async fn spend(client: &Client, json: bool, id: &str, detail: &str, table: bool) -> Result<(), ClientError> {
    let path = run_path(id, &format!("/spend?detail={detail}"));
    let report = client.request("GET", &path, None, None).await?;
    match table {
        true => println!("{}", spend_table(&report)),
        false => print(json, &report),
    }
    Ok(())
}

/// A spend report as an aligned table: a row per agent (`unattributed` for the part no agent was named for), with
/// provider and model at `model` detail, then the total and the budget when the run has one.
fn spend_table(report: &Value) -> String {
    let by_model = report["detail"] == "model";
    let labels = if by_model { 3 } else { 1 };
    let mut head = vec!["agent", "provider", "model"];
    head.truncate(labels);
    head.extend(["input", "cache read", "cache write", "output", "cost USD"]);
    let line = |mut label: Vec<String>, usage: &Value| {
        label.resize(labels, String::new());
        let tokens = [
            "input_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
            "output_tokens",
        ];
        label.extend(tokens.map(|t| usage[t].to_string()));
        label.push(usd4(&usage["cost_usd"]));
        label
    };
    let mut lines = vec![head.iter().map(|h| (*h).to_owned()).collect::<Vec<_>>()];
    for row in report["rows"].as_array().into_iter().flatten() {
        let agent = row["agent"].as_str().unwrap_or("unattributed");
        let label = [
            agent,
            row["provider"].as_str().unwrap_or_default(),
            row["model"].as_str().unwrap_or_default(),
        ];
        lines.push(line(label.map(str::to_owned).to_vec(), row));
    }
    lines.push(line(vec!["total".to_owned()], &report["total"]));
    if !report["budget_usd"].is_null() {
        let mut budget = vec![String::new(); head.len() - 1];
        budget[0] = "budget".to_owned();
        budget.push(usd4(&report["budget_usd"]));
        lines.push(budget);
    }
    align(&lines, labels)
}

/// Prints the spend metrics of every run, or of `project`'s, grouped `by`: as a table with `table`.
pub(super) async fn metrics(
    client: &Client,
    json: bool,
    by: &str,
    project: Option<String>,
    table: bool,
) -> Result<(), ClientError> {
    let project = project.map(|p| format!("&project={}", encode(&p))).unwrap_or_default();
    let report = client
        .request("GET", &format!("/v1/spend?by={by}{project}"), None, None)
        .await?;
    match table {
        true => println!("{}", metrics_table(&report)),
        false => print(json, &report),
    }
    Ok(())
}

/// Spend metrics as an aligned table: a row per key, headed by the grouping (`unattributed` for the part no agent
/// was named for), with its runs, attempts, turns, session seconds, token counts and cost.
fn metrics_table(report: &Value) -> String {
    let by = report["by"].as_str().unwrap_or("key");
    let counts = [
        "runs",
        "attempts",
        "turns",
        "session_s",
        "input_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "output_tokens",
    ];
    let head = [
        by,
        "runs",
        "attempts",
        "turns",
        "session s",
        "input",
        "cache read",
        "cache write",
        "output",
        "cost USD",
    ];
    let mut lines = vec![head.map(str::to_owned).to_vec()];
    for row in report["rows"].as_array().into_iter().flatten() {
        let mut line = vec![row["key"].as_str().unwrap_or("unattributed").to_owned()];
        line.extend(counts.map(|c| row[c].to_string()));
        line.push(usd4(&row["cost_usd"]));
        lines.push(line);
    }
    align(&lines, 1)
}

/// `lines` of cells as aligned text: the first `labels` columns to the left, the others to the right.
fn align(lines: &[Vec<String>], labels: usize) -> String {
    let widths: Vec<usize> = (0..lines.first().map_or(0, Vec::len))
        .map(|c| lines.iter().map(|l| l[c].chars().count()).max().unwrap_or(0))
        .collect();
    let aligned = lines.iter().map(|l| {
        let cells = l
            .iter()
            .zip(&widths)
            .enumerate()
            .map(|(c, (cell, w))| match c < labels {
                true => format!("{cell:<w$}"),
                false => format!("{cell:>w$}"),
            });
        cells.collect::<Vec<_>>().join("  ").trim_end().to_owned()
    });
    aligned.collect::<Vec<_>>().join("\n")
}

/// A decimal USD amount rounded to four decimals, half up.
fn usd4(usd: &Value) -> String {
    let pico = usd.as_str().and_then(|u| u.parse::<Usd>().ok()).map_or(0, |u| u.0);
    let ten_thousandths = (pico + 50_000_000) / 100_000_000;
    format!("{}.{:04}", ten_thousandths / 10_000, ten_thousandths % 10_000)
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

#[derive(Subcommand)]
pub enum BeadCommand {
    /// Show a bead of a project from its tracker: description, status, labels and comments.
    Show {
        bead: String,
        #[arg(long)]
        project: String,
    },
    /// List the beads under a root bead at any depth, closed ones too, with their status.
    List {
        root: String,
        #[arg(long)]
        project: String,
    },
    /// File a bead in the project's tracker; prints its id. Inside an agent session (`LOOP_MANAGER_RUN`,
    /// `LOOP_MANAGER_ATTEMPT_TOKEN`) the session's attempt files it.
    Create {
        #[arg(long)]
        project: String,
        #[arg(long)]
        title: String,
        #[arg(long, default_value = "")]
        description: String,
        /// A label of the bead; repeat it for more.
        #[arg(long = "label")]
        labels: Vec<String>,
        /// The bead it lies under.
        #[arg(long)]
        parent: Option<String>,
        /// The rig it is filed for: its id takes the rig's prefix. Without it, a bead of the project root.
        #[arg(long)]
        rig: Option<String>,
        /// The idempotency key: a repeat with the same key files nothing more.
        #[arg(long = "key")]
        idempotency: Option<String>,
    },
    /// Close a bead of a project, whoever holds it; a live run that holds it is cancelled, unless this session is
    /// that run's.
    Close {
        bead: String,
        #[arg(long)]
        project: String,
        #[arg(long)]
        reason: String,
        /// The idempotency key: a repeat with the same key changes nothing.
        #[arg(long = "key")]
        idempotency: Option<String>,
    },
}

pub(super) async fn bead(client: &Client, json: bool, command: BeadCommand) -> Result<(), ClientError> {
    let beads = |project: &str| format!("/v1/projects/{}/beads", encode(project));
    match command {
        BeadCommand::Show { bead, project } => {
            let path = format!("{}/{}", beads(&project), encode(&bead));
            print(json, &client.request("GET", &path, None, None).await?);
        }
        BeadCommand::List { root, project } => {
            let path = format!("{}?under={}", beads(&project), encode(&root));
            print(json, &client.request("GET", &path, None, None).await?);
        }
        BeadCommand::Create {
            project,
            title,
            description,
            labels,
            parent,
            rig,
            idempotency,
        } => {
            let body = json!({ "title": title, "description": description, "labels": labels, "parent": parent,
                "rig": rig, "session": session() });
            let reply = (client.request("POST", &beads(&project), Some(&body), idempotency.as_deref())).await?;
            match reply["id"].as_str() {
                Some(id) if !json => println!("{id}"),
                _ => print(json, &reply),
            }
        }
        BeadCommand::Close {
            bead,
            project,
            reason,
            idempotency,
        } => {
            let path = format!("{}/{}/close", beads(&project), encode(&bead));
            let body = json!({ "reason": reason, "session": session() });
            print(
                json,
                &client
                    .request("POST", &path, Some(&body), idempotency.as_deref())
                    .await?,
            );
        }
    }
    Ok(())
}

/// The agent session the command runs in, as the finisher finds it; `null` outside one.
fn session() -> Value {
    match (
        std::env::var("LOOP_MANAGER_RUN"),
        std::env::var("LOOP_MANAGER_ATTEMPT_TOKEN"),
    ) {
        (Ok(run), Ok(token)) => json!({ "run": run, "token": token }),
        _ => Value::Null,
    }
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

#[cfg(test)]
mod tests;
