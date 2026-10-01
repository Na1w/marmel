//! Shared reqwest client construction.
//!
//! The LLM client (`connect_timeout(10s)`, no total timeout) and the MCP
//! transport (`timeout(30s)`, no connect timeout) previously built near-
//! identical clients inline; this is the single shared builder.

use std::time::Duration;

/// Build a reqwest client with optional connect and total timeouts.
///
/// `None` leaves the corresponding timeout unset (reqwest default: none).
pub fn build_http_client(
    connect_timeout: Option<Duration>,
    total_timeout: Option<Duration>,
) -> Result<reqwest::Client, reqwest::Error> {
    let mut builder = reqwest::Client::builder();
    if let Some(t) = connect_timeout {
        builder = builder.connect_timeout(t);
    }
    if let Some(t) = total_timeout {
        builder = builder.timeout(t);
    }
    builder.build()
}
