//! Full MCP-protocol integration tests for hyper-mcp-proxy.
//!
//! Unlike `proxy_sentrux.rs`, these tests use the
//! [`mock-mcp-child`](support/mock_mcp_child.rs) binary as the proxy's
//! stdio child. The mock implements *every* MCP method (resources,
//! prompts, completion, sampling, elicitation, logging level, …) plus
//! a couple of trigger tools that emit every supported
//! child → client notification and request.
//!
//! The goal is coverage of the proxy's forwarding code, not the mock's
//! behaviour. Each test asserts a single proxy responsibility:
//!
//!   * Method `X` proxies through with a response that matches the
//!     mock's hardcoded output, OR
//!   * Notification `Y` emitted by the child reaches a custom
//!     `ClientHandler` on the client side, OR
//!   * Request `Z` initiated by the child round-trips through the
//!     proxy and is fulfilled by the client.
//!
//! Together these cover every `match`-arm of the proxy's
//! `ServerHandler` and `ClientHandler` impls.

use std::collections::BTreeMap;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::{
    ClientHandler, ServiceExt,
    model::{
        CallToolRequestParams, CancelledNotificationParam, CompleteRequestParams,
        CreateElicitationRequestParams, CreateElicitationResult, CreateMessageRequestParams,
        CreateMessageResult, ElicitationAction, GetPromptRequestParams, ListRootsResult,
        LoggingMessageNotificationParam, ProgressNotificationParam, PromptReference, Reference,
        ResourceUpdatedNotificationParam, Role, SamplingMessage, SetLevelRequestParams,
        SubscribeRequestParams, UnsubscribeRequestParams,
    },
    service::{NotificationContext, RequestContext, RoleClient, RunningService},
    transport::streamable_http_client::StreamableHttpClientTransport,
};

// ---------------------------------------------------------------------------
// Recording client handler
// ---------------------------------------------------------------------------

/// Tracks every notification and request that flows *from* the child *to*
/// the test client. Used by the tests to assert the proxy forwarded the
/// expected callbacks.
///
/// All fields use `Arc<Mutex<...>>` because rmcp clones the handler into
/// background tasks; the test driver needs to see writes made by those
/// tasks.
#[derive(Clone, Default)]
struct RecordingClient {
    notifications: Arc<Mutex<Vec<String>>>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl RecordingClient {
    fn record_notification(&self, name: &str) {
        self.notifications.lock().unwrap().push(name.to_string());
    }

    fn record_request(&self, name: &str) {
        self.requests.lock().unwrap().push(name.to_string());
    }

    fn notifications(&self) -> Vec<String> {
        self.notifications.lock().unwrap().clone()
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl ClientHandler for RecordingClient {
    // -- Requests (server -> client) ---------------------------------------

    async fn create_message(
        &self,
        _params: CreateMessageRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<CreateMessageResult, rmcp::ErrorData> {
        self.record_request("create_message");
        Ok(CreateMessageResult::new(
            SamplingMessage::new(Role::Assistant, "mock reply"),
            "mock-model".to_string(),
        ))
    }

    async fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, rmcp::ErrorData> {
        self.record_request("list_roots");
        Ok(ListRootsResult::new(vec![]))
    }

    async fn create_elicitation(
        &self,
        _request: CreateElicitationRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<CreateElicitationResult, rmcp::ErrorData> {
        self.record_request("create_elicitation");
        Ok(CreateElicitationResult::new(ElicitationAction::Cancel))
    }

    // -- Notifications (server -> client) ----------------------------------

    async fn on_cancelled(
        &self,
        _params: CancelledNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.record_notification("cancelled");
    }

    async fn on_progress(
        &self,
        _params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.record_notification("progress");
    }

    async fn on_logging_message(
        &self,
        _params: LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.record_notification("logging_message");
    }

    async fn on_resource_updated(
        &self,
        _params: ResourceUpdatedNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.record_notification("resource_updated");
    }

    async fn on_resource_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.record_notification("resource_list_changed");
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.record_notification("tool_list_changed");
    }

    async fn on_prompt_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.record_notification("prompt_list_changed");
    }
}

// ---------------------------------------------------------------------------
// Fixture (mirrors proxy_sentrux::ProxyFixture but launches mock-mcp-child)
// ---------------------------------------------------------------------------

struct Fixture {
    _child: ProxyChild,
    client: RunningService<RoleClient, RecordingClient>,
    recorder: RecordingClient,
}

struct ProxyChild(Child);

impl Drop for ProxyChild {
    fn drop(&mut self) {
        // SAFETY: PID came from a Child we own; SIGTERM has no
        // memory-safety implications and tolerates a dead process.
        unsafe {
            libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM);
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            match self.0.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => break,
            }
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Fixture {
    async fn start() -> Self {
        let port = free_port();
        let mock_bin = env!("CARGO_BIN_EXE_mock-mcp-child");
        let proxy_bin = env!("CARGO_BIN_EXE_hyper-mcp-proxy");
        let port_str = port.to_string();

        let child = Command::new(proxy_bin)
            .args([
                "--host",
                "127.0.0.1",
                "--port",
                &port_str,
                "--endpoint",
                "/mcp",
                "--",
                mock_bin,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to start proxy binary");
        let child = ProxyChild(child);

        wait_until_ready(port, Duration::from_secs(5)).await;

        let uri = format!("http://127.0.0.1:{port}/mcp");
        let recorder = RecordingClient::default();
        let client = connect_with_retry(&uri, recorder.clone(), 5).await;

        Self {
            _child: child,
            client,
            recorder,
        }
    }
}

fn free_port() -> u16 {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("failed to bind ephemeral port");
    listener.local_addr().unwrap().port()
}

async fn wait_until_ready(port: u16, timeout: Duration) {
    let addr = format!("127.0.0.1:{port}");
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if tokio::net::TcpStream::connect(&addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "proxy did not become ready within {} ms",
        timeout.as_millis()
    );
}

async fn connect_with_retry(
    uri: &str,
    handler: RecordingClient,
    max_retries: u32,
) -> RunningService<RoleClient, RecordingClient> {
    let mut last_err: Option<String> = None;
    for attempt in 0..max_retries {
        let transport = StreamableHttpClientTransport::from_uri(uri);
        match handler.clone().serve(transport).await {
            Ok(client) => return client,
            Err(e) => {
                last_err = Some(format!("{e}"));
                if attempt < max_retries - 1 {
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

// ---------------------------------------------------------------------------
// Server-method forwarding tests (client -> child)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_resources_is_proxied() {
    let f = Fixture::start().await;
    let result = f
        .client
        .peer()
        .list_resources(None)
        .await
        .expect("list_resources should succeed");
    assert_eq!(result.resources.len(), 1);
    assert_eq!(result.resources[0].name, "mock-resource");
}

#[tokio::test]
async fn list_resource_templates_is_proxied() {
    let f = Fixture::start().await;
    let result = f
        .client
        .peer()
        .list_resource_templates(None)
        .await
        .expect("list_resource_templates should succeed");
    assert_eq!(result.resource_templates.len(), 1);
    assert_eq!(
        result.resource_templates[0].uri_template,
        "mock://resources/{id}"
    );
}

#[tokio::test]
async fn read_resource_is_proxied() {
    let f = Fixture::start().await;
    let result = f
        .client
        .peer()
        .read_resource(rmcp::model::ReadResourceRequestParams::new(
            "mock://resources/0",
        ))
        .await
        .expect("read_resource should succeed");
    assert_eq!(result.contents.len(), 1);
}

#[tokio::test]
async fn subscribe_and_unsubscribe_are_proxied() {
    let f = Fixture::start().await;
    f.client
        .peer()
        .subscribe(SubscribeRequestParams::new("mock://resources/0"))
        .await
        .expect("subscribe should succeed");
    f.client
        .peer()
        .unsubscribe(UnsubscribeRequestParams::new("mock://resources/0"))
        .await
        .expect("unsubscribe should succeed");
}

#[tokio::test]
async fn list_prompts_is_proxied() {
    let f = Fixture::start().await;
    let result = f
        .client
        .peer()
        .list_prompts(None)
        .await
        .expect("list_prompts should succeed");
    assert_eq!(result.prompts.len(), 1);
    assert_eq!(result.prompts[0].name, "mock-prompt");
}

#[tokio::test]
async fn get_prompt_is_proxied() {
    let f = Fixture::start().await;
    let result = f
        .client
        .peer()
        .get_prompt(GetPromptRequestParams::new("mock-prompt"))
        .await
        .expect("get_prompt should succeed");
    assert_eq!(result.description.as_deref(), Some("rendered mock prompt"));
    assert_eq!(result.messages.len(), 1);
}

#[tokio::test]
async fn complete_is_proxied() {
    let f = Fixture::start().await;
    let result = f
        .client
        .peer()
        .complete(CompleteRequestParams::new(
            Reference::Prompt(PromptReference::new("mock-prompt")),
            rmcp::model::ArgumentInfo {
                name: "x".into(),
                value: "".into(),
            },
        ))
        .await
        .expect("complete should succeed");
    assert_eq!(
        result.completion.values,
        vec!["mock-completion".to_string()]
    );
}

// Logging is deprecated by SEP-2577 (advisory only, no replacement API), but
// the proxy still forwards set_level, so the test must exercise it.
#[allow(deprecated)]
#[tokio::test]
async fn set_level_is_proxied() {
    let f = Fixture::start().await;
    f.client
        .peer()
        .set_level(SetLevelRequestParams::new(rmcp::model::LoggingLevel::Debug))
        .await
        .expect("set_level should succeed");
}

// ---------------------------------------------------------------------------
// Child -> client notification forwarding
// ---------------------------------------------------------------------------

#[tokio::test]
async fn child_notifications_are_forwarded_to_client() {
    let f = Fixture::start().await;

    // Tell the mock child to emit every notification type.
    f.client
        .peer()
        .call_tool(
            CallToolRequestParams::new("trigger_notifications".to_string())
                .with_arguments(serde_json::Map::new()),
        )
        .await
        .expect("trigger_notifications should succeed");

    // Notifications are emitted asynchronously; give them a moment to
    // propagate through proxy -> SSE channel -> rmcp client dispatcher.
    let expected = [
        "progress",
        "logging_message",
        "resource_updated",
        "resource_list_changed",
        "tool_list_changed",
        "prompt_list_changed",
        "cancelled",
    ];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let got = f.recorder.notifications();
        if expected.iter().all(|name| got.iter().any(|g| g == name)) {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("missing expected notifications. expected each of {expected:?}, got {got:?}",);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Child -> client request forwarding
// ---------------------------------------------------------------------------

#[tokio::test]
async fn child_requests_are_forwarded_to_client() {
    let f = Fixture::start().await;

    // The mock awaits each server->client request before returning, so by
    // the time this call completes the round-trip is done.
    f.client
        .peer()
        .call_tool(
            CallToolRequestParams::new("trigger_server_requests".to_string())
                .with_arguments(serde_json::Map::new()),
        )
        .await
        .expect("trigger_server_requests should succeed");

    let got = f.recorder.requests();
    for name in ["create_message", "list_roots", "create_elicitation"] {
        assert!(
            got.iter().any(|g| g == name),
            "expected {name} to be recorded, got {got:?}",
        );
    }
}

// Suppress unused-import / dead-code warnings for items pulled in only by
// some test combinations on some toolchains.
#[allow(dead_code)]
fn _btreemap_marker() -> BTreeMap<String, String> {
    BTreeMap::new()
}
