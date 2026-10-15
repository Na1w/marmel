//! Shared reqwest client construction.
//!
//! The LLM client (`connect_timeout(10s)`, no total timeout) and the MCP
//! transport (`timeout(30s)`, no connect timeout) previously built near-
//! identical clients inline; this is the single shared builder. Each caller
//! keeps the client-level settings it genuinely needs by passing them in as
//! [`HttpClientOptions`]; per-request concerns (auth headers, `Accept`,
//! `Mcp-Session-Id`, bodies) stay at the call sites.

use std::time::Duration;

/// Client-level options accepted by [`build_http_client_with`].
///
/// Everything here maps 1:1 onto a reqwest builder call and is `None` by
/// default, i.e. reqwest's own default (no timeout).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HttpClientOptions {
    /// Connect timeout (TCP + TLS handshake). `None` leaves reqwest's default.
    pub connect_timeout: Option<Duration>,
    /// Total request timeout, covering the response body as well. The LLM chat
    /// client deliberately leaves this unset — its SSE streams are bounded by
    /// the watchdogs in [`crate::net::sse`] instead.
    pub total_timeout: Option<Duration>,
}

/// Build a reqwest client with optional connect and total timeouts.
///
/// `None` leaves the corresponding timeout unset (reqwest default: none).
/// Ergonomic entry point for timeout-only callers; it delegates to
/// [`build_http_client_with`] so there is exactly one construction site.
pub fn build_http_client(
    connect_timeout: Option<Duration>,
    total_timeout: Option<Duration>,
) -> Result<reqwest::Client, reqwest::Error> {
    build_http_client_with(HttpClientOptions {
        connect_timeout,
        total_timeout,
    })
}

/// Build a reqwest client from [`HttpClientOptions`] — the single place both the
/// LLM chat client and the MCP HTTP/SSE transport construct their `Client`
/// (cluster C3 in `docs/recon_duplication_net.md`).
pub fn build_http_client_with(opts: HttpClientOptions) -> Result<reqwest::Client, reqwest::Error> {
    let mut builder = reqwest::Client::builder();
    if let Some(t) = opts.connect_timeout {
        builder = builder.connect_timeout(t);
    }
    if let Some(t) = opts.total_timeout {
        builder = builder.timeout(t);
    }
    builder.build()
}
