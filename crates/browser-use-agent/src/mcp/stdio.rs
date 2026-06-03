//! Stdio MCP transport: spawn a child process and speak newline-delimited
//! JSON-RPC over its stdin/stdout.
//!
//! This is an async (tokio) port of the legacy in-house client
//! (`browser-use-core/src/mcp.rs`):
//! - process spawn: `PersistentMcpConnection::start` (`:732-744`) — args, cwd,
//!   envs, stdin/stdout piped, stderr (legacy pipes+buffers it; we inherit it).
//! - handshake: `initialize` (`:776-804`) writes `initialize`, reads the
//!   matching-id response, then writes `notifications/initialized`.
//! - framing: `write_json_rpc` (`:1014-1020`) = serialize + `\n` + flush;
//!   `read_json_rpc_response` (`:1022-1040`) = read a line, skip non-matching
//!   ids, error on an `error` field, return `result`.
//! - request ids: monotonic `i64` (`:812-816`).
//!
//! The legacy client is synchronous (blocking reads keyed to one id at a time).
//! Because our manager may issue concurrent calls and a server may interleave
//! server->client elicitation requests, we use a background reader task plus an
//! id->oneshot pending map (the standard async correlation pattern) instead of
//! the legacy blocking read loop, while preserving the exact wire framing.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};
use tokio::time::timeout;

use crate::mcp::protocol::{
    call_tool_params, elicitation_decline_result, initialize_params, initialized_notification,
    CallToolResult, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, ListToolsResult,
    McpToolInfo, RequestId,
};

type PendingMap = Arc<Mutex<HashMap<RequestId, oneshot::Sender<JsonRpcMessage>>>>;

const MCP_SERVER_REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// A connected stdio MCP server.
pub struct StdioTransport {
    /// Kept alive (killed on drop via `kill_on_drop`) for the transport's life.
    #[allow(dead_code)]
    child: Child,
    writer: Arc<Mutex<ChildStdin>>,
    pending: PendingMap,
    next_id: AtomicI64,
    tool_timeout: Duration,
    /// Background reader task; aborted on drop.
    read_handle: tokio::task::JoinHandle<()>,
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.read_handle.abort();
    }
}

impl StdioTransport {
    /// Spawn the configured command, perform the `initialize` handshake, send
    /// `notifications/initialized`, and return a ready transport. Mirrors legacy
    /// `PersistentMcpConnection::start` + `initialize`
    /// (`browser-use-core/src/mcp.rs:732-804`).
    pub async fn connect(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        cwd: Option<&std::path::Path>,
        startup_timeout: Duration,
        tool_timeout: Duration,
    ) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args);
        cmd.envs(env);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::inherit());
        cmd.kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to start MCP server `{command}`"))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("MCP child stdout was not captured"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("MCP child stdin was not captured"))?;
        let reader = BufReader::new(stdout);

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let writer = Arc::new(Mutex::new(stdin));

        // Background reader: dispatch responses by id; answer server requests
        // (elicitation) with a decline.
        let pending_reader = pending.clone();
        let writer_reader = writer.clone();
        let read_handle = tokio::spawn(async move {
            let mut lines = reader.lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let Ok(msg) = serde_json::from_str::<JsonRpcMessage>(trimmed) else {
                            continue;
                        };
                        if msg.is_server_request() {
                            handle_server_request(&writer_reader, msg).await;
                        } else if msg.is_response() {
                            if let Some(id) = msg.id.clone() {
                                let mut guard = pending_reader.lock().await;
                                if let Some(tx) = guard.remove(&id) {
                                    let _ = tx.send(msg);
                                }
                            }
                        }
                        // Notifications (no id, no result) are ignored.
                    }
                    Ok(None) => break, // EOF: child closed stdout.
                    Err(_) => break,
                }
            }
        });

        let transport = Self {
            child,
            writer,
            pending,
            next_id: AtomicI64::new(1),
            tool_timeout,
            read_handle,
        };

        // Handshake under the startup timeout.
        transport
            .request("initialize", Some(initialize_params()), startup_timeout)
            .await
            .context("initialize handshake failed")?;
        transport
            .notify(initialized_notification(), startup_timeout)
            .await
            .context("sending notifications/initialized failed")?;

        Ok(transport)
    }

    /// `tools/list` round-trip (legacy `McpOperation::ListTools`,
    /// `browser-use-core/src/mcp.rs:578-583`).
    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>> {
        let result = self.request("tools/list", None, self.tool_timeout).await?;
        let parsed: ListToolsResult = serde_json::from_value(result)?;
        Ok(parsed.tools)
    }

    /// `tools/call` round-trip (legacy `McpOperation::CallTool`,
    /// `browser-use-core/src/mcp.rs:584-592`).
    pub async fn call_tool(&self, tool: &str, arguments: Option<Value>) -> Result<CallToolResult> {
        let params = call_tool_params(tool, arguments);
        let result = self
            .request("tools/call", Some(params), self.tool_timeout)
            .await?;
        let parsed: CallToolResult = serde_json::from_value(result)?;
        Ok(parsed)
    }

    /// Send a request and await the response. The pending receiver is registered
    /// BEFORE writing to avoid a response-before-register race. The error/result
    /// extraction mirrors legacy `read_json_rpc_response`
    /// (`browser-use-core/src/mcp.rs:1022-1040`).
    async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        timeout_dur: Duration,
    ) -> Result<Value> {
        let id = RequestId::from_i64(self.next_id.fetch_add(1, Ordering::SeqCst));
        let (tx, rx) = oneshot::channel();
        {
            let mut guard = self.pending.lock().await;
            guard.insert(id.clone(), tx);
        }

        let req = JsonRpcRequest::new(id.clone(), method, params);
        let operation = async {
            write_json(&self.writer, &req).await?;
            rx.await
                .map_err(|_| anyhow!("MCP server closed stdout before responding to {method}"))
        };

        let msg = match timeout(timeout_dur, operation).await {
            Ok(Ok(msg)) => msg,
            Ok(Err(err)) => {
                self.pending.lock().await.remove(&id);
                return Err(err);
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                bail!("MCP server timed out on {method} after {timeout_dur:?}");
            }
        };

        if let Some(err) = msg.error {
            bail!(
                "MCP server returned error for {method}: {} (code {})",
                err.message,
                err.code
            );
        }
        Ok(msg.result.unwrap_or(Value::Null))
    }

    async fn notify(&self, notification: JsonRpcNotification, timeout_dur: Duration) -> Result<()> {
        match timeout(timeout_dur, write_json(&self.writer, &notification)).await {
            Ok(result) => result,
            Err(_) => {
                bail!("MCP server timed out while sending notification after {timeout_dur:?}")
            }
        }
    }
}

/// Answer a server-originated request. We only handle `elicitation/create`
/// (decline); anything else gets a generic "method not found" so the server is
/// not left hanging.
async fn handle_server_request(writer: &Arc<Mutex<ChildStdin>>, msg: JsonRpcMessage) {
    let Some(id) = msg.id else { return };
    let method = msg.method.unwrap_or_default();
    let reply = if method == "elicitation/create" {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": elicitation_decline_result(),
        })
    } else {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("method not found: {method}") },
        })
    };
    let _ = timeout(MCP_SERVER_REPLY_TIMEOUT, write_json(writer, &reply)).await;
}

/// Write a serializable message as a newline-terminated JSON line, then flush.
/// Mirrors legacy `write_json_rpc` (`browser-use-core/src/mcp.rs:1014-1020`).
async fn write_json<T: serde::Serialize>(writer: &Arc<Mutex<ChildStdin>>, msg: &T) -> Result<()> {
    let mut data = serde_json::to_vec(msg)?;
    data.push(b'\n');
    let mut guard = writer.lock().await;
    guard.write_all(&data).await?;
    guard.flush().await?;
    Ok(())
}
