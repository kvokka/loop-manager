//! Read-only ACPv2 over newline-delimited JSON-RPC on stdio.
mod history;

use crate::client::{Client, ClientError};
use serde_json::{Value, json};
use std::collections::HashMap;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::oneshot,
    task::JoinHandle,
};

struct Fault(i32, String);

impl From<ClientError> for Fault {
    fn from(error: ClientError) -> Self {
        let code = if matches!(error, ClientError::Api { status: 404, .. }) {
            -32002
        } else {
            -32603
        };
        Self(code, error.to_string())
    }
}

impl Fault {
    fn params(message: &str) -> Self {
        Self(-32602, message.to_owned())
    }
}

fn reply(id: &Value, result: Result<Value, Fault>) {
    let line = match result {
        Ok(value) => json!({"jsonrpc":"2.0","id":id,"result":value}),
        Err(Fault(code, message)) => json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}),
    };
    println!("{line}");
}

fn update(session: &str, value: Value) {
    println!(
        "{}",
        json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":value}})
    );
}

#[derive(Default)]
struct Agent {
    initialized: bool,
    readiness_probe: bool,
    views: HashMap<String, (oneshot::Sender<()>, JoinHandle<()>)>,
}

pub(super) async fn serve(client: &Client) -> Result<(), ClientError> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut agent = Agent::default();
    while let Some(line) = lines.next_line().await? {
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => {
                reply(&Value::Null, Err(Fault(-32700, "Invalid JSON".into())));
                continue;
            }
        };
        if !request.is_object() || request["jsonrpc"] != "2.0" {
            reply(&Value::Null, Err(Fault(-32600, "Invalid request".into())));
            continue;
        }
        let Some(id) = request.get("id") else { continue };
        if !id.is_string() && !id.is_i64() && !id.is_u64() {
            reply(&Value::Null, Err(Fault(-32600, "Invalid request ID".into())));
            continue;
        }
        match agent.request(client, id, &request).await {
            Ok(None) => (),
            Ok(Some(answer)) => reply(id, Ok(answer)),
            Err(error) => reply(id, Err(error)),
        }
    }
    Ok(())
}

impl Agent {
    async fn request(&mut self, client: &Client, id: &Value, request: &Value) -> Result<Option<Value>, Fault> {
        let params = &request["params"];
        let method = request["method"]
            .as_str()
            .ok_or_else(|| Fault(-32600, "Missing method".into()))?;
        if method == "initialize" {
            if self.initialized {
                return Err(Fault(-32600, "Already initialized".into()));
            }
            let version = params["protocolVersion"]
                .as_u64()
                .ok_or_else(|| Fault::params("Expected protocolVersion"))?;
            if version < 2 {
                return Err(Fault(-32022, "ACP v2 required".into()));
            }
            self.initialized = true;
            self.readiness_probe = params
                .get("info")
                .or(params.get("clientInfo"))
                .is_some_and(|i| i["name"] == "t3-code-provider-test");
            return Ok(Some(json!({"protocolVersion":2, "capabilities":{"session":{}},
                "info":{"name":"loop-manager","version":env!("CARGO_PKG_VERSION")}})));
        }
        if !self.initialized {
            return Err(Fault(-32600, "Initialize first".into()));
        }
        self.views.retain(|_, (_, task)| !task.is_finished());
        match method {
            "session/list" => Ok(Some(history::list(client, params).await?)),
            "session/close" | "session/resume" => {
                let session = params["sessionId"]
                    .as_str()
                    .ok_or_else(|| Fault::params("Expected sessionId"))?;
                let replay = params["replayFrom"] == json!({"type":"start"});
                if method == "session/resume" && !params["replayFrom"].is_null() && !replay {
                    return Err(Fault::params("Unsupported replay cursor"));
                }
                if let Some((stop, task)) = self.views.remove(session) {
                    let _ = stop.send(());
                    let _ = task.await;
                }
                if method == "session/close" {
                    return Ok(Some(json!({})));
                }
                let (stop, stopped) = oneshot::channel();
                let task = tokio::spawn(history::resume(
                    client.clone(),
                    id.clone(),
                    session.to_owned(),
                    replay,
                    stopped,
                ));
                self.views.insert(session.to_owned(), (stop, task));
                Ok(None)
            }
            // T3 checks that a local command is ready with session/new. This acknowledgement starts no run.
            "session/new" if self.readiness_probe => {
                client.request("GET", "/v1/daemon", None, None).await?;
                Ok(Some(
                    json!({"sessionId":"readiness-probe","configOptions":[],"availableCommands":[]}),
                ))
            }
            "session/new" | "session/prompt" => Err(Fault(1, "LM working sessions are read-only".into())),
            _ => Err(Fault(-32601, "Method not found".into())),
        }
    }
}
