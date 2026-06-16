//! Network-free tests for the MCP transports.
//!
//! Allowed I/O (local, not external network): loopback `127.0.0.1` TCP via
//! `tokio::net::TcpListener` bound to port 0, and spawning local child-process
//! fixtures (a tiny `python3` JSON-RPC responder written into a `tempdir`). No
//! external host is contacted; no real OAuth browser flow runs.

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::mcp::config::{McpServerConfig, McpServerTransport};
use crate::mcp::http::StreamableHttpTransport;
use crate::mcp::manager::{fully_qualified_tool_name, parse_tool_name, McpConnectionManager};
use crate::mcp::oauth::{
    build_authorization_url, code_challenge_s256, generate_pkce, parse_redirect_callback,
    perform_interactive_login, OAuthTokenStore, OauthError, StoredOAuthTokens,
};
use crate::mcp::stdio::StdioTransport;
use crate::tools::handlers::mcp::{mcp_result_tool_content, McpCallResult, McpClient, McpTool};

// ---------------------------------------------------------------------------
// stdio fixture
// ---------------------------------------------------------------------------

/// A tiny `python3` MCP server that reads newline-delimited JSON-RPC on stdin and
/// emits canned `initialize` / `tools/list` / `tools/call` responses on stdout.
/// It echoes each request's id back, matching the wire framing of the legacy
/// client (`browser-use-core/src/mcp.rs:1014-1040`).
const STDIO_FIXTURE_PY: &str = r#"
import sys, json

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except Exception:
        continue
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "serverInfo": {"name": "fixture", "version": "0.0.1"},
        }})
    elif method == "notifications/initialized":
        pass  # notification, no response
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": [
            {"name": "echo", "description": "echo back",
             "inputSchema": {"type": "object"},
             "annotations": {"readOnlyHint": True}},
            {"name": "danger", "description": "not read only",
             "inputSchema": {"type": "object"}},
        ]}})
    elif method == "tools/call":
        params = msg.get("params") or {}
        name = params.get("name")
        args = params.get("arguments")
        if name == "boom":
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "content": [{"type": "text", "text": "kaboom"}],
                "isError": True,
            }})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "content": [{"type": "text", "text": "called " + str(name)}],
                "isError": False,
                "structuredContent": {"args": args},
            }})
    else:
        send({"jsonrpc": "2.0", "id": mid,
              "error": {"code": -32601, "message": "method not found"}})
"#;

const STDIO_STALL_AFTER_INIT_PY: &str = r#"
import sys, json, time

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

line = sys.stdin.readline()
msg = json.loads(line)
send({"jsonrpc": "2.0", "id": msg.get("id"), "result": {
    "protocolVersion": "2024-11-05",
    "capabilities": {},
    "serverInfo": {"name": "stall", "version": "0.0.1"},
}})

# Do not read stdin again. A large client request can fill the pipe while the
# transport is still in write_json, before its per-request response timeout is
# armed.
time.sleep(60)
"#;

/// Write the python fixture into a tempdir and return its path. The tempdir is
/// kept alive by the returned guard.
fn write_stdio_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    write_stdio_fixture_source(STDIO_FIXTURE_PY)
}

fn write_stdio_fixture_source(source: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("server.py");
    let mut f = std::fs::File::create(&path).expect("create fixture");
    f.write_all(source.as_bytes()).expect("write fixture");
    f.flush().expect("flush fixture");
    (dir, path)
}

fn stdio_config(script: &std::path::Path) -> McpServerConfig {
    McpServerConfig {
        transport: McpServerTransport::Stdio {
            command: "python3".to_string(),
            args: vec![script.to_string_lossy().to_string()],
            env: HashMap::new(),
            cwd: None,
        },
        startup_timeout_ms: Some(5_000),
        tool_timeout_ms: Some(5_000),
        enabled_tools: None,
        disabled_tools: None,
    }
}

#[tokio::test]
async fn stdio_list_and_call_roundtrip() {
    let (_dir, script) = write_stdio_fixture();
    let transport = StdioTransport::connect(
        "python3",
        &[script.to_string_lossy().to_string()],
        &HashMap::new(),
        None,
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .await
    .expect("connect stdio");

    let tools = transport.list_tools().await.expect("list tools");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"echo"), "tools: {names:?}");
    // read-only hint is parsed from annotations.readOnlyHint.
    let echo = tools.iter().find(|t| t.name == "echo").unwrap();
    assert!(echo.read_only_hint());
    let danger = tools.iter().find(|t| t.name == "danger").unwrap();
    assert!(!danger.read_only_hint());

    let result = transport
        .call_tool("echo", Some(json!({"x": 1})))
        .await
        .expect("call tool");
    assert!(!result.is_error);
    let seam = result.into_seam();
    assert_eq!(mcp_result_tool_content(&seam), "called echo");
}

#[tokio::test]
async fn stdio_error_result_maps_is_error() {
    let (_dir, script) = write_stdio_fixture();
    let transport = StdioTransport::connect(
        "python3",
        &[script.to_string_lossy().to_string()],
        &HashMap::new(),
        None,
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .await
    .expect("connect stdio");

    let result = transport.call_tool("boom", None).await.expect("call boom");
    assert!(result.is_error);
    assert_eq!(mcp_result_tool_content(&result.into_seam()), "kaboom");
}

#[tokio::test]
#[ignore = "diagnostic repro: stdio MCP timeout does not cover a blocked stdin write"]
async fn repro_stdio_call_can_remain_pending_before_response_timeout_is_armed() {
    let (_dir, script) = write_stdio_fixture_source(STDIO_STALL_AFTER_INIT_PY);
    let transport = StdioTransport::connect(
        "python3",
        &[script.to_string_lossy().to_string()],
        &HashMap::new(),
        None,
        Duration::from_secs(2),
        Duration::from_millis(50),
    )
    .await
    .expect("connect stalling stdio fixture");

    let large_arg = "x".repeat(32 * 1024 * 1024);
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        transport.call_tool("stall", Some(json!({ "blob": large_arg }))),
    )
    .await;

    assert!(
        result.is_err(),
        "MCP call returned inside the outer watchdog; this repro expects write_json to remain pending before the tool timeout"
    );
}

// ---------------------------------------------------------------------------
// http transport (loopback TcpListener)
// ---------------------------------------------------------------------------

/// Minimal one-shot HTTP/1.1 JSON-RPC responder over a loopback `TcpListener`.
/// For every POST it reads the request, optionally asserts the
/// `Authorization: Bearer` header, and replies with `body` using `content_type`.
/// Returns the captured raw request headers+body of the FIRST non-initialize
/// (or all) request via the channel so the test can assert on them.
async fn serve_canned_http(
    listener: tokio::net::TcpListener,
    content_type: &'static str,
    require_bearer: Option<&'static str>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // Accept connections; serve every HTTP request on a connection (reqwest may
    // pool/reuse the keep-alive connection across initialize / initialized /
    // tools/*). We read one full request at a time (the bodies are small, so one
    // read per request suffices for this fixture) and respond with `Connection:
    // close`-free keep-alive so reqwest may reuse the socket. We loop accepting
    // until the test aborts the task.
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        // Serve sequential requests on this connection until it closes.
        loop {
            let mut buf = vec![0u8; 8192];
            let n = match socket.read(&mut buf).await {
                Ok(0) | Err(_) => break, // connection closed by client
                Ok(n) => n,
            };
            let request = String::from_utf8_lossy(&buf[..n]).to_string();

            if let Some(token) = require_bearer {
                assert!(
                    request.contains(&format!("authorization: Bearer {token}"))
                        || request.contains(&format!("Authorization: Bearer {token}")),
                    "missing bearer header in request:\n{request}"
                );
            }

            // Parse the JSON-RPC id from the request body (after the blank line).
            let body_start = request.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
            let body = &request[body_start..];
            let id = serde_json::from_str::<Value>(body.trim())
                .ok()
                .and_then(|v| v.get("id").cloned())
                .unwrap_or(Value::Null);
            let method = serde_json::from_str::<Value>(body.trim())
                .ok()
                .and_then(|v| v.get("method").and_then(|m| m.as_str().map(String::from)));

            // Notifications (notifications/initialized) carry no id: just 202.
            if id.is_null() {
                let _ = socket
                    .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n")
                    .await;
                continue;
            }

            let result = match method.as_deref() {
                Some("initialize") => json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "serverInfo": {"name": "http-fixture", "version": "0.0.1"}
                }),
                Some("tools/list") => json!({"tools": [
                    {"name": "ping", "description": "ping", "inputSchema": {"type": "object"}}
                ]}),
                Some("tools/call") => json!({
                    "content": [{"type": "text", "text": "pong"}],
                    "isError": false
                }),
                _ => json!({}),
            };
            let rpc = json!({"jsonrpc": "2.0", "id": id, "result": result});

            let payload = if content_type.contains("event-stream") {
                // SSE: one event whose `data:` line is the JSON-RPC message.
                format!("data: {}\n\n", serde_json::to_string(&rpc).unwrap())
            } else {
                serde_json::to_string(&rpc).unwrap()
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{payload}",
                payload.len()
            );
            if socket.write_all(response.as_bytes()).await.is_err() {
                break;
            }
            let _ = socket.flush().await;
        }
    }
}

#[tokio::test]
async fn http_json_roundtrip_with_bearer() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(serve_canned_http(
        listener,
        "application/json",
        Some("secret-token"),
    ));

    let url = format!("http://{addr}/mcp");
    let transport = StreamableHttpTransport::connect(
        &url,
        Some("secret-token".to_string()),
        HashMap::new(),
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .await
    .expect("http connect");

    let tools = transport.list_tools().await.expect("list");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "ping");

    let result = transport.call_tool("ping", None).await.expect("call");
    assert_eq!(mcp_result_tool_content(&result.into_seam()), "pong");

    server.abort();
}

#[tokio::test]
async fn http_sse_response_path() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(serve_canned_http(listener, "text/event-stream", None));

    let url = format!("http://{addr}/mcp");
    let transport = StreamableHttpTransport::connect(
        &url,
        None,
        HashMap::new(),
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .await
    .expect("http connect (sse)");

    let result = transport.call_tool("ping", None).await.expect("call sse");
    assert_eq!(mcp_result_tool_content(&result.into_seam()), "pong");

    server.abort();
}

#[test]
fn sse_parser_handles_multiline_and_blank_separators() {
    let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n\
                event: ping\n\
                data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n\n";
    let msgs = crate::mcp::http::parse_sse_data(body);
    assert_eq!(msgs.len(), 2);
    assert!(msgs[0].is_response());
}

// ---------------------------------------------------------------------------
// manager: parallel connect, naming, isolation, unknown server
// ---------------------------------------------------------------------------

#[test]
fn fully_qualified_naming_roundtrips() {
    let fq = fully_qualified_tool_name("memory", "create_entities");
    assert_eq!(fq, "mcp__memory__create_entities");
    let (server, tool) = parse_tool_name(&fq).unwrap();
    assert_eq!(server, "memory");
    assert_eq!(tool, "create_entities");
    // tool half may contain the delimiter; the server is the first segment.
    let (s2, t2) = parse_tool_name("mcp__srv__sub__tool").unwrap();
    assert_eq!(s2, "srv");
    assert_eq!(t2, "sub__tool");
    // non-MCP names do not parse.
    assert!(parse_tool_name("plain_name").is_none());
    assert!(parse_tool_name("mcp__only").is_none());
}

#[test]
fn manager_connects_two_servers_in_parallel_and_isolates_failures() {
    let (_dir, script) = write_stdio_fixture();
    let mut configs: HashMap<String, McpServerConfig> = HashMap::new();
    configs.insert("alpha".to_string(), stdio_config(&script));
    configs.insert("beta".to_string(), stdio_config(&script));
    // A server whose command does not exist: must be isolated (others connect).
    configs.insert(
        "broken".to_string(),
        McpServerConfig {
            transport: McpServerTransport::Stdio {
                command: "this-command-does-not-exist-xyz".to_string(),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
            },
            startup_timeout_ms: Some(2_000),
            tool_timeout_ms: Some(2_000),
            enabled_tools: None,
            disabled_tools: None,
        },
    );

    let (manager, errors) = McpConnectionManager::connect_all(configs).expect("connect_all");
    assert_eq!(manager.connected_count(), 2, "alpha+beta should connect");
    assert!(
        errors.contains_key("broken"),
        "broken should be recorded: {errors:?}"
    );

    // list_all_tools is keyed by fully-qualified name across servers.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let all = rt.block_on(manager.list_all_tools());
    assert!(
        all.contains_key("mcp__alpha__echo"),
        "all tools: {:?}",
        all.keys()
    );
    assert!(all.contains_key("mcp__beta__echo"));
}

#[test]
fn manager_unknown_server_errors() {
    let (_dir, script) = write_stdio_fixture();
    let mut configs: HashMap<String, McpServerConfig> = HashMap::new();
    configs.insert("alpha".to_string(), stdio_config(&script));
    let (manager, _errors) = McpConnectionManager::connect_all(configs).expect("connect_all");

    // The sync seam must surface an Err naming the unknown server.
    let err = McpClient::call_tool(&manager, "nope", "echo", None).unwrap_err();
    assert!(
        err.to_string().contains("nope"),
        "error should name the unknown server: {err}"
    );
}

#[test]
fn manager_tool_filter_disables_tools() {
    let (_dir, script) = write_stdio_fixture();
    let mut cfg = stdio_config(&script);
    cfg.disabled_tools = Some(vec!["danger".to_string()]);
    let mut configs: HashMap<String, McpServerConfig> = HashMap::new();
    configs.insert("alpha".to_string(), cfg);
    let (manager, _errors) = McpConnectionManager::connect_all(configs).expect("connect_all");

    let rt = tokio::runtime::Runtime::new().unwrap();
    let tools = rt.block_on(manager.list_tools("alpha")).expect("list");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"echo"));
    assert!(!names.contains(&"danger"), "danger should be filtered out");

    // A disabled tool call is rejected before hitting the server.
    let err = McpClient::call_tool(&manager, "alpha", "danger", None).unwrap_err();
    assert!(err.to_string().contains("danger"));
}

// ---------------------------------------------------------------------------
// seam: manager satisfies the sync McpClient trait + handler flattens content
// ---------------------------------------------------------------------------

#[test]
fn manager_drives_seam_end_to_end() {
    let (_dir, script) = write_stdio_fixture();
    let mut configs: HashMap<String, McpServerConfig> = HashMap::new();
    configs.insert("alpha".to_string(), stdio_config(&script));
    let (manager, _errors) = McpConnectionManager::connect_all(configs).expect("connect_all");

    // Wrap the manager in McpTool::new(Arc::new(manager)) — this only compiles
    // if McpConnectionManager: McpClient (the sync seam). The handler holds it.
    let client: Arc<dyn McpClient> = Arc::new(manager);
    let _tool = McpTool::new(Arc::clone(&client));

    // Dispatch a tools/call through the sync seam and flatten via the handler's
    // public flattener (proves both the trait impl and content flattening).
    let result: McpCallResult = client
        .call_tool("alpha", "echo", Some(json!({"k": "v"})))
        .expect("seam call_tool");
    assert!(!result.is_error);
    assert_eq!(mcp_result_tool_content(&result), "called echo");
    // structuredContent passes through the seam.
    assert_eq!(result.structured_content, Some(json!({"args": {"k": "v"}})));
}

// ---------------------------------------------------------------------------
// elicitation: a server elicit request is answered with Decline
// ---------------------------------------------------------------------------

/// A python fixture that, after `initialize`, sends a server->client
/// `elicitation/create` request and then expects the client's response on its
/// stdin. It records the client's `action` and surfaces it via a `tools/call`
/// result so the test can assert it was `decline`.
const ELICIT_FIXTURE_PY: &str = r#"
import sys, json

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

last_action = None
elicited = False

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except Exception:
        continue
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": "2024-11-05", "capabilities": {}}})
    elif method == "notifications/initialized":
        # Now send a server->client elicitation request.
        send({"jsonrpc": "2.0", "id": "e1", "method": "elicitation/create",
              "params": {"message": "ok?"}})
        elicited = True
    elif method == "tools/call":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "content": [{"type": "text", "text": "action=" + str(last_action)}],
            "isError": False}})
    elif msg.get("id") == "e1" and method is None:
        # This is the client's RESPONSE to our elicitation request.
        last_action = (msg.get("result") or {}).get("action")
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": []}})
"#;

#[tokio::test]
async fn server_elicitation_is_declined() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("elicit.py");
    let mut f = std::fs::File::create(&path).expect("create");
    f.write_all(ELICIT_FIXTURE_PY.as_bytes()).expect("write");
    f.flush().expect("flush");

    let transport = StdioTransport::connect(
        "python3",
        &[path.to_string_lossy().to_string()],
        &HashMap::new(),
        None,
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .await
    .expect("connect");

    // Give the reader task a moment to receive the elicitation request and send
    // the decline, which the fixture records.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let result = transport.call_tool("whatever", None).await.expect("call");
    let text = mcp_result_tool_content(&result.into_seam());
    assert_eq!(text, "action=decline", "server should observe a decline");
}

// ---------------------------------------------------------------------------
// oauth: PKCE determinism, url building, callback parse, token cache
// ---------------------------------------------------------------------------

#[test]
fn pkce_challenge_is_sha256_base64url_nopad() {
    // Known-answer: RFC 7636 appendix B verifier/challenge.
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    assert_eq!(code_challenge_s256(verifier), expected);

    // generate_pkce is internally consistent.
    let pkce = generate_pkce();
    assert_eq!(pkce.challenge, code_challenge_s256(&pkce.verifier));
    assert!(!pkce.verifier.is_empty());
    // verifier is url-safe (no '+' '/' '=').
    assert!(!pkce.verifier.contains(['+', '/', '=']));
}

#[test]
fn authorization_url_contains_pkce_and_params() {
    let url = build_authorization_url(
        "https://idp.example/authorize",
        "client-123",
        "http://127.0.0.1:1455/callback",
        "CHALLENGE",
        "STATE",
        &["openid", "profile"],
        Some("https://api.example/mcp"),
    );
    assert!(url.starts_with("https://idp.example/authorize?"));
    assert!(url.contains("response_type=code"));
    assert!(url.contains("client_id=client-123"));
    assert!(url.contains("code_challenge=CHALLENGE"));
    assert!(url.contains("code_challenge_method=S256"));
    assert!(url.contains("state=STATE"));
    assert!(url.contains("scope=openid%20profile"));
    assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fcallback"));
    assert!(url.contains("resource=https%3A%2F%2Fapi.example%2Fmcp"));
}

#[test]
fn redirect_callback_parses_code_and_surfaces_errors() {
    assert_eq!(
        parse_redirect_callback("code=abc123&state=xyz").unwrap(),
        "abc123"
    );
    // percent-decoding of the code value.
    assert_eq!(parse_redirect_callback("code=a%20b").unwrap(), "a b");
    // an idp error is surfaced.
    let err = parse_redirect_callback("error=access_denied").unwrap_err();
    assert!(matches!(err, OauthError::InvalidCallback(_)));
    // missing code is an error.
    assert!(parse_redirect_callback("state=only").is_err());
}

#[test]
fn token_cache_roundtrips_via_tempdir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(".credentials.json");

    // Loading a missing file yields an empty store.
    let empty = OAuthTokenStore::load(&path).expect("load missing");
    assert!(empty.servers.is_empty());

    let mut store = OAuthTokenStore::default();
    store.set(
        "srv",
        StoredOAuthTokens {
            access_token: "tok-abc".to_string(),
            refresh_token: Some("refresh-xyz".to_string()),
            expires_at: None,
            token_type: Some("Bearer".to_string()),
        },
    );
    store.save(&path).expect("save");

    let loaded = OAuthTokenStore::load(&path).expect("load");
    let tokens = loaded.get("srv").expect("srv present");
    assert_eq!(tokens.access_token, "tok-abc");
    assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-xyz"));
    assert_eq!(loaded, store);
}

#[test]
fn interactive_login_is_stubbed() {
    let err = perform_interactive_login("srv").unwrap_err();
    assert!(matches!(err, OauthError::InteractiveNotWired(_)));
}
