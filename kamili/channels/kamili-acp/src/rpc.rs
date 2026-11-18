//! JSON-RPC 2.0 over newline-delimited JSON (the ACP stdio transport).
//!
//! Everything sent goes through one queue to one writer task, so messages are
//! never interleaved and keep their order. Requests we send (permission
//! requests) are matched to responses by id.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

/// JSON-RPC error codes used here.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
/// ACP: the agent needs the user to authenticate first.
pub const AUTH_REQUIRED: i64 = -32000;

#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

type Pending = Mutex<HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>>;

/// The sending half of the connection. Cheap to share behind an `Arc`.
pub struct Out {
    tx: mpsc::UnboundedSender<Value>,
    next_id: AtomicI64,
    pending: Pending,
}

impl Out {
    /// A sender, and the receiver the writer task drains.
    pub fn new() -> (Self, mpsc::UnboundedReceiver<Value>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                tx,
                next_id: AtomicI64::new(1),
                pending: Mutex::new(HashMap::new()),
            },
            rx,
        )
    }

    fn send(&self, message: Value) {
        // Fails only once the writer is gone, i.e. the client disconnected.
        let _ = self.tx.send(message);
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    pub fn respond(&self, id: Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    pub fn respond_error(&self, id: Value, error: RpcError) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": error.code, "message": error.message},
        }));
    }

    /// Sends a request to the client and waits for its response.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        rx.await
            .unwrap_or_else(|_| Err(RpcError::new(INTERNAL_ERROR, "client disconnected")))
    }

    /// Routes a response from the client to the request waiting for it.
    pub fn resolve(&self, message: &Value) {
        let Some(id) = message["id"].as_i64() else {
            return;
        };
        let Some(tx) = self.pending.lock().expect("pending lock").remove(&id) else {
            return;
        };
        let outcome = match message.get("error") {
            Some(e) => Err(RpcError::new(
                e["code"].as_i64().unwrap_or(INTERNAL_ERROR),
                e["message"].as_str().unwrap_or("error").to_string(),
            )),
            None => Ok(message["result"].clone()),
        };
        let _ = tx.send(outcome);
    }

    /// Fails every request still waiting (the client went away).
    pub fn fail_pending(&self) {
        self.pending.lock().expect("pending lock").clear();
    }
}

/// Writes queued messages, one JSON object per line, until the queue closes.
pub async fn write_loop<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut rx: mpsc::UnboundedReceiver<Value>,
) -> std::io::Result<()> {
    while let Some(message) = rx.recv().await {
        // serde_json never emits raw newlines inside a compact document.
        let mut line = serde_json::to_vec(&message).map_err(std::io::Error::other)?;
        line.push(b'\n');
        writer.write_all(&line).await?;
        writer.flush().await?;
    }
    Ok(())
}
