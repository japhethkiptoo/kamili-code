//! ACP channel for Kamili: lets editors such as Zed drive Kamili as an agent
//! over the Agent Client Protocol (JSON-RPC 2.0, newline-delimited, on stdio).
//!
//! Protocol version 1. Implemented directly on `serde_json` rather than an ACP
//! SDK: Kamili uses a small part of the protocol, and this keeps the channel
//! independent of SDK churn.
//!
//! | ACP | Kamili |
//! |---|---|
//! | `initialize` | protocol v1; text and embedded-context prompts; no session loading |
//! | `session/new` | an engine for `cwd` (its `.kamili.toml`), via [`EngineFactory`] |
//! | `session/prompt` | `/spec`, `/build`, `/resume`, `/runs`, `/graph`, `/skills`, `/help`; plain text answers an open Analyst question |
//! | `session/cancel` | builds stop at a safe point (resumable); spec sessions pause |
//! | `session/update` | messages, thoughts, a build plan, tool calls (see `channel.rs`) |
//! | `session/request_permission` | every approval gate, with file diffs |
//!
//! Nothing but protocol messages is written to stdout.
#![forbid(unsafe_code)]

mod channel;
mod rpc;
mod session;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use kamili::{Engine, Error};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite};
use tokio::sync::RwLock;

use rpc::{Out, RpcError};
use session::{Session, prompt_text};

/// The protocol version this agent speaks.
pub const PROTOCOL_VERSION: u64 = 1;

/// Builds an engine for a project. The composition root (the CLI) supplies
/// this, since it owns configuration loading and credentials.
#[async_trait]
pub trait EngineFactory: Send + Sync {
    async fn engine(&self, project_root: &Path) -> Result<Engine, Error>;
}

struct Server {
    out: Arc<Out>,
    factory: Arc<dyn EngineFactory>,
    sessions: RwLock<HashMap<String, Arc<Session>>>,
    next_session: AtomicU64,
}

/// Serves ACP on stdin/stdout until the editor disconnects.
pub async fn serve(factory: Arc<dyn EngineFactory>) -> Result<(), Error> {
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    serve_io(factory, stdin, tokio::io::stdout()).await
}

/// Serves ACP over any reader/writer pair (used by tests).
pub async fn serve_io<R, W>(
    factory: Arc<dyn EngineFactory>,
    reader: R,
    writer: W,
) -> Result<(), Error>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out, rx) = Out::new();
    let out = Arc::new(out);
    let writer_task = tokio::spawn(rpc::write_loop(writer, rx));
    let server = Arc::new(Server {
        out: out.clone(),
        factory,
        sessions: RwLock::new(HashMap::new()),
        next_session: AtomicU64::new(1),
    });

    let mut lines = reader.lines();
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| Error::Workspace(format!("reading stdin: {e}")))?
    {
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                out.respond_error(Value::Null, RpcError::new(rpc::PARSE_ERROR, e.to_string()));
                continue;
            }
        };
        match (
            message.get("method").and_then(Value::as_str),
            message.get("id"),
        ) {
            // A request: handled in its own task, so long prompts don't block
            // cancellation or permission responses.
            (Some(method), Some(id)) => {
                let (server, method, id) = (server.clone(), method.to_string(), id.clone());
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                tokio::spawn(async move {
                    match server.handle(&method, params).await {
                        Ok(result) => {
                            let new_session = result["sessionId"].as_str().map(str::to_string);
                            server.out.respond(id, result);
                            // Only once the client knows the session.
                            if method == "session/new"
                                && let Some(sid) = new_session
                                && let Some(s) = server.sessions.read().await.get(&sid)
                            {
                                s.announce_commands();
                            }
                        }
                        Err(e) => server.out.respond_error(id, e),
                    }
                });
            }
            (Some(method), None) => {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                server.notification(method, &params).await;
            }
            (None, Some(_)) => out.resolve(&message),
            (None, None) => out.respond_error(
                Value::Null,
                RpcError::new(rpc::INVALID_REQUEST, "not a JSON-RPC message"),
            ),
        }
    }

    // The editor closed the connection: stop everything.
    for session in server.sessions.read().await.values() {
        session.cancel();
    }
    out.fail_pending();
    drop(server);
    drop(out);
    let _ = writer_task.await;
    Ok(())
}

fn invalid(message: impl Into<String>) -> RpcError {
    RpcError::new(rpc::INVALID_PARAMS, message)
}

impl Server {
    async fn handle(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        match method {
            "initialize" => Ok(json!({
                // We speak only v1: answer with it whatever the client asked.
                "protocolVersion": PROTOCOL_VERSION,
                "agentCapabilities": {
                    "loadSession": false,
                    "promptCapabilities": {"image": false, "audio": false, "embeddedContext": true},
                    "mcpCapabilities": {"http": false, "sse": false},
                },
                "agentInfo": {
                    "name": "kamili",
                    "title": "Kamili",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                // Credentials come from `kamili login` before the editor starts us.
                "authMethods": [],
            })),
            "authenticate" => Ok(json!({})),
            "session/new" => self.new_session(&params).await,
            "session/prompt" => {
                let session = self.session(&params).await?;
                let text = prompt_text(&params["prompt"]);
                let stop = session.prompt(text).await;
                Ok(json!({"stopReason": stop}))
            }
            "session/load" => Err(RpcError::new(
                rpc::METHOD_NOT_FOUND,
                "Kamili does not load sessions; use /spec <feature> --resume or /resume",
            )),
            other => Err(RpcError::new(
                rpc::METHOD_NOT_FOUND,
                format!("unknown method `{other}`"),
            )),
        }
    }

    async fn notification(&self, method: &str, params: &Value) {
        if method == "session/cancel"
            && let Ok(session) = self.session(params).await
        {
            session.cancel();
        }
        // Other notifications are ignored, as the protocol allows.
    }

    async fn session(&self, params: &Value) -> Result<Arc<Session>, RpcError> {
        let id = params["sessionId"]
            .as_str()
            .ok_or_else(|| invalid("missing sessionId"))?;
        self.sessions
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| invalid(format!("unknown session `{id}`")))
    }

    async fn new_session(&self, params: &Value) -> Result<Value, RpcError> {
        let cwd = PathBuf::from(
            params["cwd"]
                .as_str()
                .ok_or_else(|| invalid("missing cwd"))?,
        );
        if !cwd.is_absolute() {
            return Err(invalid("cwd must be an absolute path"));
        }
        let root = cwd
            .canonicalize()
            .map_err(|e| invalid(format!("{}: {e}", cwd.display())))?;
        let engine = self.factory.engine(&root).await.map_err(|e| match e {
            Error::Llm(m) => {
                RpcError::new(rpc::AUTH_REQUIRED, format!("{m}; then start a new thread"))
            }
            other => RpcError::new(rpc::INTERNAL_ERROR, other.to_string()),
        })?;
        let id = format!(
            "kamili_{}_{}",
            std::process::id(),
            self.next_session.fetch_add(1, Ordering::Relaxed)
        );
        let session = Arc::new(Session::new(id.clone(), root, engine, self.out.clone()));
        self.sessions.write().await.insert(id.clone(), session);
        Ok(json!({"sessionId": id}))
    }
}
