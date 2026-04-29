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
    model::{CallToolRequestParams, Content, Tool},
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

/// RAII wrapper that kills the proxy process on drop.
struct ProxyChild(Child);

impl Drop for ProxyChild {
    fn drop(&mut self) {
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
        let bin = env!("CARGO_BIN_EXE_hyper-mcp-proxy");
        let child = Command::new(bin)
            .args([
                "--host",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "--endpoint",
                "/mcp",
                "--",
                "sentrux",
                "--mcp",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to start proxy binary");

        let child = ProxyChild(child);

        let uri = format!("http://127.0.0.1:{}/mcp", port);

        // Wait for the proxy to be ready at the TCP level before the rmcp
        // client transport tries to connect.
        wait_until_ready(port, Duration::from_secs(5)).await;

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
    fn server_info(&self) -> &rmcp::model::InitializeResult {
        self.client
            .peer_info()
            .expect("peer_info should be available after initialize")
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Find a free TCP port on localhost by briefly binding to port 0.
fn free_port() -> u16 {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("failed to bind ephemeral port");
    listener.local_addr().unwrap().port()
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
fn first_text(content: &[Content]) -> &str {
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
        info.server_info.name, "hyper-streamable-http-proxy",
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
