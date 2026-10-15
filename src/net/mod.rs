//! Shared network plumbing for the LLM chat client and the MCP HTTP/SSE transport.
//!
//! Extracted from `src/llm/client.rs` and `src/mcp/http.rs` per
//! `docs/recon_duplication_net.md` §2 (clusters C1–C4): the SSE pump skeleton
//! (50 ms poll + idle-watchdog + abort hook), the generic retry-with-backoff
//! policy, and the reqwest client builder previously existed in two divergent
//! copies.

pub mod http;
pub mod retry;
pub mod sse;

pub use http::build_http_client;
pub use retry::{BACKOFF_BASE_MS, MAX_ATTEMPTS, RetryOp, Retryable, retry_with_backoff};
pub use sse::{PumpNext, SSE_POLL_INTERVAL, pump_future, pump_next};
