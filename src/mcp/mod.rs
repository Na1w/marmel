//! Model Context Protocol (MCP) Client implementation.
//!
//! Provides JSON-RPC 2.0 communication over stdio and HTTP/SSE transports,
//! supporting server initialization, tool discovery (`tools/list`), and tool execution (`tools/call`).
//!
//! # Name policy (security t-034c)
//!
//! MCP tools reach the model and the dispatcher under a *composed* wire name,
//! `{server_name}__{raw_tool_name}` (see [`McpTool::qualified_name`]). Because the
//! harness dispatcher owns bare names such as `run_command` and namespaced names
//! such as `terminal__run_command`, an unvalidated server name is a sandbox-escape
//! vector: a server called `terminal` exposing a tool called `run_command` would
//! publish exactly the built-in alias that the sandboxed PTY runner answers to.
//!
//! [`validate_server_name`], [`validate_mcp_tool_name`] and
//! [`validate_composed_registration`] define the policy; the harness applies them
//! at registration time (`harness::set_mcp_manager`) and again at dispatch time
//! (`harness::McpDispatchGate::route`). Rejected names are reported, never renamed.

pub mod client;
pub mod http;
#[cfg(test)]
mod http_tests;
mod protocol;

pub use client::{McpClient, McpManager, McpServerConfig, McpTool};
pub use http::HttpSseConnection;

// ── Name policy ────────────────────────────────────────────────────────────

/// The separator used to compose an MCP server name with a raw tool name
/// (`{server_name}{NAME_SEPARATOR}{raw_tool_name}`).
pub const NAME_SEPARATOR: &str = "__";

/// Maximum accepted length of an MCP server name.
pub const MAX_SERVER_NAME_LEN: usize = 64;

/// Maximum accepted length of a raw tool name reported by an MCP server.
pub const MAX_MCP_TOOL_NAME_LEN: usize = 128;

/// Why a configured MCP server name was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerNameError {
    /// The name is empty.
    Empty,
    /// The name is longer than [`MAX_SERVER_NAME_LEN`].
    TooLong {
        /// The bound that was exceeded.
        limit: usize,
        /// The length of the offending name.
        length: usize,
    },
    /// The name embeds [`NAME_SEPARATOR`], which would make the composed wire name
    /// ambiguous and let a server forge a namespaced built-in name.
    ContainsSeparator,
    /// The name holds a character outside ASCII alphanumerics, `-` and `_`. This
    /// covers `.`, `/`, `\`, whitespace, control characters and non-ASCII bytes,
    /// i.e. the dotted-namespace and path-separator forms that must never become
    /// part of a wire tool name.
    IllegalCharacter {
        /// The offending character.
        character: char,
    },
}

impl std::fmt::Display for ServerNameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerNameError::Empty => write!(f, "server name is empty"),
            ServerNameError::TooLong { limit, length } => write!(
                f,
                "server name is {length} characters long (maximum {limit})"
            ),
            ServerNameError::ContainsSeparator => write!(
                f,
                "server name must not contain the composition separator '{NAME_SEPARATOR}'"
            ),
            ServerNameError::IllegalCharacter { character } => write!(
                f,
                "server name contains illegal character {character:?} \u{28}only ASCII letters, digits, '-' and '_' are allowed\u{29}"
            ),
        }
    }
}

impl std::error::Error for ServerNameError {}

/// Why a raw tool name reported by an MCP server was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpToolNameError {
    /// The name is empty.
    Empty,
    /// The name is longer than [`MAX_MCP_TOOL_NAME_LEN`].
    TooLong {
        /// The bound that was exceeded.
        limit: usize,
        /// The length of the offending name.
        length: usize,
    },
    /// The name embeds [`NAME_SEPARATOR`], which would make the composed wire name
    /// ambiguous under [`split_composed_name`].
    ContainsSeparator,
    /// The name holds a character outside ASCII alphanumerics, `-`, `_` and `.`.
    IllegalCharacter {
        /// The offending character.
        character: char,
    },
}

impl std::fmt::Display for McpToolNameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpToolNameError::Empty => write!(f, "tool name is empty"),
            McpToolNameError::TooLong { limit, length } => {
                write!(f, "tool name is {length} characters long (maximum {limit})")
            }
            McpToolNameError::ContainsSeparator => write!(
                f,
                "tool name must not contain the composition separator '{NAME_SEPARATOR}'"
            ),
            McpToolNameError::IllegalCharacter { character } => write!(
                f,
                "tool name contains illegal character {character:?} \u{28}only ASCII letters, digits, '-', '_' and '.' are allowed\u{29}"
            ),
        }
    }
}

impl std::error::Error for McpToolNameError {}

/// Why an MCP tool registration was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpNameError {
    /// The server name is invalid.
    Server(ServerNameError),
    /// The raw tool name is invalid.
    Tool(McpToolNameError),
    /// The composed name is (or normalizes to) a name the built-in dispatcher owns.
    BuiltInCollision {
        /// The composed wire name the server proposed.
        qualified: String,
        /// The built-in name it would have shadowed.
        builtin: String,
    },
}

impl std::fmt::Display for McpNameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpNameError::Server(e) => write!(f, "{e}"),
            McpNameError::Tool(e) => write!(f, "{e}"),
            McpNameError::BuiltInCollision { qualified, builtin } => write!(
                f,
                "composed MCP tool name '{qualified}' would shadow built-in tool '{builtin}' \u{28}built-in tools always resolve first\u{29}"
            ),
        }
    }
}

impl std::error::Error for McpNameError {}

/// Validate an MCP server name.
///
/// The accepted grammar is exactly `[A-Za-z0-9_-]+`, non-empty and at most
/// [`MAX_SERVER_NAME_LEN`] characters, and it must not contain [`NAME_SEPARATOR`].
/// That grammar excludes `.`, `/`, `\`, whitespace, control characters and
/// non-ASCII bytes, so a server name can never carry a path, a dotted namespace,
/// or a forged namespace prefix.
pub fn validate_server_name(name: &str) -> Result<(), ServerNameError> {
    if name.is_empty() {
        return Err(ServerNameError::Empty);
    }
    let length = name.chars().count();
    if length > MAX_SERVER_NAME_LEN {
        return Err(ServerNameError::TooLong {
            limit: MAX_SERVER_NAME_LEN,
            length,
        });
    }
    if name.contains(NAME_SEPARATOR) {
        return Err(ServerNameError::ContainsSeparator);
    }
    for character in name.chars() {
        if !(character.is_ascii_alphanumeric() || character == '-' || character == '_') {
            return Err(ServerNameError::IllegalCharacter { character });
        }
    }
    Ok(())
}

/// Validate a raw tool name as reported by an MCP server.
///
/// The accepted grammar is `[A-Za-z0-9_.]+` (non-empty, at most
/// [`MAX_MCP_TOOL_NAME_LEN`] characters) with no embedded [`NAME_SEPARATOR`], which
/// keeps [`split_composed_name`] the exact inverse of [`composed_name`].
pub fn validate_mcp_tool_name(name: &str) -> Result<(), McpToolNameError> {
    if name.is_empty() {
        return Err(McpToolNameError::Empty);
    }
    let length = name.chars().count();
    if length > MAX_MCP_TOOL_NAME_LEN {
        return Err(McpToolNameError::TooLong {
            limit: MAX_MCP_TOOL_NAME_LEN,
            length,
        });
    }
    if name.contains(NAME_SEPARATOR) {
        return Err(McpToolNameError::ContainsSeparator);
    }
    for character in name.chars() {
        if !(character.is_ascii_alphanumeric()
            || character == '-'
            || character == '_'
            || character == '.')
        {
            return Err(McpToolNameError::IllegalCharacter { character });
        }
    }
    Ok(())
}

/// Compose the wire name of an MCP tool, byte-identical to
/// [`McpTool::qualified_name`].
pub fn composed_name(server_name: &str, raw_tool_name: &str) -> String {
    format!("{server_name}{NAME_SEPARATOR}{raw_tool_name}")
}

/// Total parse of a composed wire name into `(server_name, raw_tool_name)`.
///
/// The split is taken at the *first* separator and is only accepted when the
/// leading segment is itself a valid server name. Because a valid server name can
/// never contain the separator, that leading segment is unique: `split_once` and
/// [`composed_name`] are exact inverses, and a name such as `terminal__run_command`
/// can only ever be read as server `terminal` plus tool `run_command` — which is
/// precisely the composition the policy refuses. Names with more than one
/// separator (`my__server__do_thing`) do not parse, and neither do names whose
/// leading segment is not a valid server name (`__x`, `a.b__c`, `path/x__y`).
pub fn split_composed_name(name: &str) -> Option<(&str, &str)> {
    let (server, tool) = name.split_once(NAME_SEPARATOR)?;
    // Both halves must independently satisfy the grammar: a name carrying more
    // than one separator can never round-trip a policy-valid registration, so it
    // never parses (and therefore never reaches any handler) rather than being
    // split ambiguously.
    if validate_server_name(server).is_err() || validate_mcp_tool_name(tool).is_err() {
        return None;
    }
    Some((server, tool))
}

/// `true` when `name` has the shape of a policy-valid composed MCP name.
pub fn is_composed_name_shape(name: &str) -> bool {
    split_composed_name(name).is_some()
}

/// The built-in tool name that an MCP composition would shadow, if any.
///
/// Two forms of collision are detected:
/// 1. the composed name *is* a built-in name (`terminal` + `run_command` \u{2192}
///    `terminal__run_command`);
/// 2. the server name reserves a prefix that owns built-in names (a server called
///    `terminal` may never register, even if a particular tool name happens not to
///    collide today).
pub fn builtin_shadow_target<'a>(
    qualified_name: &str,
    server_name: &str,
    builtin_tool_names: &[&'a str],
) -> Option<&'a str> {
    if let Some(exact) = builtin_tool_names
        .iter()
        .find(|name| *name == &qualified_name)
    {
        return Some(*exact);
    }
    let prefix = composed_name(server_name, "");
    builtin_tool_names
        .iter()
        .find(|name| name.starts_with(&prefix))
        .copied()
}

/// Registration-time verdict for one MCP tool.
///
/// Returns the composed wire name when the tool may be registered and dispatched,
/// or the reason it must be refused. `builtin_tool_names` is the harness's table of
/// names owned by the built-in dispatcher ([`crate::harness::BUILTIN_TOOL_NAMES`]).
pub fn validate_composed_registration(
    server_name: &str,
    raw_tool_name: &str,
    builtin_tool_names: &[&str],
) -> Result<String, McpNameError> {
    validate_server_name(server_name).map_err(McpNameError::Server)?;
    validate_mcp_tool_name(raw_tool_name).map_err(McpNameError::Tool)?;
    let qualified = composed_name(server_name, raw_tool_name);
    if let Some(builtin) = builtin_shadow_target(&qualified, server_name, builtin_tool_names) {
        return Err(McpNameError::BuiltInCollision {
            qualified,
            builtin: builtin.to_string(),
        });
    }
    Ok(qualified)
}

#[cfg(test)]
mod name_policy_tests {
    use super::*;
    use crate::tool_names::{
        TERMINAL_GLOB, TERMINAL_RUN_COMMAND, TOOL_GLOB, TOOL_GREP_SEARCH, TOOL_RUN_COMMAND,
    };

    /// Stand-in for the harness table so this module does not depend on the
    /// harness's live dispatcher (the harness table itself is exercised by
    /// `harness::tool_resolution_tests`).
    const FAKE_BUILTINS: &[&str] = &[
        TOOL_RUN_COMMAND,
        TOOL_GREP_SEARCH,
        TERMINAL_RUN_COMMAND,
        TERMINAL_GLOB,
    ];

    #[test]
    fn server_name_grammar_accepts_plain_identifiers() {
        for ok in ["filesystem", "my-server", "my_server", "S1", "a", "-", "_"] {
            assert!(
                validate_server_name(ok).is_ok(),
                "'{ok}' must be a valid server name"
            );
        }
    }

    #[test]
    fn server_name_grammar_rejects_empty_and_over_long() {
        assert_eq!(validate_server_name(""), Err(ServerNameError::Empty));
        let over = "a".repeat(MAX_SERVER_NAME_LEN + 1);
        assert_eq!(
            validate_server_name(&over),
            Err(ServerNameError::TooLong {
                limit: MAX_SERVER_NAME_LEN,
                length: MAX_SERVER_NAME_LEN + 1
            })
        );
        assert_eq!(
            validate_server_name(&"a".repeat(MAX_SERVER_NAME_LEN)),
            Ok(()),
            "exactly the bound is still accepted"
        );
    }

    #[test]
    fn server_name_rejects_separator_and_path_forms() {
        assert_eq!(
            validate_server_name("ter__minal"),
            Err(ServerNameError::ContainsSeparator)
        );
        assert_eq!(
            validate_server_name("my.server"),
            Err(ServerNameError::IllegalCharacter { character: '.' })
        );
        assert_eq!(
            validate_server_name("../evil"),
            Err(ServerNameError::IllegalCharacter { character: '.' })
        );
        assert_eq!(
            validate_server_name("a/b"),
            Err(ServerNameError::IllegalCharacter { character: '/' })
        );
        assert_eq!(
            validate_server_name("a\\b"),
            Err(ServerNameError::IllegalCharacter { character: '\\' })
        );
        assert_eq!(
            validate_server_name("my server"),
            Err(ServerNameError::IllegalCharacter { character: ' ' })
        );
        assert_eq!(
            validate_server_name("\u{0}"),
            Err(ServerNameError::IllegalCharacter { character: '\0' })
        );
    }

    #[test]
    fn tool_name_rejects_empty_separator_and_path_forms() {
        assert_eq!(validate_mcp_tool_name(""), Err(McpToolNameError::Empty));
        assert_eq!(
            validate_mcp_tool_name("do__thing"),
            Err(McpToolNameError::ContainsSeparator)
        );
        assert_eq!(
            validate_mcp_tool_name("do/thing"),
            Err(McpToolNameError::IllegalCharacter { character: '/' })
        );
        let over = "x".repeat(MAX_MCP_TOOL_NAME_LEN + 1);
        assert_eq!(
            validate_mcp_tool_name(&over),
            Err(McpToolNameError::TooLong {
                limit: MAX_MCP_TOOL_NAME_LEN,
                length: MAX_MCP_TOOL_NAME_LEN + 1
            })
        );
        assert!(validate_mcp_tool_name("do.thing").is_ok());
    }

    #[test]
    fn composed_split_is_total_and_unambiguous() {
        assert_eq!(
            split_composed_name("myserver__do_thing"),
            Some(("myserver", "do_thing"))
        );
        // The dangerous composition parses exactly as the built-in alias it spells.
        assert_eq!(
            split_composed_name(TERMINAL_RUN_COMMAND),
            Some(("terminal", TOOL_RUN_COMMAND))
        );
        // Multiple separators never parse: the leading segment can never be the
        // real server of a policy-valid registration.
        assert_eq!(split_composed_name("my__server__do_thing"), None);
        assert_eq!(split_composed_name("__run_command"), None);
        assert_eq!(split_composed_name("myserver__"), None);
        assert_eq!(split_composed_name("my.server__do_thing"), None);
        assert_eq!(split_composed_name("path/x__do_thing"), None);
        assert_eq!(split_composed_name(TOOL_RUN_COMMAND), None);
        assert!(!is_composed_name_shape(TOOL_RUN_COMMAND));
        assert!(is_composed_name_shape("myserver__do_thing"));
    }

    #[test]
    fn composed_round_trips_with_mcp_tool_qualified_name() {
        let tool = McpTool {
            name: "do_thing".to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            server_name: "myserver".to_string(),
        };
        assert_eq!(
            composed_name(&tool.server_name, &tool.name),
            tool.qualified_name()
        );
        assert_eq!(
            split_composed_name(&tool.qualified_name()),
            Some((tool.server_name.as_str(), tool.name.as_str()))
        );
    }

    #[test]
    fn registration_refuses_builtin_collisions() {
        // server `terminal` + tool `run_command` spells the built-in alias.
        let err = validate_composed_registration("terminal", TOOL_RUN_COMMAND, FAKE_BUILTINS)
            .expect_err("terminal__run_command must never be registrable");
        assert!(matches!(err, McpNameError::BuiltInCollision { .. }));
        assert!(err.to_string().contains("would shadow built-in tool"));

        // Any tool of a server that owns a built-in prefix is refused, so the
        // namespace itself is reserved.
        let err = validate_composed_registration("terminal", TOOL_GLOB, FAKE_BUILTINS)
            .expect_err("the `terminal__` namespace is reserved by the built-ins");
        assert!(matches!(err, McpNameError::BuiltInCollision { .. }));
    }

    #[test]
    fn registration_accepts_ordinary_namespaces() {
        assert_eq!(
            validate_composed_registration("myserver", "do_thing", FAKE_BUILTINS),
            Ok("myserver__do_thing".to_string())
        );
        assert_eq!(
            validate_composed_registration("filesystem", TOOL_GREP_SEARCH, FAKE_BUILTINS)
                .expect("a namespaced grep is not the built-in one"),
            composed_name("filesystem", TOOL_GREP_SEARCH)
        );
    }
}
