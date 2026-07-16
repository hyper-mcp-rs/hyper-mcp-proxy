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
mod stderr;

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

/// Apply the operator-provided `--session-keep-alive` flag to an rmcp
/// [`LocalSessionManager`].
///
/// - `None` — leave the rmcp default (5 minutes) in place.
/// - `Some(0)` — disable the idle timeout entirely.
/// - `Some(n)` — set the idle timeout to `n` seconds.
fn apply_session_keep_alive(manager: &mut LocalSessionManager, value: Option<u64>) {
    match value {
        None => {}
        Some(0) => manager.session_config.keep_alive = None,
        Some(n) => manager.session_config.keep_alive = Some(Duration::from_secs(n)),
    }
}

/// Wait for an OS shutdown signal.
///
/// On Unix this awaits either `SIGINT` (Ctrl-C) or `SIGTERM`, so the
/// proxy responds correctly to both interactive interrupts and standard
/// process-supervisor termination (Docker, systemd, etc.). On other
/// platforms only Ctrl-C is awaited.
///
/// Returns once any of the supported signals has been observed, or if
/// the signal handlers themselves cannot be installed.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        tracing::info!("received SIGINT");
                    }
                    _ = sigterm.recv() => {
                        tracing::info!("received SIGTERM");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "failed to install SIGTERM handler; falling back to SIGINT only"
                );
                tokio::signal::ctrl_c().await.ok();
                tracing::info!("received SIGINT");
            }
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("received shutdown signal");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing. Logs are emitted as structured JSON lines on
    // stdout so log collectors (ECS, Cloud Run, etc.) can ingest them
    // without a parsing step.
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,hyper_streamable_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer().json())
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
    apply_session_keep_alive(&mut session_manager, cli.session_keep_alive);

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
                shutdown_signal().await;
                token.cancel();
            }
        })
        .await?;

    tracing::info!("server shut down");
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Cli parsing
    // -----------------------------------------------------------------------

    /// Parse argv as if it had been passed to the binary, with the program
    /// name `hyper-mcp-proxy` prepended so clap is happy.
    fn parse(args: &[&str]) -> Cli {
        let mut full = vec!["hyper-mcp-proxy"];
        full.extend_from_slice(args);
        Cli::parse_from(full)
    }

    #[test]
    fn cli_uses_documented_defaults_for_host_port_endpoint() {
        let cli = parse(&["--", "echo"]);

        assert_eq!(cli.host, "127.0.0.1");
        assert_eq!(cli.port, 8080);
        assert_eq!(cli.endpoint, "/mcp");
        assert!(cli.session_keep_alive.is_none());
    }

    #[test]
    fn cli_session_keep_alive_is_none_by_default() {
        let cli = parse(&["--", "echo"]);
        assert!(cli.session_keep_alive.is_none());
    }

    #[test]
    fn cli_session_keep_alive_parses_seconds() {
        let cli = parse(&["--session-keep-alive", "42", "--", "echo"]);
        assert_eq!(cli.session_keep_alive, Some(42));
    }

    #[test]
    fn cli_session_keep_alive_accepts_zero() {
        let cli = parse(&["--session-keep-alive", "0", "--", "echo"]);
        assert_eq!(cli.session_keep_alive, Some(0));
    }

    #[test]
    fn cli_accepts_short_host_flag() {
        let cli = parse(&["-H", "0.0.0.0", "--", "echo"]);
        assert_eq!(cli.host, "0.0.0.0");
    }

    #[test]
    fn cli_accepts_custom_port_and_endpoint() {
        let cli = parse(&["--port", "9090", "--endpoint", "/rpc", "--", "echo"]);
        assert_eq!(cli.port, 9090);
        assert_eq!(cli.endpoint, "/rpc");
    }

    #[test]
    fn cli_captures_command_after_double_dash() {
        let cli = parse(&["--", "node", "server.js", "--flag"]);

        assert_eq!(
            cli.command,
            vec![
                "node".to_string(),
                "server.js".to_string(),
                "--flag".to_string()
            ]
        );
    }

    #[test]
    fn cli_command_preserves_argument_order() {
        let cli = parse(&["--", "a", "b", "c"]);
        assert_eq!(cli.command, vec!["a", "b", "c"]);
    }

    #[test]
    fn cli_rejects_missing_command() {
        // `Cli::try_parse_from` returns Err rather than exiting the process.
        let result = Cli::try_parse_from(["hyper-mcp-proxy"]);
        assert!(result.is_err(), "command is required (required = true)");
    }

    #[test]
    fn cli_rejects_invalid_port() {
        let result =
            Cli::try_parse_from(["hyper-mcp-proxy", "--port", "not-a-number", "--", "echo"]);
        assert!(result.is_err(), "port must parse as u16");
    }

    #[test]
    fn cli_rejects_invalid_session_keep_alive() {
        let result = Cli::try_parse_from([
            "hyper-mcp-proxy",
            "--session-keep-alive",
            "abc",
            "--",
            "echo",
        ]);
        assert!(result.is_err(), "session-keep-alive must parse as u64");
    }

    #[test]
    fn cli_rejects_negative_session_keep_alive() {
        let result = Cli::try_parse_from([
            "hyper-mcp-proxy",
            "--session-keep-alive",
            "-1",
            "--",
            "echo",
        ]);
        assert!(result.is_err(), "session-keep-alive must be unsigned");
    }

    // -----------------------------------------------------------------------
    // apply_session_keep_alive
    // -----------------------------------------------------------------------

    #[test]
    fn apply_session_keep_alive_none_preserves_rmcp_default() {
        let mut manager = LocalSessionManager::default();
        let original = manager.session_config.keep_alive;

        apply_session_keep_alive(&mut manager, None);

        assert_eq!(
            manager.session_config.keep_alive, original,
            "None must not mutate the rmcp default"
        );
    }

    #[test]
    fn apply_session_keep_alive_zero_disables_timeout() {
        let mut manager = LocalSessionManager::default();
        // Pre-seed with a value so we can detect that it gets cleared.
        manager.session_config.keep_alive = Some(Duration::from_secs(300));

        apply_session_keep_alive(&mut manager, Some(0));

        assert!(
            manager.session_config.keep_alive.is_none(),
            "Some(0) must disable the idle timeout"
        );
    }

    #[test]
    fn apply_session_keep_alive_some_n_sets_n_seconds() {
        let mut manager = LocalSessionManager::default();

        apply_session_keep_alive(&mut manager, Some(7));

        assert_eq!(
            manager.session_config.keep_alive,
            Some(Duration::from_secs(7)),
        );
    }

    #[test]
    fn apply_session_keep_alive_overwrites_previous_value() {
        let mut manager = LocalSessionManager::default();
        manager.session_config.keep_alive = Some(Duration::from_secs(60));

        apply_session_keep_alive(&mut manager, Some(1));

        assert_eq!(
            manager.session_config.keep_alive,
            Some(Duration::from_secs(1)),
        );
    }

    #[test]
    fn apply_session_keep_alive_handles_large_values() {
        let mut manager = LocalSessionManager::default();

        apply_session_keep_alive(&mut manager, Some(86_400));

        assert_eq!(
            manager.session_config.keep_alive,
            Some(Duration::from_secs(86_400)),
        );
    }
}
