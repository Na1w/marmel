//! Shared network plumbing for the LLM chat client and the MCP HTTP/SSE transport.
//!
//! Single home for the SSE pump skeleton (50 ms poll + idle-watchdog + abort
//! hook), the generic retry-with-backoff policy, and the reqwest client
//! builder.

pub mod http;
pub mod retry;
pub mod sse;

pub use http::build_http_client;
pub use retry::{BACKOFF_BASE_MS, MAX_ATTEMPTS, RetryOp, Retryable, retry_with_backoff};
pub use sse::{PumpNext, SSE_POLL_INTERVAL, pump_future, pump_next};
