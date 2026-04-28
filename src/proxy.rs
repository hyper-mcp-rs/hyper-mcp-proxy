use std::sync::Arc;

use rmcp::{
    ErrorData, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResult, CancelledNotificationParam, CompleteRequestParams,
        CompleteResult, CustomNotification, CustomRequest, CustomResult, ErrorCode,
        GetPromptRequestParams, GetPromptResult, Implementation, InitializeRequestParams,
        InitializeResult, ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult,
        ListToolsResult, PaginatedRequestParams, ProgressNotificationParam,
        ReadResourceRequestParams, ReadResourceResult, ServerCapabilities, SetLevelRequestParams,
        SubscribeRequestParams, UnsubscribeRequestParams,
    },
    service::{NotificationContext, Peer, RequestContext, RoleClient, RoleServer},
    transport::child_process::TokioChildProcess,
};
use tokio::sync::OnceCell;

/// Shared state for a single proxy session, created during initialization.
struct ProxyInner {
    /// Peer handle to call methods on the child MCP server.
    peer: Peer<RoleClient>,
    /// Background task keeping the client service alive.
    _service_handle: tokio::task::JoinHandle<()>,
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
}

impl ProxyHandler {
    /// Create a new proxy handler for the given command.
    ///
    /// The child process is **not** spawned until [`initialize`] is called.
    pub fn new(command: Arc<[String]>) -> Self {
        Self {
            command,
            inner: OnceCell::new(),
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

    /// Spawn the child process and establish an MCP client connection to it.
    ///
    /// Returns the peer handle, the child's [`InitializeResult`], and a
    /// background join-handle that keeps the client service alive.
    async fn spawn_child(
        &self,
    ) -> Result<
        (
            Peer<RoleClient>,
            InitializeResult,
            tokio::task::JoinHandle<()>,
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

        // Connect as an MCP client to the child.
        // `()` as the client handler means we accept defaults (no custom notification handling).
        // `.serve()` performs the full initialize handshake with the child.
        let client_service = ().serve(transport).await.map_err(|e| {
            tracing::error!(error = %e, "failed to connect to child MCP server");
            ErrorData::new(
                ErrorCode::INTERNAL_ERROR,
                format!("failed to connect to child MCP server: {e}"),
                None,
            )
        })?;

        let peer = client_service.peer().clone();

        // Probe the child to discover its actual capabilities.
        let capabilities = Self::probe_capabilities(&peer).await;

        let mut init_result = InitializeResult::new(capabilities);
        init_result.server_info =
            Implementation::new("hyper-streamable-http-proxy", env!("CARGO_PKG_VERSION"));

        // Keep the client service alive in a background task.
        let handle = tokio::spawn(async move {
            let _ = client_service.waiting().await;
            tracing::info!("child MCP server session ended");
        });

        Ok((peer, init_result, handle))
    }

    /// Probe the child server to discover its actual capabilities.
    ///
    /// We try each list operation and record which ones the child supports.
    async fn probe_capabilities(peer: &Peer<RoleClient>) -> ServerCapabilities {
        let mut caps = ServerCapabilities::default();

        if peer.list_tools(None).await.is_ok() {
            caps.tools = Some(rmcp::model::ToolsCapability {
                list_changed: Some(true),
            });
        }

        if peer.list_resources(None).await.is_ok() {
            caps.resources = Some(rmcp::model::ResourcesCapability {
                subscribe: Some(true),
                list_changed: Some(true),
            });
        }

        if peer.list_prompts(None).await.is_ok() {
            caps.prompts = Some(rmcp::model::PromptsCapability {
                list_changed: Some(true),
            });
        }

        caps
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
    #[tracing::instrument(skip_all, fields(session = "initializing"))]
    async fn initialize(
        &self,
        _request: InitializeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        let (peer, init_result, handle) = self.spawn_child().await?;

        self.inner
            .set(ProxyInner {
                peer,
                _service_handle: handle,
            })
            .map_err(|_| {
                ErrorData::new(
                    ErrorCode::INTERNAL_ERROR,
                    "session already initialized",
                    None,
                )
            })?;

        tracing::info!("proxy session initialized successfully");
        Ok(init_result)
    }

    // -- Tools --------------------------------------------------------------

    #[tracing::instrument(skip_all)]
    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        self.peer()?
            .list_tools(request)
            .await
            .map_err(Self::child_error)
    }

    #[tracing::instrument(skip_all, fields(tool = %request.name))]
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        self.peer()?
            .call_tool(request)
            .await
            .map_err(Self::child_error)
    }

    // -- Resources ----------------------------------------------------------

    #[tracing::instrument(skip_all)]
    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        self.peer()?
            .list_resources(request)
            .await
            .map_err(Self::child_error)
    }

    #[tracing::instrument(skip_all)]
    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        self.peer()?
            .list_resource_templates(request)
            .await
            .map_err(Self::child_error)
    }

    #[tracing::instrument(skip_all, fields(uri = %request.uri))]
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        self.peer()?
            .read_resource(request)
            .await
            .map_err(Self::child_error)
    }

    #[tracing::instrument(skip_all)]
    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        self.peer()?
            .subscribe(request)
            .await
            .map_err(Self::child_error)
    }

    #[tracing::instrument(skip_all)]
    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        self.peer()?
            .unsubscribe(request)
            .await
            .map_err(Self::child_error)
    }

    // -- Prompts ------------------------------------------------------------

    #[tracing::instrument(skip_all)]
    async fn list_prompts(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        self.peer()?
            .list_prompts(request)
            .await
            .map_err(Self::child_error)
    }

    #[tracing::instrument(skip_all, fields(name = %request.name))]
    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, ErrorData> {
        self.peer()?
            .get_prompt(request)
            .await
            .map_err(Self::child_error)
    }

    // -- Completions --------------------------------------------------------

    #[tracing::instrument(skip_all)]
    async fn complete(
        &self,
        request: CompleteRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, ErrorData> {
        self.peer()?
            .complete(request)
            .await
            .map_err(Self::child_error)
    }

    // -- Logging ------------------------------------------------------------

    #[tracing::instrument(skip_all)]
    async fn set_level(
        &self,
        request: SetLevelRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        self.peer()?
            .set_level(request)
            .await
            .map_err(Self::child_error)
    }

    // -- Notifications (client → child) -------------------------------------

    #[tracing::instrument(skip_all)]
    async fn on_cancelled(
        &self,
        notification: CancelledNotificationParam,
        _context: NotificationContext<RoleServer>,
    ) {
        if let Ok(peer) = self.peer()
            && let Err(e) = peer.notify_cancelled(notification).await {
                tracing::warn!(error = %e, "failed to forward cancellation to child");
            }
    }

    #[tracing::instrument(skip_all)]
    async fn on_progress(
        &self,
        notification: ProgressNotificationParam,
        _context: NotificationContext<RoleServer>,
    ) {
        if let Ok(peer) = self.peer()
            && let Err(e) = peer.notify_progress(notification).await {
                tracing::warn!(error = %e, "failed to forward progress to child");
            }
    }

    #[tracing::instrument(skip_all)]
    async fn on_initialized(&self, _context: NotificationContext<RoleServer>) {
        tracing::debug!("client sent initialized notification");
    }

    #[tracing::instrument(skip_all)]
    async fn on_roots_list_changed(&self, _context: NotificationContext<RoleServer>) {
        if let Ok(peer) = self.peer()
            && let Err(e) = peer.notify_roots_list_changed().await {
                tracing::warn!(error = %e, "failed to forward roots_list_changed to child");
            }
    }

    #[tracing::instrument(skip_all)]
    async fn on_custom_notification(
        &self,
        notification: CustomNotification,
        _context: NotificationContext<RoleServer>,
    ) {
        tracing::debug!(
            method = %notification.method,
            "received custom notification (cannot be proxied at typed level)"
        );
    }

    // -- Custom requests ----------------------------------------------------

    #[tracing::instrument(skip_all)]
    async fn on_custom_request(
        &self,
        request: CustomRequest,
        _context: RequestContext<RoleServer>,
    ) -> Result<CustomResult, ErrorData> {
        tracing::debug!(method = %request.method, "received custom request");
        Err(ErrorData::new(
            ErrorCode::METHOD_NOT_FOUND,
            format!("custom method '{}' cannot be proxied", request.method),
            None,
        ))
    }
}
