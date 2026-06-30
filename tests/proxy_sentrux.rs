//! Integration tests for hyper-mcp-proxy running in front of the Sentrux
//! stdio MCP server (`sentrux --mcp`).
//!
//! Each test spins up its own proxy instance on a unique port, connects
//! via the rmcp [`StreamableHttpClientTransport`] (the same transport a
//! real MCP client would use), and exercises the protocol end-to-end.
//! This validates that the proxy speaks correct MCP — not just
//! well-formed HTTP.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, ContentBlock, Tool},
    service::RunningService,
    transport::streamable_http_client::StreamableHttpClientTransport,
};

// ---------------------------------------------------------------------------
// Test fixture
// ---------------------------------------------------------------------------

/// A running proxy process together with an rmcp client session connected
/// to it.
struct ProxyFixture {
    /// The proxy child process — killed on drop.
    _child: ProxyChild,
    /// The rmcp client service connected to the proxy.
    client: RunningService<rmcp::service::RoleClient, ()>,
}

/// RAII wrapper that terminates the proxy process on drop.
///
/// On Unix we send `SIGTERM` and give the proxy a short grace period to
/// exit cleanly via its `axum::serve(...).with_graceful_shutdown(...)`
/// handler. This matters for two reasons:
///   1. It exercises the real production shutdown path end-to-end.
///   2. Under `cargo llvm-cov`, the LLVM profile runtime only flushes
///      coverage data on graceful exit, so `SIGKILL` would discard all
///      subprocess coverage. We fall back to `SIGKILL` if the proxy
///      hasn't exited after the grace window.
struct ProxyChild(Child);

impl Drop for ProxyChild {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let pid = self.0.id();
            // SAFETY: `pid` came from a live `Child` we own. `SIGTERM`
            // is a request to terminate and has no memory-safety
            // implications; the worst case is the call returns -1
            // because the process has already exited, which we ignore.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
            // Poll for graceful exit. 3s is generous on local hardware
            // and still well under typical CI test timeouts.
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while std::time::Instant::now() < deadline {
                match self.0.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                    Err(_) => break,
                }
            }
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl ProxyFixture {
    /// Start the proxy on `port`, pointing at `sentrux --mcp`, then
    /// connect an rmcp client to it.
    ///
    /// This performs the full MCP initialization handshake via the rmcp
    /// client transport, which validates the proxy's protocol compliance
    /// at the transport level.
    async fn start(port: u16) -> Self {
        let child = spawn_proxy_process(port, &[]).await;
        let uri = format!("http://127.0.0.1:{}/mcp", port);

        // Connect to the proxy using the rmcp streamable-HTTP client
        // transport. `().serve(transport)` uses the default (no-op)
        // ClientHandler, which is fine — we are only driving the client
        // side here.  The `.await` on `serve` performs the full
        // initialize / initialized handshake.
        let client = connect_with_retry(&uri, 5).await;

        Self {
            _child: child,
            client,
        }
    }

    /// Convenience accessor for the peer handle.
    fn peer(&self) -> &rmcp::service::Peer<rmcp::service::RoleClient> {
        self.client.peer()
    }

    /// Return the server info advertised by the proxy during initialize.
    fn server_info(&self) -> std::sync::Arc<rmcp::model::InitializeResult> {
        self.client
            .peer_info()
            .expect("peer_info should be available after initialize")
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// MCP protocol version advertised by the test client.
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// Find a free TCP port on localhost by briefly binding to port 0.
fn free_port() -> u16 {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("failed to bind ephemeral port");
    listener.local_addr().unwrap().port()
}

/// Spawn the proxy binary in front of `sentrux --mcp` on the given port and
/// wait until it accepts TCP connections. Returns an RAII handle that kills
/// the child on drop.
///
/// `extra_proxy_args` is appended to the proxy's argv *before* the `--`
/// separator, so callers can pass flags like `--session-keep-alive`.
async fn spawn_proxy_process(port: u16, extra_proxy_args: &[&str]) -> ProxyChild {
    let bin = env!("CARGO_BIN_EXE_hyper-mcp-proxy");
    let port_str = port.to_string();
    let mut args: Vec<&str> = vec![
        "--host",
        "127.0.0.1",
        "--port",
        &port_str,
        "--endpoint",
        "/mcp",
    ];
    args.extend_from_slice(extra_proxy_args);
    args.extend_from_slice(&["--", "sentrux", "--mcp"]);

    let child = Command::new(bin)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to start proxy binary");

    let child = ProxyChild(child);
    wait_until_ready(port, Duration::from_secs(5)).await;
    child
}

/// Poll until `127.0.0.1:port` accepts a TCP connection, or panic after
/// `timeout`.
async fn wait_until_ready(port: u16, timeout: Duration) {
    let addr = format!("127.0.0.1:{}", port);
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "proxy did not become ready within {} ms",
                timeout.as_millis()
            );
        }
        match tokio::net::TcpStream::connect(&addr).await {
            Ok(_) => return,
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// Try to connect the rmcp client, retrying on transient connection resets
/// that can happen when the TCP listener is up but the HTTP server has not
/// fully initialised its handler.
///
/// A fresh [`StreamableHttpClientTransport`] is created for each attempt
/// because `serve` consumes the transport.
async fn connect_with_retry(
    uri: &str,
    max_retries: u32,
) -> RunningService<rmcp::service::RoleClient, ()> {
    let mut last_err: Option<String> = None;

    for attempt in 0..max_retries {
        let transport = StreamableHttpClientTransport::from_uri(uri);

        match ().serve(transport).await {
            Ok(client) => return client,
            Err(e) => {
                last_err = Some(format!("{e}"));
                if attempt < max_retries - 1 {
                    eprintln!(
                        "retrying rmcp connect (attempt {}/{}): {e}",
                        attempt + 1,
                        max_retries
                    );
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            }
        }
    }

    panic!(
        "failed to connect rmcp client after {max_retries} attempts: {}",
        last_err.unwrap_or_default()
    );
}

/// Extract the text from the first content item of a `CallToolResult`.
fn first_text(content: &[ContentBlock]) -> &str {
    content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.as_str())
        .expect("expected at least one text content item")
}

/// Collect tool names from a list of tools.
fn tool_names(tools: &[Tool]) -> Vec<&str> {
    tools.iter().map(|t| t.name.as_ref()).collect()
}

/// Build a `CallToolRequestParams` with a JSON object argument map.
fn call_tool_with_args(
    name: &str,
    args: serde_json::Map<String, serde_json::Value>,
) -> CallToolRequestParams {
    CallToolRequestParams::new(name.to_string()).with_arguments(args)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_initialize_returns_proxy_server_info() {
    let proxy = ProxyFixture::start(free_port()).await;
    let info = proxy.server_info();

    // The proxy rewrites server_info to its own identity.
    assert_eq!(
        info.server_info.name, "hyper-mcp-proxy",
        "expected proxy's own server name, got: {}",
        info.server_info.name
    );

    // Version should match the proxy crate version.
    assert_eq!(
        info.server_info.version,
        env!("CARGO_PKG_VERSION"),
        "server version should match the proxy crate version"
    );
}

#[tokio::test]
async fn test_initialize_advertises_tools_capability() {
    let proxy = ProxyFixture::start(free_port()).await;
    let info = proxy.server_info();

    assert!(
        info.capabilities.tools.is_some(),
        "capabilities should include tools (sentrux exposes tools)"
    );
}

#[tokio::test]
async fn test_list_tools_returns_sentrux_tools() {
    let proxy = ProxyFixture::start(free_port()).await;

    let tools = proxy
        .peer()
        .list_all_tools()
        .await
        .expect("list_all_tools should succeed");

    assert!(!tools.is_empty(), "sentrux should expose at least one tool");

    let names = tool_names(&tools);
    assert!(
        names.contains(&"scan"),
        "expected 'scan' tool, got: {names:?}"
    );
    assert!(
        names.contains(&"health"),
        "expected 'health' tool, got: {names:?}"
    );
}

#[tokio::test]
async fn test_list_tools_returns_expected_sentrux_set() {
    let proxy = ProxyFixture::start(free_port()).await;

    let tools = proxy
        .peer()
        .list_all_tools()
        .await
        .expect("list_all_tools should succeed");

    let names = tool_names(&tools);

    // Sentrux should expose at minimum these core tools.
    for expected in ["scan", "rescan", "health", "session_start", "session_end"] {
        assert!(
            names.contains(&expected),
            "expected '{expected}' tool, got: {names:?}"
        );
    }
}

#[tokio::test]
async fn test_tools_have_valid_input_schemas() {
    let proxy = ProxyFixture::start(free_port()).await;

    let tools = proxy
        .peer()
        .list_all_tools()
        .await
        .expect("list_all_tools should succeed");

    for tool in &tools {
        let schema = &tool.input_schema;
        // Every tool's inputSchema must be a JSON object with type "object"
        // and a "properties" key.
        assert_eq!(
            schema.get("type").and_then(|v| v.as_str()),
            Some("object"),
            "tool '{}' inputSchema.type should be 'object', got: {:?}",
            tool.name,
            schema.get("type")
        );
        assert!(
            schema.get("properties").is_some(),
            "tool '{}' inputSchema should have 'properties'",
            tool.name
        );
    }
}

#[tokio::test]
async fn test_call_tool_scan_with_valid_path() {
    let proxy = ProxyFixture::start(free_port()).await;

    // Use the project directory itself as the scan target — it's
    // guaranteed to exist.
    let project_dir = env!("CARGO_MANIFEST_DIR");

    let mut args = serde_json::Map::new();
    args.insert("path".to_string(), serde_json::json!(project_dir));

    let result = proxy
        .peer()
        .call_tool(call_tool_with_args("scan", args))
        .await
        .expect("call_tool(scan) should succeed");

    assert!(
        result.is_error != Some(true),
        "scan should not return an error: {:?}",
        result.content
    );
    assert!(
        !result.content.is_empty(),
        "scan should return non-empty content"
    );

    let text = first_text(&result.content);
    assert!(
        text.contains("quality_signal"),
        "scan output should mention quality_signal, got: {text}"
    );
}

#[tokio::test]
async fn test_call_tool_health_after_scan() {
    let proxy = ProxyFixture::start(free_port()).await;
    let project_dir = env!("CARGO_MANIFEST_DIR");

    // First scan so sentrux has data loaded.
    let mut scan_args = serde_json::Map::new();
    scan_args.insert("path".to_string(), serde_json::json!(project_dir));

    let scan_result = proxy
        .peer()
        .call_tool(call_tool_with_args("scan", scan_args))
        .await
        .expect("scan should succeed");
    assert!(scan_result.is_error != Some(true), "scan returned error");

    // Now call health.
    let health_result = proxy
        .peer()
        .call_tool(CallToolRequestParams::new("health"))
        .await
        .expect("call_tool(health) should succeed");

    assert!(
        health_result.is_error != Some(true),
        "health should not return an error: {:?}",
        health_result.content
    );

    let text = first_text(&health_result.content);

    // Health output should mention the five root-cause metrics.
    for keyword in [
        "modularity",
        "acyclicity",
        "depth",
        "equality",
        "redundancy",
    ] {
        assert!(
            text.to_lowercase().contains(keyword),
            "health output should mention '{keyword}', got: {text}"
        );
    }
}

#[tokio::test]
async fn test_call_tool_with_invalid_name_returns_error() {
    let proxy = ProxyFixture::start(free_port()).await;

    let result = proxy
        .peer()
        .call_tool(CallToolRequestParams::new("nonexistent_tool_xyz"))
        .await;

    // Should either be a transport/protocol-level error or a tool result
    // with is_error = true.
    match result {
        Err(_) => { /* protocol-level error — acceptable */ }
        Ok(r) => {
            assert!(
                r.is_error == Some(true),
                "calling a nonexistent tool should produce is_error=true, got: {:?}",
                r
            );
        }
    }
}

#[tokio::test]
async fn test_multiple_requests_on_same_session() {
    let proxy = ProxyFixture::start(free_port()).await;

    // First request: list tools.
    let tools1 = proxy
        .peer()
        .list_all_tools()
        .await
        .expect("first list_all_tools should succeed");

    // Second request: list tools again — should get the same result from
    // the same underlying child process session.
    let tools2 = proxy
        .peer()
        .list_all_tools()
        .await
        .expect("second list_all_tools should succeed");

    assert_eq!(
        tools1.len(),
        tools2.len(),
        "repeated list_all_tools should return the same number of tools"
    );

    let names1 = tool_names(&tools1);
    let names2 = tool_names(&tools2);
    assert_eq!(
        names1, names2,
        "tool names should be identical across calls"
    );
}

/// Read chunks from `resp` until `marker` appears in the accumulated body,
/// the body ends, or `timeout` elapses. Returns whatever was accumulated.
///
/// rmcp's streamable HTTP server prepends a priming SSE event (`data:\nid:
/// 0\nretry: 3000`) before the actual JSON-RPC response, so we can't just
/// stop at the first `\n\n` event terminator — we have to keep reading
/// until the *content* event arrives. The `marker` is the substring that
/// identifies a successful response (e.g. `"result"`).
async fn read_sse_until_marker(
    mut resp: reqwest::Response,
    marker: &str,
    timeout: Duration,
) -> String {
    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline - now;
        match tokio::time::timeout(remaining, resp.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                buf.push_str(&String::from_utf8_lossy(&chunk));
                if buf.contains(marker) {
                    break;
                }
            }
            // Body ended, body errored, or per-chunk timeout — stop.
            Ok(Ok(None)) | Ok(Err(_)) | Err(_) => break,
        }
    }
    buf
}

/// End-to-end test for the `--session-keep-alive` CLI flag.
///
/// Spawns the proxy with a 1-second idle timeout, completes the initialize
/// handshake, sleeps past the timeout, then sends `tools/list` with the
/// stale session id. Per the MCP spec (and our `rmcp_session_lifecycle.rs`
/// tests) the server must respond with 404 once the session has been
/// reaped. This test pins that the CLI flag actually plumbs through to
/// `LocalSessionManager.session_config.keep_alive`.
#[tokio::test]
async fn test_session_keep_alive_flag_reaps_idle_session() {
    let port = free_port();
    let _proxy = spawn_proxy_process(port, &["--session-keep-alive", "1"]).await;
    let url = format!("http://127.0.0.1:{}/mcp", port);

    // Step 1: initialize and capture the session id.
    let init_client = reqwest::Client::new();
    let init_resp = init_client
        .post(&url)
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "keep-alive-test", "version": "0.0.0" }
            }
        }))
        .send()
        .await
        .expect("initialize POST should succeed at the HTTP layer");

    assert_eq!(
        init_resp.status(),
        reqwest::StatusCode::OK,
        "initialize should return 200"
    );
    let session_id = init_resp
        .headers()
        .get("mcp-session-id")
        .expect("initialize response must include Mcp-Session-Id")
        .to_str()
        .expect("session id must be ASCII")
        .to_string();
    assert!(!session_id.is_empty(), "session id must not be empty");

    // Drain + drop so the connection pool is gone.
    let _ = init_resp.bytes().await;
    drop(init_client);

    // Step 2: sleep past the configured 1s idle timeout. 2.5s gives the
    // worker's keep_alive_timeout, the WorkerTransport teardown, and the
    // `close_session` cleanup in `spawn_session_worker` all plenty of
    // headroom on a loaded CI machine.
    tokio::time::sleep(Duration::from_millis(2500)).await;

    // Step 3: tools/list with the stale session id.
    let resume_client = reqwest::Client::new();
    let resp = resume_client
        .post(&url)
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .header("Mcp-Session-Id", &session_id)
        .header("MCP-Protocol-Version", MCP_PROTOCOL_VERSION)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }))
        .send()
        .await
        .expect("tools/list POST should succeed at the HTTP layer");

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();

    assert_eq!(
        status,
        reqwest::StatusCode::NOT_FOUND,
        "after --session-keep-alive 1s elapses, a stale session id should \
         return 404, got: {status} (body: {body})"
    );
    assert!(
        body.to_lowercase().contains("not found"),
        "expected 404 body to mention 'not found', got: {body}"
    );
    assert!(
        body.to_lowercase().contains("session"),
        "expected 404 body to mention the session, got: {body}"
    );
}

#[tokio::test]
async fn test_session_resumption_after_sse_close() {
    // This test exercises MCP streamable-HTTP session resumption:
    //
    //   1. Open a fresh TCP/HTTP client and POST `initialize` — capture the
    //      `Mcp-Session-Id` the server assigns in the response header.
    //   2. Drop the response (closing the SSE socket) AND drop the entire
    //      reqwest client so the underlying connection pool is gone.
    //   3. With a brand-new reqwest client (i.e. a brand-new TCP
    //      connection), POST `tools/list` with the captured session id.
    //
    // The session — and therefore the proxy's `ProxyHandler` and its
    // child stdio process — must still be alive on the server side, even
    // though the SSE stream from initialize was torn down. If the proxy
    // were tearing the child down when the HTTP stream closed, step 3
    // would either 404 (`Session not found`) or fail with
    // `proxy session not initialized`.

    let port = free_port();
    let _proxy = spawn_proxy_process(port, &[]).await;
    let url = format!("http://127.0.0.1:{}/mcp", port);

    // -- Step 1: initialize with a one-shot client --------------------------
    let init_client = reqwest::Client::new();
    let init_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "session-resumption-test",
                "version": "0.0.0"
            }
        }
    });

    let init_resp = init_client
        .post(&url)
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .json(&init_body)
        .send()
        .await
        .expect("initialize POST should succeed at the HTTP layer");

    assert_eq!(
        init_resp.status(),
        reqwest::StatusCode::OK,
        "initialize should return 200, got: {}",
        init_resp.status()
    );

    let session_id = init_resp
        .headers()
        .get("mcp-session-id")
        .expect("initialize response must include the Mcp-Session-Id header")
        .to_str()
        .expect("session id should be ASCII")
        .to_string();
    assert!(
        !session_id.is_empty(),
        "session id from response header must not be empty"
    );

    // Read just enough of the SSE body to confirm we got a valid
    // InitializeResult, then drop the response — this is the "SSE socket
    // close" the test simulates.
    let init_event =
        read_sse_until_marker(init_resp, "\"protocolVersion\"", Duration::from_secs(5)).await;
    assert!(
        init_event.contains("\"result\""),
        "initialize SSE event should contain a JSON-RPC result, got: {init_event}"
    );
    assert!(
        init_event.contains("\"protocolVersion\""),
        "initialize result should advertise a protocolVersion, got: {init_event}"
    );

    // Tear down the original client and its connection pool entirely. This
    // guarantees that the next request opens a brand-new TCP connection
    // and is treated as a resumed session by the server, not as a
    // continuation of the original keep-alive connection.
    drop(init_client);

    // -- Step 2: send the initialized notification on a new connection ----
    //
    // Per the MCP lifecycle, the client signals readiness with
    // `notifications/initialized` before issuing further requests. This
    // also serves as a quick liveness check on the resumed session: if
    // the session were gone we'd get a 404 here, not a 202.
    let resume_client = reqwest::Client::new();
    let initialized_notif = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let notif_resp = resume_client
        .post(&url)
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .header("Mcp-Session-Id", &session_id)
        .header("MCP-Protocol-Version", MCP_PROTOCOL_VERSION)
        .json(&initialized_notif)
        .send()
        .await
        .expect("initialized notification POST should succeed at the HTTP layer");

    assert!(
        notif_resp.status().is_success(),
        "initialized notification on resumed session should return 2xx, got: {} (body: {})",
        notif_resp.status(),
        notif_resp.text().await.unwrap_or_default(),
    );

    // -- Step 3: tools/list on the resumed session ------------------------
    let tools_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });

    let tools_resp = resume_client
        .post(&url)
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .header("Mcp-Session-Id", &session_id)
        .header("MCP-Protocol-Version", MCP_PROTOCOL_VERSION)
        .json(&tools_body)
        .send()
        .await
        .expect("tools/list POST should succeed at the HTTP layer");

    let status = tools_resp.status();
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "tools/list on resumed session should return 200, got: {status} (body: {})",
        tools_resp.text().await.unwrap_or_default(),
    );

    let tools_event = read_sse_until_marker(tools_resp, "\"tools\"", Duration::from_secs(10)).await;

    // Sanity: it's the response to id=2, contains a `tools` array, and
    // mentions at least one well-known sentrux tool.
    assert!(
        tools_event.contains("\"id\":2") || tools_event.contains("\"id\": 2"),
        "tools/list SSE event should be the response to request id=2, got: {tools_event}"
    );
    assert!(
        tools_event.contains("\"tools\""),
        "tools/list SSE event should contain a `tools` array, got: {tools_event}"
    );
    assert!(
        tools_event.contains("scan") || tools_event.contains("health"),
        "tools/list on resumed session should include at least one sentrux tool, got: {tools_event}"
    );
}

#[tokio::test]
async fn test_scan_then_rescan_succeeds() {
    let proxy = ProxyFixture::start(free_port()).await;
    let project_dir = env!("CARGO_MANIFEST_DIR");

    // Initial scan.
    let mut scan_args = serde_json::Map::new();
    scan_args.insert("path".to_string(), serde_json::json!(project_dir));

    let scan_result = proxy
        .peer()
        .call_tool(call_tool_with_args("scan", scan_args))
        .await
        .expect("scan should succeed");
    assert!(scan_result.is_error != Some(true));

    // Rescan — should work now that a directory has been scanned.
    let rescan_result = proxy
        .peer()
        .call_tool(CallToolRequestParams::new("rescan"))
        .await
        .expect("call_tool(rescan) should succeed");

    assert!(
        rescan_result.is_error != Some(true),
        "rescan should not return an error: {:?}",
        rescan_result.content
    );

    let text = first_text(&rescan_result.content);
    assert!(
        text.contains("quality_signal"),
        "rescan output should mention quality_signal, got: {text}"
    );
}
