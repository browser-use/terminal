//! MCP tool-dispatch handler.
//!
//! This is the async re-implementation of codex's MCP tool handler over our
//! merged [`ToolRuntime`](crate::tools::runtime::ToolRuntime) seam. It is the
//! MODEL-FACING dispatch handler: it takes a namespaced MCP tool call
//! (`mcp__<server>__<tool>` style, parsed into an [`McpToolCallRequest`]),
//! forwards it to an MCP client behind a trait, and maps the returned
//! `CallToolResult` into the seam's [`ExecOutput`].
//!
//! # Why the transport is deferred (out of scope)
//!
//! The EXISTING MCP client lives in `browser-use-core` (`src/mcp.rs`, the sync
//! stdio JSON-RPC client: `tools/list` / `tools/call` / `resources/*`). It pulls
//! in core types, and `browser-use-agent` cannot depend on `browser-use-core`
//! (that would be a dependency cycle / a heavy pull). The real connection +
//! transport wiring (spawning servers, JSON-RPC framing, capability negotiation)
//! is a later subsystem WP (MCP transports). THIS WP is the model-facing
//! dispatch handler only.
//!
//! To keep the handler crate-local and testable we define a small
//! [`McpClient`] trait plus minimal local result types ([`McpCallResult`]) that
//! mirror the real `CallToolResult` shape. The production client (the wiring to
//! `browser-use-core`'s MCP client) drops in behind this trait later; tests
//! inject a fake and never touch a real MCP server / the network.
//!
//! # Parity grounding
//!
//! * **codex** `core/src/tools/handlers/mcp.rs` + `core/src/mcp_tool_call.rs`:
//!     * `supports_parallel_tool_calls` (handlers/mcp.rs:46-57): an MCP tool is
//!       parallel-safe only if the server opted in OR the tool's
//!       `read_only_hint` is set. Mirrored by [`ToolRuntime::parallel_safe`].
//!     * the error-vs-success split: `result.is_error.unwrap_or(false)`
//!       (mcp_tool_call.rs:886) selects `Failed`; a transport `Err` becomes a
//!       model-facing error string (mcp_tool_call.rs:579). Mirrored by
//!       [`map_call_result`] + [`McpTool::run`].
//!     * the model-facing event cap is `MCP_TOOL_CALL_EVENT_RESULT_MAX_BYTES`
//!       applied only in `truncate_mcp_tool_result_for_event`
//!       (mcp_tool_call.rs:805-847) — the EVENT copy, NOT the model-facing
//!       result. See [`MCP_EVENT_RESULT_MAX_CHARS`].
//! * **legacy** `browser-use-core/src/mcp.rs`:
//!     * `CallToolResult` shape (mcp.rs:150-163): `content`, `is_error`
//!       (`isError`), `structured_content` (`structuredContent`). Mirrored 1:1
//!       by [`McpCallResult`].
//!     * `call_tool(server, tool, arguments) -> Result<CallToolResult>` is SYNC
//!       (blocking stdio JSON-RPC); so [`McpClient::call_tool`] is sync and run
//!       on a blocking thread, matching the [`browser`](super::browser) /
//!       [`python`](super::python) handlers.
//! * **legacy** `browser-use-core/src/lib.rs`:
//!     * `dispatch_mcp_tool(server, tool, arguments, call_id)`
//!       (lib.rs:13398-13430): unknown manager → error; call failure →
//!       `MCP tool call failed: {server}/{tool}: {e}`; map via
//!       `mcp_result_tool_content`; `is_err = result.is_error` selects
//!       error/success. Mirrored by [`McpTool::run`] + [`map_call_result`].
//!     * `mcp_result_tool_content` (lib.rs:13817-13833): walk content items —
//!       `Text` → text, `Image` → `"[image content]"` marker, other →
//!       JSON-encoded; joined with `\n`. Mirrored by [`mcp_result_tool_content`].
//!     * `MCP_EVENT_RESULT_MAX_CHARS = 20_000` (mcp.rs): EVENT-LOG-ONLY. The
//!       MODEL-FACING output here is NOT capped; it flows through the
//!       orchestrator's normal tool-output budget. See [`MCP_EVENT_RESULT_MAX_CHARS`].
//!
//! # Approval / sandbox / parallelism
//!
//! * **approval**: codex gates "consequential" MCP tools behind an approval
//!   prompt driven by the server-advertised hints (mcp_tool_call.rs
//!   `requires_mcp_tool_approval` + `maybe_request_mcp_tool_approval`). In this
//!   seam the orchestrator's policy-driven default approval suffices, so
//!   [`Approvable::exec_approval_requirement`] is left at `None`. See the
//!   `TODO(WP-mcp-approval-gating)` below.
//! * **sandbox**: the MCP call runs in the server process, not ours, so we use
//!   [`SandboxPreference::Auto`] / [`SandboxPermissions::UseDefault`] (uniform
//!   with the other handlers; today everything resolves to `SandboxType::None`).
//! * **parallel_safe**: an MCP tool's parallel-safety comes from the server's
//!   read-only hint. We default to `false` (serial) and only return `true` when
//!   the request is explicitly flagged read-only via
//!   [`McpToolCallRequest::read_only`]. This matches codex
//!   (handlers/mcp.rs:46-57), where MCP tools are excluded from the parallel set
//!   unless the server advertises read-only.

use std::sync::Arc;

use serde_json::Value;

use crate::tools::approval::ExecApprovalRequirement;
use crate::tools::runtime::{Approvable, Sandboxable};
use crate::tools::runtime::{ExecOutput, SandboxAttempt, ToolCtx, ToolError, ToolRuntime};
use crate::tools::sandbox::{SandboxPermissions, SandboxPreference};

/// Maximum characters of an MCP tool result retained in the EVENT LOG.
///
/// PARITY NOTE: this mirrors legacy `MCP_EVENT_RESULT_MAX_CHARS = 20_000`
/// (browser-use-core mcp.rs). It is EVENT-LOG-ONLY: the legacy code applies it
/// when recording the result into the event stream (and codex applies the
/// analogous `MCP_TOOL_CALL_EVENT_RESULT_MAX_BYTES` only in
/// `truncate_mcp_tool_result_for_event`, mcp_tool_call.rs:805-847), NOT to the
/// model-facing tool output. The model-facing output produced by
/// [`McpTool::run`] is intentionally uncapped here; it is subject to the
/// orchestrator's normal tool-output budget (the same budget the other
/// handlers' [`ExecOutput`] flows through). The constant is exported so the
/// later event-log/transport WP can reuse the value without re-deriving it.
pub const MCP_EVENT_RESULT_MAX_CHARS: usize = 20_000;

/// Exit code reported when an MCP tool result has `is_error = true`.
///
/// The seam's [`ExecOutput`] carries an `exit_code`; codex/legacy fold an MCP
/// error into a model-facing error (codex mcp_tool_call.rs:886, legacy
/// lib.rs:13425-13426 -> `ToolDispatchResult::error`). We surface that as a
/// nonzero exit code plus the content on stderr so the orchestrator's
/// success/failure classification matches.
pub const MCP_ERROR_EXIT_CODE: i32 = 1;

/// A model-facing MCP tool call, already parsed out of the namespaced
/// `mcp__<server>__<tool>` function name.
///
/// Field shape follows legacy `dispatch_mcp_tool(server, tool, arguments, ..)`
/// (browser-use-core lib.rs:13398-13403) and codex's
/// `(server, tool)` + `arguments`. The model-facing function name is namespaced;
/// the caller (the registry/router, a later WP) splits it before constructing
/// this request, so the handler receives the already-resolved `server` / `tool`.
///
/// # Deserialization (via [`McpWireArgs`])
///
/// `server` / `tool` are split out of the namespaced function NAME (not the
/// model's argument object), so this `Req` deserializes THROUGH the resolved flat
/// [`McpWireArgs`] object: `#[serde(from = "McpWireArgs")]` runs the
/// [`From<McpWireArgs>`](McpToolCallRequest::from) adapter after deserializing the
/// resolved args. This makes `McpToolCallRequest: Deserialize`, so the tool
/// registers with the registry's plain `register`. Behavior is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(from = "McpWireArgs")]
pub struct McpToolCallRequest {
    /// The MCP server the tool lives on (the first segment after `mcp__`).
    pub server: String,
    /// The tool name on that server (the remaining segment(s)).
    pub tool: String,
    /// The JSON arguments object for the call. A `Value::Null` is treated as
    /// "no arguments" when forwarding (legacy `dispatch_mcp_tool` takes an
    /// `Option<Value>`; codex maps an empty argument string to `None`).
    pub arguments: Value,
    /// Whether the server advertised this tool as read-only (parallel-safe).
    ///
    /// MCP tools' parallel-safety comes from the server's read-only hint; this
    /// is the resolved value (the router populates it from the server's tool
    /// listing). Defaults to `false` (serial) — see
    /// [`ToolRuntime::parallel_safe`].
    pub read_only: bool,
}

impl McpToolCallRequest {
    /// Convenience constructor: a (serial) call with the given arguments.
    pub fn new(server: impl Into<String>, tool: impl Into<String>, arguments: Value) -> Self {
        Self {
            server: server.into(),
            tool: tool.into(),
            arguments,
            read_only: false,
        }
    }

    /// Parse a namespaced `mcp__<server>__<tool>` function name into a request.
    ///
    /// Parity: codex's MCP tool names are `mcp__<server>__<tool>` (e.g.
    /// `mcp__memory__create_entities`, mcp_tool_call tests). The server is the
    /// first segment after `mcp__`; the tool is the remainder (which may itself
    /// contain `__`, so we split only ONCE after the server). Returns `None` if
    /// the name is not a well-formed MCP namespaced name.
    pub fn parse_namespaced(name: &str, arguments: Value) -> Option<Self> {
        let rest = name.strip_prefix("mcp__")?;
        let (server, tool) = rest.split_once("__")?;
        if server.is_empty() || tool.is_empty() {
            return None;
        }
        Some(Self::new(server, tool, arguments))
    }

    /// The arguments to forward to the client: `None` when the arguments are
    /// JSON `null` (legacy `dispatch_mcp_tool` arguments are `Option<Value>`),
    /// otherwise the value cloned.
    fn forward_arguments(&self) -> Option<Value> {
        if self.arguments.is_null() {
            None
        } else {
            Some(self.arguments.clone())
        }
    }
}

/// Model-facing wire arguments for the MCP dispatch handler.
///
/// [`McpToolCallRequest`] is a PARSED form: its `server` / `tool` are split out
/// of the namespaced `mcp__<server>__<tool>` function NAME (not present in the
/// model's argument object), and `read_only` is resolved from the server's tool
/// listing. So the registry cannot deserialize a `McpToolCallRequest` directly
/// from the model's argument object. Instead this `McpWireArgs` matches a flat
/// object carrying the resolved `server` / `tool` plus the call `arguments`, and
/// an [`From<McpWireArgs>`](McpToolCallRequest::from) adapter builds the typed
/// request (the registry registers the tool over `McpWireArgs`).
///
/// # Wire shape (router-resolved args)
///
/// ```json
/// { "server": "memory", "tool": "create_entities", "arguments": { /* ... */ } }
/// ```
///
/// Parity: legacy `dispatch_mcp_tool(server, tool, arguments, ..)`
/// (`browser-use-core/src/lib.rs:13398-13403`) and codex's `(server, tool)` +
/// `arguments` (codex `core/src/mcp_tool_call.rs`). `arguments` defaults to
/// `Value::Null` ("no arguments") and `read_only` defaults to `false` (serial),
/// matching [`McpToolCallRequest::new`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct McpWireArgs {
    /// The MCP server the tool lives on.
    pub server: String,
    /// The tool name on that server.
    pub tool: String,
    /// The JSON arguments object for the call (`Value::Null` = no arguments).
    #[serde(default)]
    pub arguments: Value,
    /// Whether the server advertised this tool as read-only (parallel-safe).
    #[serde(default)]
    pub read_only: bool,
}

impl From<McpWireArgs> for McpToolCallRequest {
    fn from(w: McpWireArgs) -> Self {
        McpToolCallRequest {
            server: w.server,
            tool: w.tool,
            arguments: w.arguments,
            read_only: w.read_only,
        }
    }
}

/// The result of an MCP `tools/call`, mirroring the real `CallToolResult`.
///
/// PARITY: 1:1 with legacy `CallToolResult` (browser-use-core mcp.rs:150-163):
/// the content array, the `isError` flag, and the optional `structuredContent`.
/// Defined locally (rather than importing the core type) so this crate does not
/// depend on `browser-use-core`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct McpCallResult {
    /// The content items (text / image / resource) produced by the tool. Each is
    /// a raw JSON value (an object with a `"type"` discriminator).
    pub content: Vec<Value>,
    /// Whether the tool reported an error (`isError`).
    pub is_error: bool,
    /// Optional structured content payload (`structuredContent`).
    pub structured_content: Option<Value>,
}

impl McpCallResult {
    /// Construct a successful text result (test/convenience helper).
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![serde_json::json!({ "type": "text", "text": text.into() })],
            is_error: false,
            structured_content: None,
        }
    }

    /// Construct an error text result (test/convenience helper).
    pub fn error_text(text: impl Into<String>) -> Self {
        Self {
            content: vec![serde_json::json!({ "type": "text", "text": text.into() })],
            is_error: true,
            structured_content: None,
        }
    }
}

/// The seam over the MCP client.
///
/// Implemented for real by the later MCP-transport WP (wiring to
/// `browser-use-core`'s sync JSON-RPC client) and by a fake in tests so the
/// dispatch handler can be exercised without a real MCP server / the network.
///
/// `call_tool` is SYNC to mirror the real legacy client
/// (`browser-use-core` `McpClientManager::call_tool`, blocking stdio JSON-RPC).
/// The adapter runs it on a blocking thread (see [`McpTool::run`]), matching the
/// [`browser`](super::browser) / [`python`](super::python) handlers. The
/// `server` is passed explicitly so a single client can route to multiple
/// connected servers.
pub trait McpClient: Send + Sync {
    /// Dispatch a `tools/call` for `tool` on `server` with `args`.
    ///
    /// `args` is `None` when the model supplied no arguments (a JSON `null`);
    /// the implementation forwards an empty object in that case. Errors are
    /// `anyhow::Error`, mirroring the wrapped client.
    fn call_tool(
        &self,
        server: &str,
        tool: &str,
        args: Option<Value>,
    ) -> anyhow::Result<McpCallResult>;
}

/// Map an [`McpCallResult`]'s content array into model-facing text.
///
/// PARITY: legacy `mcp_result_tool_content` (browser-use-core lib.rs:13817-13833).
/// Each content item is walked by its `"type"` discriminator:
///   * `"text"` -> the item's `"text"` string (concatenated)
///   * `"image"` -> a `"[image content]"` marker (the raw base64 is not surfaced
///     to the text channel; this matches the legacy marker)
///   * anything else (resource / structured / unknown) -> JSON-encoded verbatim
/// The parts are joined with `\n`.
///
/// NOTE on structured content: the legacy model-facing mapping derives the text
/// purely from `content`; `structured_content` is not folded into the text
/// channel (it is carried for callers that want the typed payload). We follow
/// that — a structured item that appears IN the content array (e.g. a
/// `"type": "resource"` block) is JSON-encoded by the `_ =>` arm.
pub fn mcp_result_tool_content(result: &McpCallResult) -> String {
    let mut parts: Vec<String> = Vec::new();
    for item in &result.content {
        match item.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                    parts.push(t.to_string());
                }
            }
            Some("image") => {
                parts.push("[image content]".to_string());
            }
            _ => {
                parts.push(serde_json::to_string(item).unwrap_or_default());
            }
        }
    }
    parts.join("\n")
}

/// Map a successful client [`McpCallResult`] into an [`ExecOutput`].
///
/// PARITY: legacy lib.rs:13423-13429. The model-facing `content` is produced by
/// [`mcp_result_tool_content`]. When `is_error` is set, the dispatch is a
/// model-facing error (codex mcp_tool_call.rs:886) — we surface that as a
/// nonzero exit code with the content on `stderr` (so the orchestrator
/// classifies it as a failure) and an empty `stdout`. On success the content is
/// the `stdout` with a `0` exit code. The result is NOT truncated here (the 20k
/// [`MCP_EVENT_RESULT_MAX_CHARS`] cap is event-log-only).
fn map_call_result(result: McpCallResult) -> ExecOutput {
    let content = mcp_result_tool_content(&result);
    if result.is_error {
        ExecOutput {
            exit_code: MCP_ERROR_EXIT_CODE,
            stdout: String::new(),
            stderr: content,
        }
    } else {
        ExecOutput {
            exit_code: 0,
            stdout: content,
            stderr: String::new(),
        }
    }
}

/// Test-only re-export of [`map_call_result`] for direct unit assertions.
#[cfg(test)]
pub(crate) fn map_call_result_for_test(result: McpCallResult) -> ExecOutput {
    map_call_result(result)
}

/// The MCP tool-dispatch handler.
///
/// Holds the MCP client behind a trait object so production code uses the real
/// (transport-wired) client and tests inject a fake. Construct with
/// [`McpTool::new`].
#[derive(Clone)]
pub struct McpTool {
    client: Arc<dyn McpClient>,
}

impl std::fmt::Debug for McpTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpTool").finish_non_exhaustive()
    }
}

impl McpTool {
    /// Construct an MCP tool over the given client.
    ///
    /// There is no `Default`/no-client constructor: unlike the browser/python
    /// handlers there is no real client to default to yet (the transport is a
    /// later WP), so a client must always be supplied.
    pub fn new(client: Arc<dyn McpClient>) -> Self {
        Self { client }
    }
}

/// Approval key: the server + tool + arguments identify an MCP call for session
/// caching, mirroring the shape the other handlers use.
#[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub struct McpApprovalKey {
    server: String,
    tool: String,
    /// The serialized arguments, so two identical calls share a cache entry.
    arguments: String,
}

impl Approvable<McpToolCallRequest> for McpTool {
    type ApprovalKey = McpApprovalKey;

    fn approval_keys(&self, req: &McpToolCallRequest) -> Vec<Self::ApprovalKey> {
        vec![McpApprovalKey {
            server: req.server.clone(),
            tool: req.tool.clone(),
            arguments: serde_json::to_string(&req.arguments).unwrap_or_default(),
        }]
    }

    /// The MCP call runs in the server process; request the default sandbox
    /// permissions (no escalation).
    fn sandbox_permissions(&self, _req: &McpToolCallRequest) -> SandboxPermissions {
        SandboxPermissions::UseDefault
    }

    // `exec_approval_requirement` left at its trait default (`None`).
    //
    // TODO(WP-mcp-approval-gating): codex gates "consequential" MCP tools behind
    // an approval prompt driven by the server-advertised hints
    // (mcp_tool_call.rs `requires_mcp_tool_approval` +
    // `maybe_request_mcp_tool_approval`). Our seam already routes approval
    // through the orchestrator's policy-driven default
    // (`default_exec_approval_requirement`), which is sufficient for this WP;
    // the per-tool consequential-gating refinement (mapping `!read_only` /
    // destructive hints to `NeedsApproval` under the right policy) is deferred to
    // the MCP subsystem WP, where the full tool listing / annotations are
    // available.
    fn exec_approval_requirement(
        &self,
        _req: &McpToolCallRequest,
    ) -> Option<ExecApprovalRequirement> {
        None
    }
}

impl Sandboxable for McpTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        // The MCP server manages its own process/isolation; let the provider
        // decide (today everything resolves to `SandboxType::None`). `Auto` keeps
        // the seam uniform with the other tools.
        SandboxPreference::Auto
    }

    fn escalate_on_failure(&self) -> bool {
        // An MCP failure is not a sandbox denial we can usefully retry
        // unsandboxed; keep it uniform with the other tools.
        true
    }
}

#[async_trait::async_trait]
impl ToolRuntime<McpToolCallRequest, ExecOutput> for McpTool {
    fn parallel_safe(&self, req: &McpToolCallRequest) -> bool {
        // An MCP tool's parallel-safety comes from the server's read-only hint.
        // Default to serial (`false`); only a tool explicitly flagged read-only
        // may run concurrently. Codex parity: handlers/mcp.rs:46-57
        // (`supports_parallel_tool_calls` = server opt-in OR `read_only_hint`).
        req.read_only
    }

    async fn run(
        &self,
        req: &McpToolCallRequest,
        attempt: &SandboxAttempt<'_>,
        ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        // The MCP call runs in the server process; no sandbox backend is
        // exercised here. Acknowledge the seam args explicitly.
        let cancel = attempt.cancel.clone();
        let _ = ctx;

        // Validate the request before touching the client (legacy unknown-server
        // / empty-name guards, lib.rs:13404-13411).
        if req.server.trim().is_empty() {
            return Err(ToolError::Rejected(
                "MCP server must not be empty".to_string(),
            ));
        }
        if req.tool.trim().is_empty() {
            return Err(ToolError::Rejected(
                "MCP tool must not be empty".to_string(),
            ));
        }

        let client = Arc::clone(&self.client);
        let server = req.server.clone();
        let tool = req.tool.clone();
        let args = req.forward_arguments();

        // The real client is synchronous (blocking stdio JSON-RPC); run it on a
        // blocking thread so we never stall the async runtime (mirrors the
        // browser/python handlers).
        let task = tokio::task::spawn_blocking(move || -> Result<ExecOutput, ToolError> {
            let call = client
                .call_tool(&server, &tool, args)
                // Parity with legacy lib.rs:13416-13419 / codex mcp_tool_call.rs:579:
                // a transport failure is a model-facing error naming the call.
                .map_err(|e| {
                    ToolError::Other(anyhow::anyhow!(
                        "MCP tool call failed: {server}/{tool}: {e}"
                    ))
                })?;
            Ok(map_call_result(call))
        });

        let result = tokio::select! {
            biased;
            _ = async {
                if let Some(cancel) = cancel {
                    cancel.cancelled().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                return Err(ToolError::Other(anyhow::anyhow!("MCP task cancelled")));
            }
            joined = task => joined
                .map_err(|e| ToolError::Other(anyhow::anyhow!("MCP task panicked: {e}")))?,
        };

        result
    }
}
