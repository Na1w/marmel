//! Model Context Protocol (MCP) Client implementation.
//!
//! Provides JSON-RPC 2.0 communication over stdio and HTTP/SSE transports,
//! supporting server initialization, tool discovery (`tools/list`), and tool execution (`tools/call`).

pub mod client;
pub mod http;
#[cfg(test)]
mod http_tests;

pub use client::{McpClient, McpManager, McpServerConfig, McpTool};
pub use http::HttpSseConnection;
