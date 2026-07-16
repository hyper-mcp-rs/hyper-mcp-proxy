//! Capture of child-process stderr into the proxy's `tracing` pipeline.
//!
//! Per the MCP spec, stderr is a stdio server's *logging* channel — clients
//! may capture, forward, or ignore it. This proxy captures it so that child
//! diagnostics (including crash output) land in the proxy's structured logs,
//! correlated with the child PID and MCP session ID, instead of interleaving
//! raw across sessions.

use std::sync::Arc;

use tokio::io::AsyncBufReadExt;
use tokio::sync::OnceCell;

/// Tracing target for lines captured from a child process's stderr.
///
/// Kept distinct from the module path so operators can filter or re-level
/// child output independently, e.g. `RUST_LOG=info,child_stderr=off`.
const TARGET: &str = "child";

/// Read a child process's stderr to EOF, logging each line through
/// [`tracing`] tagged with the child PID and MCP session ID.
///
/// Routine diagnostics as well as crash output land on stderr, so lines are
/// emitted at INFO level under the [`CHILD_STDERR_TARGET`] target rather
/// than as errors.
///
/// The session ID cell is read per line because stderr output can begin
/// before the first post-`initialize` request populates it.
///
/// Returns the number of lines logged (primarily for tests). Stops at EOF
/// or on a read error (e.g. invalid UTF-8), logging the latter.
pub async fn log_stderr<R>(stderr: R, pid: Option<u32>, session_id: Arc<OnceCell<Arc<str>>>) -> u64
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = tokio::io::BufReader::new(stderr).lines();
    let mut logged = 0u64;
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                logged += 1;
                let session_id = session_id.get().map(|s| s.as_ref()).unwrap_or("<unknown>");
                tracing::info!(
                    target: TARGET,
                    pid,
                    %session_id,
                    "{line}"
                );
            }
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(pid, error = %e, "failed to read child stderr");
                break;
            }
        }
    }
    logged
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: run `log_stderr` over an in-memory byte buffer with an unset
    /// session ID cell.
    async fn log_bytes(bytes: &[u8]) -> u64 {
        log_stderr(bytes, Some(1234), Arc::new(OnceCell::new())).await
    }

    #[tokio::test]
    async fn test_log_stderr_counts_lines() {
        assert_eq!(log_bytes(b"first\nsecond\nthird\n").await, 3);
    }

    #[tokio::test]
    async fn test_log_stderr_empty_input_logs_nothing() {
        assert_eq!(log_bytes(b"").await, 0);
    }

    #[tokio::test]
    async fn test_log_stderr_handles_missing_trailing_newline() {
        // A final partial line (EOF without '\n') must still be logged.
        assert_eq!(log_bytes(b"one\ntwo").await, 2);
    }

    #[tokio::test]
    async fn test_log_stderr_logs_blank_lines() {
        assert_eq!(log_bytes(b"\n\n").await, 2);
    }

    #[tokio::test]
    async fn test_log_stderr_stops_on_invalid_utf8() {
        // `lines()` yields an error for invalid UTF-8; the loop must stop
        // gracefully rather than spin or panic.
        assert_eq!(log_bytes(b"ok\n\xff\xfe\n").await, 1);
    }

    #[tokio::test]
    async fn test_log_stderr_with_populated_session_id() {
        // Session ID present and no PID — exercises the other tag branch.
        let session_id: Arc<OnceCell<Arc<str>>> = Arc::new(OnceCell::new());
        session_id.set(Arc::from("session-abc")).unwrap();

        let n = log_stderr(&b"line\n"[..], None, session_id).await;
        assert_eq!(n, 1);
    }
}
