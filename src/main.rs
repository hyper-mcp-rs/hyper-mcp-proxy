use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::Router;
use clap::Parser;
use rmcp::transport::{
    StreamableHttpServerConfig,
    streamable_http_server::{session::local::LocalSessionManager, tower::StreamableHttpService},
};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod proxy;

use proxy::ProxyHandler;

/// A streamable-http to stdio MCP proxy.
///
/// Accepts MCP clients over Streamable HTTP and, for each session,
/// spawns a dedicated stdio MCP child process, forwarding all messages
/// bidirectionally.
#[derive(Parser, Debug)]
#[command(
    name = "hyper-mcp-proxy",
    version,
    about = "Streamable HTTP to stdio MCP proxy"
)]
struct Cli {
    /// Host address to bind to
    #[arg(short = 'H', long, default_value = "127.0.0.1")]
    host: String,

    /// Port to bind to
    #[arg(short, long, default_value_t = 8080)]
    port: u16,

    /// Endpoint path for MCP (e.g. "/mcp")
    #[arg(short, long, default_value = "/mcp")]
    endpoint: String,

    /// Idle timeout for MCP sessions, in seconds.
    ///
    /// Resets on every client request or notification. When the timer
    /// elapses without activity, the session worker is reaped and its
    /// stdio child process is killed. Subsequent requests carrying the
    /// stale `Mcp-Session-Id` will receive HTTP 404 per the MCP spec.
    ///
    /// When unset, rmcp's `LocalSessionManager` default (5 minutes)
    /// applies. The proxy does not override `SessionConfig` unless this
    /// flag is provided.
    #[arg(long, value_name = "SECONDS")]
    session_keep_alive: Option<u64>,

    /// The stdio MCP server command and arguments (after --)
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,hyper_streamable_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();
    let cancellation_token = CancellationToken::new();
    let command: Arc<[String]> = cli.command.into();

    let config = StreamableHttpServerConfig::default()
        .with_cancellation_token(cancellation_token.clone())
        .disable_allowed_hosts();

    // Build the session manager. We only construct a custom
    // `SessionConfig` when the operator passed `--session-keep-alive`;
    // otherwise we use `LocalSessionManager::default()` so rmcp's own
    // defaults govern every knob.
    let mut session_manager = LocalSessionManager::default();

    if let Some(session_keep_alive) = cli.session_keep_alive {
        if session_keep_alive == 0 {
            session_manager.session_config.keep_alive = None;
        } else {
            session_manager.session_config.keep_alive =
                Some(Duration::from_secs(session_keep_alive));
        }
    }

    let cmd = command.clone();
    let mcp_service: StreamableHttpService<ProxyHandler, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(ProxyHandler::new(cmd.clone())),
            Arc::new(session_manager),
            config,
        );

    let endpoint = cli.endpoint.clone();
    let app = Router::new().nest_service(&endpoint, mcp_service);

    let addr: SocketAddr = format!("{}:{}", cli.host, cli.port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;

    tracing::info!(
        %addr,
        endpoint = %cli.endpoint,
        command = ?&*command,
        session_keep_alive = ?cli.session_keep_alive,
        "proxy server started"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown({
            let token = cancellation_token.clone();
            async move {
                tokio::signal::ctrl_c().await.ok();
                tracing::info!("received shutdown signal");
                token.cancel();
            }
        })
        .await?;

    tracing::info!("server shut down");
    Ok(())
}
