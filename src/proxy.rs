use std::sync::Arc;

use rmcp::{
    ClientHandler, ErrorData, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResult, CancelledNotificationParam, CompleteRequestParams,
        CompleteResult, CreateElicitationRequestParams, CreateElicitationResult,
        CreateMessageRequestParams, CreateMessageResult, CustomNotification, CustomRequest,
        CustomResult, ElicitationResponseNotificationParam, ErrorCode, Extensions,
        GetPromptRequestParams, GetPromptResult, Implementation, InitializeRequestParams,
        InitializeResult, ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult,
        ListRootsResult, ListToolsResult, LoggingMessageNotificationParam, PaginatedRequestParams,
        ProgressNotificationParam, ReadResourceRequestParams, ReadResourceResult,
        ResourceUpdatedNotificationParam, ServerCapabilities, SetLevelRequestParams,
        SubscribeRequestParams, UnsubscribeRequestParams,
    },
    service::{NotificationContext, Peer, RequestContext, RoleClient, RoleServer},
    transport::child_process::TokioChildProcess,
};
use tokio::sync::OnceCell;
use tracing::{Span, field};

/// HTTP header that carries the MCP session ID on every post-`initialize` request.
const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";

/// Extract the MCP session ID from a request's HTTP parts, if present.
///
/// Returns `None` on the `initialize` request itself (the client doesn't have
/// an ID yet) and for any non-HTTP transport.
fn session_id_from_extensions(extensions: &Extensions) -> Option<Arc<str>> {
    let parts = extensions.get::<http::request::Parts>()?;
    let value = parts.headers.get(MCP_SESSION_ID_HEADER)?;
    let s = value.to_str().ok()?;
    Some(Arc::from(s))
}

// ---------------------------------------------------------------------------
// ChildClientHandler — forwards child→client messages to the upstream peer
// ---------------------------------------------------------------------------

/// A [`ClientHandler`] that receives notifications and requests from the child
/// MCP server and forwards them to the upstream client via a [`Peer<RoleServer>`].
struct ChildClientHandler {
    /// Peer handle for the upstream (external) client connection.
    upstream: Peer<RoleServer>,
    /// Shared cell holding the session ID (populated by the upstream
    /// [`ProxyHandler`] on the first post-`initialize` request).
    ///
    /// Notifications flowing from the child to the client don't carry HTTP
    /// headers, so we rely on this cached value for log correlation.
    session_id: Arc<OnceCell<Arc<str>>>,
}

impl ClientHandler for ChildClientHandler {
    // -- Requests from the child server ------------------------------------

    // Sampling is deprecated by SEP-2577, but the proxy must keep forwarding it
    // to remain transparent for clients/servers that still rely on it.
    #[allow(deprecated)]
    async fn create_message(
        &self,
        params: CreateMessageRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<CreateMessageResult, ErrorData> {
        self.upstream
            .create_message(params)
            .await
            .map_err(Self::upstream_error)
    }

    // Roots is deprecated by SEP-2577, but the proxy must keep forwarding it
    // to remain transparent for clients/servers that still rely on it.
    #[allow(deprecated)]
    async fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> Result<ListRootsResult, ErrorData> {
        self.upstream
            .list_roots()
            .await
            .map_err(Self::upstream_error)
    }

    async fn create_elicitation(
        &self,
        request: CreateElicitationRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<CreateElicitationResult, ErrorData> {
        self.upstream
            .create_elicitation(request)
            .await
            .map_err(Self::upstream_error)
    }

    // -- Notifications from the child server --------------------------------

    #[tracing::instrument(skip_all, fields(session_id = field::Empty))]
    async fn on_cancelled(
        &self,
        params: CancelledNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.record_session();
        if let Err(e) = self.upstream.notify_cancelled(params).await {
            tracing::warn!(error = %e, "failed to forward cancelled notification to client");
        }
    }

    #[tracing::instrument(skip_all, fields(session_id = field::Empty))]
    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.record_session();
        if let Err(e) = self.upstream.notify_progress(params).await {
            tracing::warn!(error = %e, "failed to forward progress notification to client");
        }
    }

    // Logging is deprecated by SEP-2577, but the proxy must keep forwarding it
    // to remain transparent for clients/servers that still rely on it.
    #[allow(deprecated)]
    #[tracing::instrument(skip_all, fields(session_id = field::Empty))]
    async fn on_logging_message(
        &self,
        params: LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.record_session();
        if let Err(e) = self.upstream.notify_logging_message(params).await {
            tracing::warn!(error = %e, "failed to forward logging message to client");
        }
    }

    #[tracing::instrument(skip_all, fields(session_id = field::Empty))]
    async fn on_resource_updated(
        &self,
        params: ResourceUpdatedNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.record_session();
        if let Err(e) = self.upstream.notify_resource_updated(params).await {
            tracing::warn!(error = %e, "failed to forward resource updated notification to client");
        }
    }

    #[tracing::instrument(skip_all, fields(session_id = field::Empty))]
    async fn on_resource_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.record_session();
        if let Err(e) = self.upstream.notify_resource_list_changed().await {
            tracing::warn!(error = %e, "failed to forward resource list changed notification to client");
        }
    }

    #[tracing::instrument(skip_all, fields(session_id = field::Empty))]
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.record_session();
        if let Err(e) = self.upstream.notify_tool_list_changed().await {
            tracing::warn!(error = %e, "failed to forward tool list changed notification to client");
        }
    }

    #[tracing::instrument(skip_all, fields(session_id = field::Empty))]
    async fn on_prompt_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.record_session();
        if let Err(e) = self.upstream.notify_prompt_list_changed().await {
            tracing::warn!(error = %e, "failed to forward prompt list changed notification to client");
        }
    }

    #[tracing::instrument(skip_all, fields(session_id = field::Empty))]
    async fn on_url_elicitation_notification_complete(
        &self,
        params: ElicitationResponseNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.record_session();
        if let Err(e) = self.upstream.notify_url_elicitation_completed(params).await {
            tracing::warn!(error = %e, "failed to forward elicitation completion notification to client");
        }
    }
}

impl ChildClientHandler {
    /// Convert a service error into an [`ErrorData`] suitable for returning
    /// to the child server.
    fn upstream_error(e: impl std::fmt::Display) -> ErrorData {
        ErrorData::new(
            ErrorCode::INTERNAL_ERROR,
            format!("upstream client error: {e}"),
            None,
        )
    }

    /// Record the cached session ID on the current tracing span (if known).
    fn record_session(&self) {
        if let Some(id) = self.session_id.get() {
            Span::current().record("session_id", id.as_ref());
        }
    }
}

// ---------------------------------------------------------------------------
// ProxyHandler — the server-side handler presented to the upstream client
// ---------------------------------------------------------------------------

/// Shared state for a single proxy session, created during initialization.
struct ProxyInner {
    /// Peer handle to call methods on the child MCP server.
    peer: Peer<RoleClient>,
    /// Background task keeping the client service alive.
    _service_handle: tokio::task::JoinHandle<()>,
    /// PID of the spawned child process, when available.
    child_pid: Option<u32>,
}

/// A [`ServerHandler`] implementation that proxies all MCP operations to a
/// child stdio MCP server process.
///
/// One `ProxyHandler` is created per MCP session. On [`initialize`], it spawns
/// the configured command as a child process, connects to it as an MCP client,
/// and then forwards every subsequent request/notification through.
pub struct ProxyHandler {
    /// The command (program + args) to spawn for the child MCP server.
    command: Arc<[String]>,
    /// Lazily initialized connection to the child process.
    inner: OnceCell<ProxyInner>,
    /// The MCP session ID, learned from the first post-`initialize` request.
    ///
    /// Shared with [`ChildClientHandler`] so that notifications flowing back
    /// from the child can also be logged with the right session.
    session_id: Arc<OnceCell<Arc<str>>>,
}

impl ProxyHandler {
    /// Create a new proxy handler for the given command.
    ///
    /// The child process is **not** spawned until [`initialize`] is called.
    pub fn new(command: Arc<[String]>) -> Self {
        Self {
            command,
            inner: OnceCell::new(),
            session_id: Arc::new(OnceCell::new()),
        }
    }

    /// Get the peer to the child MCP server, or return an error if not yet initialized.
    fn peer(&self) -> Result<&Peer<RoleClient>, ErrorData> {
        self.inner.get().map(|inner| &inner.peer).ok_or_else(|| {
            ErrorData::new(
                ErrorCode::INTERNAL_ERROR,
                "proxy session not initialized",
                None,
            )
        })
    }

    /// Record the session ID and child PID on the current tracing span.
    ///
    /// On the first call that carries an `Mcp-Session-Id` header (i.e. the
    /// first request after `initialize`), the ID is cached so that subsequent
    /// requests *and* child→client notifications can all be correlated.
    fn record_session(&self, extensions: &Extensions) {
        // Cache the session ID the first time we see it on a request.
        if let Some(id) = session_id_from_extensions(extensions) {
            let _ = self.session_id.set(id);
        }
        let span = Span::current();
        if let Some(id) = self.session_id.get() {
            span.record("session_id", id.as_ref());
        }
        if let Some(pid) = self.inner.get().and_then(|inner| inner.child_pid) {
            span.record("pid", pid);
        }
    }

    /// Spawn the child process and establish an MCP client connection to it.
    ///
    /// `upstream` is the [`Peer<RoleServer>`] for the external client so that
    /// notifications and requests originating from the child can be forwarded
    /// back through it.
    ///
    /// Returns the peer handle, the child's [`InitializeResult`], a background
    /// join-handle that keeps the client service alive, and the child PID (if
    /// the platform exposed one).
    async fn spawn_child(
        &self,
        upstream: Peer<RoleServer>,
    ) -> Result<
        (
            Peer<RoleClient>,
            InitializeResult,
            tokio::task::JoinHandle<()>,
            Option<u32>,
        ),
        ErrorData,
    > {
        let program = &self.command[0];
        let args = &self.command[1..];

        tracing::info!(%program, ?args, "spawning child MCP server");

        // TokioChildProcess::new expects a tokio::process::Command
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());

        let transport = TokioChildProcess::new(cmd).map_err(|e| {
            tracing::error!(error = %e, %program, "failed to create child transport");
            ErrorData::new(
                ErrorCode::INTERNAL_ERROR,
                format!("failed to create child transport: {e}"),
                None,
            )
        })?;

        // Capture the child PID before the transport is consumed by `serve`.
        let child_pid = transport.id();
        if let Some(pid) = child_pid {
            Span::current().record("pid", pid);
            tracing::info!(pid, %program, "child MCP server process spawned");
        }

        // Connect as an MCP client to the child, using our forwarding handler
        // so that notifications / requests the child sends are relayed back to
        // the upstream client.
        let handler = ChildClientHandler {
            upstream,
            session_id: Arc::clone(&self.session_id),
        };
        let client_service = handler.serve(transport).await.map_err(|e| {
            tracing::error!(error = %e, "failed to connect to child MCP server");
            ErrorData::new(
                ErrorCode::INTERNAL_ERROR,
                format!("failed to connect to child MCP server: {e}"),
                None,
            )
        })?;

        let peer = client_service.peer().clone();

        // Get the child's capabilities directly from the initialization handshake.
        let mut init_result = client_service
            .peer_info()
            .map(|info| (*info).clone())
            .unwrap_or_else(|| InitializeResult::new(ServerCapabilities::default()));
        init_result.server_info = Implementation::new("hyper-mcp-proxy", env!("CARGO_PKG_VERSION"));

        // Keep the client service alive in a background task. Carry the
        // session ID forward so the "session ended" log line can be
        // correlated with the rest of the session.
        let session_id_for_task = Arc::clone(&self.session_id);
        let handle = tokio::spawn(async move {
            let _ = client_service.waiting().await;
            let session_id = session_id_for_task
                .get()
                .map(|s| s.as_ref().to_string())
                .unwrap_or_else(|| "<unknown>".to_string());
            tracing::info!(session_id = %session_id, pid = ?child_pid, "child MCP server session ended");
        });

        Ok((peer, init_result, handle, child_pid))
    }

    /// Helper to convert a service error into an [`ErrorData`].
    fn child_error(e: impl std::fmt::Display) -> ErrorData {
        ErrorData::new(
            ErrorCode::INTERNAL_ERROR,
            format!("child server error: {e}"),
            None,
        )
    }
}

// ---------------------------------------------------------------------------
// ServerHandler implementation — every method forwards to the child via Peer
// ---------------------------------------------------------------------------

impl ServerHandler for ProxyHandler {
    #[tracing::instrument(
        skip_all,
        fields(session_id = field::Empty, pid = field::Empty)
    )]
    async fn initialize(
        &self,
        _request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        // The `Mcp-Session-Id` header is only assigned in the *response* to
        // `initialize`, so it's usually absent here. We still try, in case a
        // future transport surfaces it earlier.
        self.record_session(&context.extensions);

        let (peer, init_result, handle, child_pid) = self.spawn_child(context.peer.clone()).await?;

        self.inner
            .set(ProxyInner {
                peer,
                _service_handle: handle,
                child_pid,
            })
            .map_err(|_| {
                ErrorData::new(
                    ErrorCode::INTERNAL_ERROR,
                    "session already initialized",
                    None,
                )
            })?;

        if let Some(pid) = child_pid {
            Span::current().record("pid", pid);
        }
        tracing::info!(pid = ?child_pid, "proxy session initialized successfully");
        Ok(init_result)
    }

    // -- Tools --------------------------------------------------------------

    #[tracing::instrument(skip_all, fields(session_id = field::Empty, pid = field::Empty))]
    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        self.record_session(&context.extensions);
        self.peer()?
            .list_tools(request)
            .await
            .map_err(Self::child_error)
    }

    #[tracing::instrument(
        skip_all,
        fields(session_id = field::Empty, pid = field::Empty, tool = %request.name)
    )]
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        self.record_session(&context.extensions);
        self.peer()?
            .call_tool(request)
            .await
            .map_err(Self::child_error)
    }

    // -- Resources ----------------------------------------------------------

    #[tracing::instrument(skip_all, fields(session_id = field::Empty, pid = field::Empty))]
    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        self.record_session(&context.extensions);
        self.peer()?
            .list_resources(request)
            .await
            .map_err(Self::child_error)
    }

    #[tracing::instrument(skip_all, fields(session_id = field::Empty, pid = field::Empty))]
    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        self.record_session(&context.extensions);
        self.peer()?
            .list_resource_templates(request)
            .await
            .map_err(Self::child_error)
    }

    #[tracing::instrument(
        skip_all,
        fields(session_id = field::Empty, pid = field::Empty, uri = %request.uri)
    )]
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        self.record_session(&context.extensions);
        self.peer()?
            .read_resource(request)
            .await
            .map_err(Self::child_error)
    }

    #[tracing::instrument(skip_all, fields(session_id = field::Empty, pid = field::Empty))]
    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        self.record_session(&context.extensions);
        self.peer()?
            .subscribe(request)
            .await
            .map_err(Self::child_error)
    }

    #[tracing::instrument(skip_all, fields(session_id = field::Empty, pid = field::Empty))]
    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        self.record_session(&context.extensions);
        self.peer()?
            .unsubscribe(request)
            .await
            .map_err(Self::child_error)
    }

    // -- Prompts ------------------------------------------------------------

    #[tracing::instrument(skip_all, fields(session_id = field::Empty, pid = field::Empty))]
    async fn list_prompts(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        self.record_session(&context.extensions);
        self.peer()?
            .list_prompts(request)
            .await
            .map_err(Self::child_error)
    }

    #[tracing::instrument(
        skip_all,
        fields(session_id = field::Empty, pid = field::Empty, name = %request.name)
    )]
    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, ErrorData> {
        self.record_session(&context.extensions);
        self.peer()?
            .get_prompt(request)
            .await
            .map_err(Self::child_error)
    }

    // -- Completions --------------------------------------------------------

    #[tracing::instrument(skip_all, fields(session_id = field::Empty, pid = field::Empty))]
    async fn complete(
        &self,
        request: CompleteRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, ErrorData> {
        self.record_session(&context.extensions);
        self.peer()?
            .complete(request)
            .await
            .map_err(Self::child_error)
    }

    // -- Logging ------------------------------------------------------------

    // Logging is deprecated by SEP-2577, but the proxy must keep forwarding it
    // to remain transparent for clients/servers that still rely on it.
    #[allow(deprecated)]
    #[tracing::instrument(skip_all, fields(session_id = field::Empty, pid = field::Empty))]
    async fn set_level(
        &self,
        request: SetLevelRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        self.record_session(&context.extensions);
        self.peer()?
            .set_level(request)
            .await
            .map_err(Self::child_error)
    }

    // -- Notifications (client → child) -------------------------------------

    #[tracing::instrument(skip_all, fields(session_id = field::Empty, pid = field::Empty))]
    async fn on_cancelled(
        &self,
        notification: CancelledNotificationParam,
        context: NotificationContext<RoleServer>,
    ) {
        self.record_session(&context.extensions);
        if let Ok(peer) = self.peer()
            && let Err(e) = peer.notify_cancelled(notification).await
        {
            tracing::warn!(error = %e, "failed to forward cancellation to child");
        }
    }

    #[tracing::instrument(skip_all, fields(session_id = field::Empty, pid = field::Empty))]
    async fn on_progress(
        &self,
        notification: ProgressNotificationParam,
        context: NotificationContext<RoleServer>,
    ) {
        self.record_session(&context.extensions);
        if let Ok(peer) = self.peer()
            && let Err(e) = peer.notify_progress(notification).await
        {
            tracing::warn!(error = %e, "failed to forward progress to child");
        }
    }

    #[tracing::instrument(skip_all, fields(session_id = field::Empty, pid = field::Empty))]
    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        self.record_session(&context.extensions);
        tracing::debug!("client sent initialized notification");
    }

    #[tracing::instrument(skip_all, fields(session_id = field::Empty, pid = field::Empty))]
    async fn on_roots_list_changed(&self, context: NotificationContext<RoleServer>) {
        self.record_session(&context.extensions);
        if let Ok(peer) = self.peer()
            && let Err(e) = peer.notify_roots_list_changed().await
        {
            tracing::warn!(error = %e, "failed to forward roots_list_changed to child");
        }
    }

    #[tracing::instrument(skip_all, fields(session_id = field::Empty, pid = field::Empty))]
    async fn on_custom_notification(
        &self,
        notification: CustomNotification,
        context: NotificationContext<RoleServer>,
    ) {
        self.record_session(&context.extensions);
        tracing::debug!(
            method = %notification.method,
            "received custom notification (cannot be proxied at typed level)"
        );
    }

    // -- Custom requests ----------------------------------------------------

    #[tracing::instrument(
        skip_all,
        fields(session_id = field::Empty, pid = field::Empty, method = %request.method)
    )]
    async fn on_custom_request(
        &self,
        request: CustomRequest,
        context: RequestContext<RoleServer>,
    ) -> Result<CustomResult, ErrorData> {
        self.record_session(&context.extensions);
        tracing::debug!("received custom request");
        Err(ErrorData::new(
            ErrorCode::METHOD_NOT_FOUND,
            format!("custom method '{}' cannot be proxied", request.method),
            None,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // -----------------------------------------------------------------------
    // ProxyHandler::new
    // -----------------------------------------------------------------------

    #[test]
    fn test_new_creates_uninitialized_handler() {
        let command: Arc<[String]> = vec!["echo".into(), "hello".into()].into();
        let handler = ProxyHandler::new(command.clone());

        assert_eq!(&*handler.command, &*command);
        assert!(
            handler.inner.get().is_none(),
            "inner must be None before initialize"
        );
    }

    #[test]
    fn test_new_with_single_command() {
        let command: Arc<[String]> = vec!["my-server".into()].into();
        let handler = ProxyHandler::new(command);

        assert_eq!(handler.command.len(), 1);
        assert_eq!(handler.command[0], "my-server");
    }

    #[test]
    fn test_new_preserves_command_order_and_args() {
        let command: Arc<[String]> = vec![
            "node".into(),
            "--experimental-modules".into(),
            "server.js".into(),
        ]
        .into();
        let handler = ProxyHandler::new(command);

        assert_eq!(handler.command.len(), 3);
        assert_eq!(handler.command[0], "node");
        assert_eq!(handler.command[1], "--experimental-modules");
        assert_eq!(handler.command[2], "server.js");
    }

    // -----------------------------------------------------------------------
    // ProxyHandler::peer — error when not initialized
    // -----------------------------------------------------------------------

    #[test]
    fn test_peer_returns_error_when_not_initialized() {
        let handler = ProxyHandler::new(vec!["echo".into()].into());

        let result = handler.peer();
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert_eq!(err.code, ErrorCode::INTERNAL_ERROR);
        assert!(
            err.message.contains("not initialized"),
            "expected 'not initialized' in message, got: {}",
            err.message,
        );
        assert!(err.data.is_none());
    }

    // -----------------------------------------------------------------------
    // ProxyHandler::child_error
    // -----------------------------------------------------------------------

    #[test]
    fn test_child_error_with_string_message() {
        let err = ProxyHandler::child_error("connection refused");

        assert_eq!(err.code, ErrorCode::INTERNAL_ERROR);
        assert!(
            err.message.contains("child server error"),
            "expected 'child server error' prefix, got: {}",
            err.message,
        );
        assert!(
            err.message.contains("connection refused"),
            "expected original message in output, got: {}",
            err.message,
        );
        assert!(err.data.is_none());
    }

    #[test]
    fn test_child_error_with_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe broken");
        let err = ProxyHandler::child_error(io_err);

        assert_eq!(err.code, ErrorCode::INTERNAL_ERROR);
        assert!(err.message.contains("child server error"));
        assert!(err.message.contains("pipe broken"));
    }

    #[test]
    fn test_child_error_with_empty_message() {
        let err = ProxyHandler::child_error("");

        assert_eq!(err.code, ErrorCode::INTERNAL_ERROR);
        assert!(err.message.contains("child server error"));
    }

    // -----------------------------------------------------------------------
    // ChildClientHandler::upstream_error
    // -----------------------------------------------------------------------

    #[test]
    fn test_upstream_error_with_string_message() {
        let err = ChildClientHandler::upstream_error("timeout");

        assert_eq!(err.code, ErrorCode::INTERNAL_ERROR);
        assert!(
            err.message.contains("upstream client error"),
            "expected 'upstream client error' prefix, got: {}",
            err.message,
        );
        assert!(
            err.message.contains("timeout"),
            "expected original message in output, got: {}",
            err.message,
        );
        assert!(err.data.is_none());
    }

    #[test]
    fn test_upstream_error_with_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "connection reset");
        let err = ChildClientHandler::upstream_error(io_err);

        assert_eq!(err.code, ErrorCode::INTERNAL_ERROR);
        assert!(err.message.contains("upstream client error"));
        assert!(err.message.contains("connection reset"));
    }

    #[test]
    fn test_upstream_error_with_empty_message() {
        let err = ChildClientHandler::upstream_error("");

        assert_eq!(err.code, ErrorCode::INTERNAL_ERROR);
        assert!(err.message.contains("upstream client error"));
    }

    // -----------------------------------------------------------------------
    // Error helpers produce distinct prefixes
    // -----------------------------------------------------------------------

    #[test]
    fn test_child_and_upstream_errors_have_distinct_prefixes() {
        let child_err = ProxyHandler::child_error("boom");
        let upstream_err = ChildClientHandler::upstream_error("boom");

        // Same error code, but different human-readable prefixes
        assert_eq!(child_err.code, upstream_err.code);
        assert_ne!(child_err.message, upstream_err.message);
        assert!(child_err.message.contains("child server error"));
        assert!(upstream_err.message.contains("upstream client error"));
    }
}
