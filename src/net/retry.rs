//! Generic retry-with-linear-backoff policy shared by the LLM chat client and
//! the MCP HTTP/SSE transport.
//!
//! **Intentional behavior change (task t-c3x1):** the MCP client previously
//! had *no* retries — a failed POST failed immediately. It now shares this
//! exact policy with the LLM client: up to [`MAX_ATTEMPTS`] total attempts
//! with linear backoff `BACKOFF_BASE_MS × attempt`, retrying only on errors
//! whose implementor of [`Retryable`] marks retryable (HTTP 503/429/502/504,
//! transport failures, stream errors, and timeouts). Application-level errors
//! (e.g. JSON-RPC errors) are never retried.

use std::future::Future;
use std::time::Duration;

/// Maximum total attempts (initial + up to 2 retries for 503/429/timeouts).
pub const MAX_ATTEMPTS: u32 = 3;
/// Backoff base: sleep = `BACKOFF_BASE_MS × attempt`.
pub const BACKOFF_BASE_MS: u64 = 1000;

/// Marker trait for errors that know whether they are worth retrying.
///
/// Implemented by `llm::client::ChatError` (LLM backend) and
/// `mcp::http::McpError` (MCP transport) so both clients share one retry
/// policy with per-client retryability classification.
pub trait Retryable: std::fmt::Display {
    /// Whether a new attempt is likely to succeed for this error class.
    fn is_retryable(&self) -> bool;
}

/// Run `op` up to `max_attempts` times, sleeping `base_ms × attempt` between
/// retryable failures. Returns the first success or the last error.
///
/// `tag` is used in the warn/error log lines (e.g. `"LLM backend call"` or
/// `"MCP call to '<server>' for '<method>'"`).
///
/// `state` is passed to `op` on every attempt so the operation can borrow
/// mutable per-call state (e.g. the LLM client's `on_delta` callback) across
/// retries without the closure-capture lifetime issues of `FnMut() -> Fut`.
/// Pass `&mut ()` when no state is needed.
pub async fn retry_with_backoff<E, T, S, F, Fut>(
    max_attempts: u32,
    base_ms: u64,
    tag: &str,
    state: &mut S,
    mut op: F,
) -> Result<T, E>
where
    E: Retryable,
    F: FnMut(&mut S) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match op(state).await {
            Ok(v) => return Ok(v),
            Err(e) if e.is_retryable() && attempt < max_attempts => {
                let ms = base_ms * attempt as u64;
                tracing::warn!(
                    "{tag} attempt {attempt}/{max_attempts} failed ({e}), retrying in {ms}ms..."
                );
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
            Err(e) => {
                tracing::error!("{tag} failed after {attempt} attempts: {e}");
                return Err(e);
            }
        }
    }
}
