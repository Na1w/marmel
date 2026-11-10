use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::sync::Arc;

use crate::embed::SEMANTIC_SUPPORTED;
use crate::error::{Result, ZimSearchError};
use crate::index::IndexMode;
use crate::manager::{ArchiveManager, SemanticOptions, SemanticScope};
use crate::search::SearchMode;

/// Banner line describing the retrieval this binary can actually offer. An ONNX-free build (no
/// `semantic` Cargo feature) states plainly that it has no vector support and how to enable it; a
/// feature-on build keeps the "opt-in per archive" wording.
const DEFAULT_MODE_BANNER: &str = if SEMANTIC_SUPPORTED {
    "BM25 lexical (semantic vectors are opt-in per archive)"
} else {
    "BM25 lexical only - this build has NO dense-vector support; rebuild with `cargo build --features semantic` to enable semantic vectors"
};

/// Startup log note about the semantic pass, per build configuration.
const STARTUP_SEMANTIC_NOTE: &str = if SEMANTIC_SUPPORTED {
    "[MCP] Semantic dense vectors are NOT generated at startup (SemanticScope::None); opt in per archive through the 'index_archive' MCP tool with \"semantic\": true."
} else {
    "[MCP] Dense semantic vectors are unavailable in this build: it was compiled without the 'semantic' Cargo feature, so no embedding model exists and \"semantic\": true can never generate vectors. Rebuild with `cargo build --features semantic` to enable them; BM25 lexical indexing is unaffected."
};

/// `search` tool description: advertises only the retrieval this build performs.
const SEARCH_TOOL_DESCRIPTION: &str = if SEMANTIC_SUPPORTED {
    "Search across one or all ZIM archives. Retrieval mode is auto-detected by default: dense semantic vectors are used only when this archive's index already carries them, otherwise BM25 lexical keyword search is used. Supports pagination via offset and limit."
} else {
    "Search across one or all ZIM archives. This build retrieves LEXICALLY (BM25) only: it was compiled without the 'semantic' Cargo feature, so dense semantic vectors are neither generated nor consulted here (rebuild with `cargo build --features semantic` to enable vector retrieval). Supports pagination via offset and limit."
};

/// `search` -> `mode` property description.
const SEARCH_MODE_DESCRIPTION: &str = if SEMANTIC_SUPPORTED {
    "Retrieval mode. Omit for auto-detect (default): dense vectors when this index has them, otherwise BM25. 'bm25' = lexical keywords only. 'semantic' = dense vectors, and silently degrades to BM25 when the index has no vectors (generate them with 'index_archive' and \"semantic\": true). 'hybrid' = semantic + BM25, also degrading to BM25 when no vectors exist."
} else {
    "Retrieval mode. Every mode resolves to BM25 lexical search in THIS build because it has no dense-vector support (compiled without the 'semantic' Cargo feature; rebuild with `cargo build --features semantic`). 'bm25' = lexical keywords only; 'semantic' and 'hybrid' are accepted for compatibility and behave exactly like 'bm25'."
};

/// `index_archive` tool description.
const INDEX_TOOL_DESCRIPTION: &str = if SEMANTIC_SUPPORTED {
    "Index a single ZIM archive for search. Always builds the BM25 lexical Tantivy index (instant, < 0.1s); dense semantic vector embeddings are generated only when explicitly opted in with 'semantic': true."
} else {
    "Index a single ZIM archive for search. Builds the BM25 lexical Tantivy index (instant, < 0.1s) and nothing else: this binary was compiled without the 'semantic' Cargo feature, so it cannot generate dense semantic vector embeddings at all (rebuild with `cargo build --features semantic` to enable them)."
};

/// `index_archive` -> `semantic` property description. The property stays declared in both builds so
/// the advertised schema shape never changes; only its documented behaviour does.
const SEMANTIC_PROPERTY_DESCRIPTION: &str = if SEMANTIC_SUPPORTED {
    "Opt in to generating dense semantic vector embeddings for this archive. Default false."
} else {
    "UNAVAILABLE in this build: this server has no dense-vector support (compiled without the 'semantic' Cargo feature), so \"semantic\": true cannot generate embeddings and the call stays BM25-only - rebuild with `cargo build --features semantic` to make this property effective. Default false."
};

/// `index_archive` -> legacy `mode` property description.
const INDEX_MODE_DESCRIPTION: &str = if SEMANTIC_SUPPORTED {
    "Legacy alias kept for compatibility (default: 'bm25' = lexical only). 'semantic' and 'hybrid' are treated as opting in to the vector pass, equivalent to \"semantic\": true; pass 'bm25' or omit this property to skip vectors."
} else {
    "Legacy alias kept for compatibility (default: 'bm25' = lexical only). 'semantic' and 'hybrid' are accepted and treated as opting in to the vector pass, but this build has no dense-vector support (compiled without the 'semantic' Cargo feature), so they still produce a BM25-only index."
};

/// Non-fatal note appended to a **successful** `index_archive` result in an ONNX-free build, and it
/// belongs to exactly one case: the client never opted in to dense vectors explicitly, so the BM25-only
/// index that was delivered is precisely what it asked for and the absent vectors are only a note.
///
/// The other case is the opposite one: an EXPLICIT opt-in (`"semantic": true`, or the legacy `mode`
/// of `semantic`/`hybrid`) in the same build is refused with [`SEMANTIC_OPT_IN_UNSUPPORTED`] as an
/// error payload, matching the CLI and the library (audit A1) — whether or not an embedder happens to
/// be injected through [`ArchiveManager::set_embedder`], because the blocker is the build itself.
/// It therefore no longer invites a retry with `"semantic": true`: in this build that retry is refused.
const VECTOR_UNAVAILABLE_NOTE: &str = "semantic vectors: NOT generated because this binary was compiled without the 'semantic' Cargo feature and contains no embedding model - the BM25 index above remains usable, rebuild with `cargo build --features semantic` to obtain dense vectors; an explicit \"semantic\": true is refused as an error in this build";

/// The refusal an explicit vector opt-in gets from an ONNX-free build.
///
/// Same sentence the CLI fail-fast prints (`semantic_request_supported` in `main.rs`), so the CLI,
/// the library and the MCP protocol all refuse an unusable vector request in the same words — the
/// protocol must never report success where the other two surfaces report a failure (audit A1).
const SEMANTIC_OPT_IN_UNSUPPORTED: &str =
    "Semantic vector indexing is not available in this build: recompile with `cargo build --features semantic`.";

/// Server usage instructions advertised in the MCP `initialize` response.
pub const MCP_SERVER_INSTRUCTIONS: &str = "Local offline ZIM & Tantivy BM25 search engine over indexed knowledge archives (Wikipedia, StackOverflow, Linux/Arch wikis, documentation, Gutenberg, etc.). Always prefer querying this server FIRST for research, documentation, specifications, algorithms, and reference lookups before falling back to external web lookups or curl. Workflow: call `list_archives` to inspect available archives, `search` (fast BM25 lexical search; pass `archive` from `list_archives` to target a specific archive or omit `archive` to search across all archives), and `read_article` (paginated via `offset_chars` and `limit_chars`) as your primary reference source.";

/// JSON-RPC 2.0 Request representation.
#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

/// Run the stdio Model Context Protocol (MCP) server loop.
pub fn run_mcp_server(manager: ArchiveManager, auto_index: bool) -> Result<()> {
    let manager = Arc::new(manager);
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    eprintln!("============================================================");
    eprintln!("  ZIM-Search Model Context Protocol (MCP) Server");
    eprintln!("  ZIM directory:   {}", manager.zim_dir().display());
    eprintln!("  Index directory: {}", manager.index_dir().display());
    eprintln!("  Auto-indexing:   {}", auto_index);
    eprintln!("  Default mode:    {}", DEFAULT_MODE_BANNER);
    eprintln!("  Listening for JSON-RPC 2.0 messages on stdin...");
    eprintln!("============================================================");
    log_vector_capability();

    if auto_index {
        eprintln!(
            "[MCP] Checking for unindexed archives on startup in '{}' (BM25 lexical pass only)...",
            manager.zim_dir().display()
        );
        eprintln!("{}", STARTUP_SEMANTIC_NOTE);
        // Startup is deliberately BM25-only: dense embedding work is never triggered implicitly.
        match manager.ensure_all_indexed(SemanticScope::None, &SemanticOptions::default()) {
            Ok(indexed) => eprintln!(
                "[MCP] Startup indexing complete: {} archive(s) newly BM25-indexed, 0 archives embedded (semantic vectors skipped).",
                indexed
            ),
            Err(e) => eprintln!("[MCP] Startup indexing error: {}", e),
        }
    }

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[MCP] Stdin read error: {}", e);
                break;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(req) => req,
            Err(e) => {
                eprintln!("[MCP] Failed to parse JSON-RPC request: {}", e);
                let err_res = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": {
                        "code": -32700,
                        "message": format!("Parse error: {}", e)
                    }
                });
                let _ = writeln!(stdout, "{}", serde_json::to_string(&err_res).unwrap());
                let _ = stdout.flush();
                continue;
            }
        };

        // Notifications (id == None) do not expect a response
        let is_notification = request.id.is_none();
        let id_val = request.id.clone().unwrap_or(Value::Null);

        match request.method.as_str() {
            "initialize" => {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": id_val,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {
                            "tools": {}
                        },
                        "serverInfo": {
                            "name": "zim-search",
                            "version": "0.1.0"
                        },
                        "instructions": MCP_SERVER_INSTRUCTIONS
                    }
                });
                send_response(&mut stdout, &response)?;
            }

            "notifications/initialized" => {
                eprintln!("[MCP] Client successfully initialized session.");
            }

            "ping" => {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": id_val,
                    "result": {}
                });
                send_response(&mut stdout, &response)?;
            }

            "tools/list" => {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": id_val,
                    "result": {
                        "tools": get_tool_definitions()
                    }
                });
                send_response(&mut stdout, &response)?;
            }

            "tools/call" => {
                let tool_name = request
                    .params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let arguments = request
                    .params
                    .get("arguments")
                    .cloned()
                    .unwrap_or(Value::Null);

                let (text_output, is_error) =
                    handle_tool_call(&manager, tool_name, arguments, auto_index);

                let response = json!({
                    "jsonrpc": "2.0",
                    "id": id_val,
                    "result": {
                        "content": [
                            {
                                "type": "text",
                                "text": text_output
                            }
                        ],
                        "isError": is_error
                    }
                });
                send_response(&mut stdout, &response)?;
            }

            other => {
                eprintln!("[MCP] Unrecognized method: {}", other);
                if !is_notification {
                    let response = json!({
                        "jsonrpc": "2.0",
                        "id": id_val,
                        "error": {
                            "code": -32601,
                            "message": format!("Method '{}' not found", other)
                        }
                    });
                    send_response(&mut stdout, &response)?;
                }
            }
        }
    }

    eprintln!("[MCP] Stdin closed, server terminating.");
    Ok(())
}

fn send_response(stdout: &mut io::Stdout, response: &Value) -> Result<()> {
    let serialized = serde_json::to_string(response)?;
    writeln!(stdout, "{}", serialized)?;
    stdout.flush()?;
    Ok(())
}

fn get_tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "list_archives",
            "description": "List available ZIM archives in the configured directory, with pagination (offset and limit).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "offset": {
                        "type": "integer",
                        "description": "Pagination offset (default: 0)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of archives to return (default: 50)"
                    }
                }
            }
        }),
        json!({
            "name": "list_articles",
            "description": "List articles and entries inside a specific ZIM archive with pagination and optional substring filter.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "archive": {
                        "type": "string",
                        "description": "Name or filename of the archive to inspect"
                    },
                    "filter": {
                        "type": "string",
                        "description": "Optional substring filter matching article URL or title"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Pagination offset (default: 0)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of entries to return (default: 30, max: 200)"
                    }
                },
                "required": ["archive"]
            }
        }),
        json!({
            "name": "search",
            "description": SEARCH_TOOL_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query or natural language question"
                    },
                    "archive": {
                        "type": "string",
                        "description": "Optional specific archive name or filename to search. If omitted, searches across all available archives in the directory."
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["semantic", "hybrid", "bm25"],
                        "description": SEARCH_MODE_DESCRIPTION
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Pagination offset (start rank index, default: 0)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of results to return per page (default: 5, max: 50)"
                    }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "read_article",
            "description": "Read article content from a ZIM archive with character-based pagination (offset_chars and limit_chars) to prevent overflowing context.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "archive": {
                        "type": "string",
                        "description": "Name or filename of the archive containing the article"
                    },
                    "url": {
                        "type": "string",
                        "description": "Canonical URL or path of the article (e.g. 'mariokartwii.com/ppc/ch24.html')"
                    },
                    "offset_chars": {
                        "type": "integer",
                        "description": "Character offset to start reading from (default: 0)"
                    },
                    "limit_chars": {
                        "type": "integer",
                        "description": "Maximum characters to return (default: 8000, max: 50000)"
                    },
                    "raw": {
                        "type": "boolean",
                        "description": "If true, returns raw HTML instead of cleaned plain text (default: false)"
                    }
                },
                "required": ["archive", "url"]
            }
        }),
        json!({
            "name": "index_archive",
            "description": INDEX_TOOL_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "archive": {
                        "type": "string",
                        "description": "Name or filename of the archive to index"
                    },
                    "semantic": {
                        "type": "boolean",
                        "description": SEMANTIC_PROPERTY_DESCRIPTION
                    },
                    "semantic_options": {
                        "type": "object",
                        "description": "Optional cost knobs for the semantic pass, used only when 'semantic' is true.",
                        "properties": {
                            "lead_only": {
                                "type": "boolean",
                                "description": "Embed only the lead chunk of each article instead of every chunk (default: false)"
                            },
                            "max_chunks": {
                                "type": "integer",
                                "description": "Hard cap on the number of vectors generated for this archive (default: unbounded)"
                            },
                            "batch_size": {
                                "type": "integer",
                                "description": "Number of passages sent to the embedder per batch (default: 256)"
                            }
                        }
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["bm25", "semantic", "hybrid"],
                        "description": INDEX_MODE_DESCRIPTION
                    }
                },
                "required": ["archive"]
            }
        }),
        json!({
            "name": "get_archive_info",
            "description": "Retrieve comprehensive metadata and stats for a specific ZIM archive.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "archive": {
                        "type": "string",
                        "description": "Name or filename of the archive"
                    }
                },
                "required": ["archive"]
            }
        }),
    ]
}

/// Build [`SemanticOptions`] from the optional `semantic_options` object of an `index_archive` call.
///
/// Malformed or out-of-range values are ignored and fall back to the manager defaults, so a bad hint
/// can never turn into a failed indexing call.
fn parse_semantic_options(arguments: &Value) -> SemanticOptions {
    let object = arguments.get("semantic_options");
    let field = |key: &str| object.and_then(|opts| opts.get(key));

    SemanticOptions {
        lead_only: field("lead_only")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        max_chunks: field("max_chunks")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize),
        // A zero batch size would degenerate the embedder loop, so it is treated as "unset".
        batch_size: field("batch_size")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .filter(|v| *v > 0),
    }
}

/// Log what this build can do with dense semantic vectors, straight after the startup banner.
///
/// A feature-on build keeps the existing "opt-in per archive" framing; a feature-off build is told
/// explicitly that the binary carries no vector support at all and which command enables it — an MCP
/// client must never be left to discover that only after a failed `"semantic": true` call.
fn log_vector_capability() {
    if SEMANTIC_SUPPORTED {
        eprintln!(
            "[MCP] Vector capability: dense semantic vectors are compiled in and strictly opt-in \
             per archive (use the 'index_archive' tool with \"semantic\": true)."
        );
    } else {
        eprintln!(
            "[MCP] Vector capability: NONE - this binary was compiled without the 'semantic' Cargo \
             feature, so it contains no embedding model and cannot generate or search dense vectors. \
             Rebuild with `{}` to enable them; BM25 lexical search is unaffected.",
            crate::embed::SEMANTIC_REBUILD_COMMAND
        );
    }
}

/// Render a failed semantic (vector) pass as an actionable, non-fatal note.
///
/// The BM25 index built before the vector pass stays usable, so the tool result remains a success
/// payload that states plainly whether vectors exist and why they do not.
fn semantic_failure_note(error: &ZimSearchError) -> String {
    match error {
        // The binary simply has no embedder at all (what `default_embedder()` reports in an
        // ONNX-free build). Say that plainly instead of blaming an ONNX model download failure, and
        // name the rebuild command that would make the retry meaningful.
        ZimSearchError::Embedding(detail) if detail.contains("without the 'semantic' feature") => {
            format!(
                "semantic vectors: NOT generated because this binary was compiled without the 'semantic' Cargo feature and contains no embedding model - {} ; the BM25 index above remains usable, rebuild with `{}` and retry with \"semantic\": true",
                detail,
                crate::embed::SEMANTIC_REBUILD_COMMAND
            )
        }
        ZimSearchError::Embedding(detail) => format!(
            "semantic vectors: NOT generated because the local ONNX embedding model could not be loaded or run (offline model download failure?) - {} ; the BM25 index above remains usable, retry with \"semantic\": true once the model is available",
            detail
        ),
        other => format!(
            "semantic vectors: NOT generated ({}) ; the BM25 index above remains usable",
            other
        ),
    }
}

/// Whether an `index_archive` call asks for dense vectors **explicitly**: `"semantic": true`, or the
/// legacy `mode` property set to `semantic`/`hybrid` (its documented alias for that same opt-in).
/// Omitting both properties — or passing `bm25` / an unparseable mode — is an implicit lexical-only
/// request, which is a different case for reporting purposes (see [`vector_opt_in_report`]).
fn explicit_vector_opt_in(arguments: &Value) -> bool {
    let legacy_mode = arguments
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("bm25")
        .parse::<IndexMode>()
        .unwrap_or(IndexMode::BM25Only);

    arguments
        .get("semantic")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || matches!(legacy_mode, IndexMode::SemanticOnly | IndexMode::Hybrid)
}

/// How an ONNX-free build must report the vector part of an `index_archive` request: the text to
/// append to the result plus the MCP `isError` flag. `None` means "this build can and did run the
/// vector pass" (the `semantic` feature is compiled in), so the caller reports the real per-archive
/// outcome instead — including the non-fatal `Ok(0)` case where vectors are genuinely expected to come
/// from an index that already carries them on disk.
///
/// * EXPLICIT opt-in — refused as an error with [`SEMANTIC_OPT_IN_UNSUPPORTED`]. An embedder injected
///   through [`ArchiveManager::set_embedder`] cannot rescue the request: the reason for the refusal is
///   the build (no embedding model is linked in), not the injectable embedder (audit A1 — the CLI, the
///   library and the protocol must agree).
/// * IMPLICIT request (no explicit opt-in) — stays a success carrying [`VECTOR_UNAVAILABLE_NOTE`],
///   because a BM25-only index is exactly what the client asked for.
fn vector_opt_in_report(explicit_opt_in: bool) -> Option<(&'static str, bool)> {
    if SEMANTIC_SUPPORTED {
        return None;
    }

    if explicit_opt_in {
        Some((SEMANTIC_OPT_IN_UNSUPPORTED, true))
    } else {
        Some((VECTOR_UNAVAILABLE_NOTE, false))
    }
}

fn handle_tool_call(
    manager: &ArchiveManager,
    tool_name: &str,
    arguments: Value,
    default_auto_index: bool,
) -> (String, bool) {
    match tool_name {
        "list_archives" => {
            let offset = arguments
                .get("offset")
                .and_then(|v| v.as_u64())
                .map(|o| o as usize)
                .unwrap_or(0);
            let limit = arguments
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|l| l as usize)
                .unwrap_or(50)
                .clamp(1, 200);

            match manager.list_archives() {
                Ok(archives) => {
                    let total = archives.len();
                    let page: Vec<_> = archives.into_iter().skip(offset).take(limit).collect();
                    let has_more = offset.saturating_add(page.len()) < total;
                    let payload = json!({
                        "total_archives": total,
                        "offset": offset,
                        "limit": limit,
                        "has_more": has_more,
                        "archives": page,
                    });
                    let json_text =
                        serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string());
                    (json_text, false)
                }
                Err(e) => (format!("Failed to list archives: {}", e), true),
            }
        }

        "list_articles" => {
            let archive_target = arguments
                .get("archive")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if archive_target.is_empty() {
                return ("Error: 'archive' parameter is required".to_string(), true);
            }

            let filter = arguments.get("filter").and_then(|v| v.as_str());
            let offset = arguments
                .get("offset")
                .and_then(|v| v.as_u64())
                .map(|o| o as usize)
                .unwrap_or(0);
            let limit = arguments
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|l| l as usize)
                .unwrap_or(30)
                .clamp(1, 200);

            match manager.list_articles_paginated(archive_target, filter, offset, limit) {
                Ok(res) => {
                    let json_text =
                        serde_json::to_string_pretty(&res).unwrap_or_else(|_| "{}".to_string());
                    (json_text, false)
                }
                Err(e) => (format!("Failed to list articles: {}", e), true),
            }
        }

        "search" => {
            let query = arguments
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if query.trim().is_empty() {
                return ("Error: 'query' parameter is required".to_string(), true);
            }

            let archive_target = arguments.get("archive").and_then(|v| v.as_str());
            let mode = arguments
                .get("mode")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<SearchMode>().ok())
                .or(Some(SearchMode::BM25));
            let offset = arguments
                .get("offset")
                .and_then(|v| v.as_u64())
                .map(|o| o as usize)
                .unwrap_or(0);
            let limit = arguments
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|l| l as usize)
                .unwrap_or(5)
                .clamp(1, 100);

            match manager.search(
                archive_target,
                query,
                mode,
                offset,
                limit,
                default_auto_index,
            ) {
                Ok(results) => {
                    let json_text =
                        serde_json::to_string_pretty(&results).unwrap_or_else(|_| "{}".to_string());
                    (json_text, false)
                }
                Err(e) => (format!("Search failed: {}", e), true),
            }
        }

        "read_article" => {
            let archive_target = arguments
                .get("archive")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let url = arguments.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let raw = arguments
                .get("raw")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let offset_chars = arguments
                .get("offset_chars")
                .and_then(|v| v.as_u64())
                .map(|o| o as usize)
                .unwrap_or(0);
            let limit_chars = arguments
                .get("limit_chars")
                .and_then(|v| v.as_u64())
                .map(|l| l as usize)
                .unwrap_or(8000)
                .clamp(100, 100_000);

            if archive_target.is_empty() || url.is_empty() {
                return (
                    "Error: both 'archive' and 'url' parameters are required".to_string(),
                    true,
                );
            }

            match manager.read_article_paginated(
                archive_target,
                url,
                raw,
                offset_chars,
                limit_chars,
            ) {
                Ok(res) => {
                    let json_text =
                        serde_json::to_string_pretty(&res).unwrap_or_else(|_| "{}".to_string());
                    (json_text, false)
                }
                Err(e) => (format!("Failed to read article: {}", e), true),
            }
        }

        "index_archive" => {
            let archive_target = arguments
                .get("archive")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if archive_target.is_empty() {
                return ("Error: 'archive' parameter is required".to_string(), true);
            }

            // Dense semantic vectors are strictly opt-in per archive: the default is BM25-only
            // lexical indexing. The legacy 'mode' property is kept as an alias, defaulting to bm25.
            let semantic_opt_in = explicit_vector_opt_in(&arguments);
            let semantic_opts = parse_semantic_options(&arguments);

            match manager.build_tantivy_for_archive(archive_target, false) {
                Ok(chunks) => {
                    let mut msg = format!(
                        "Built disk-backed Tantivy BM25 index for '{}' ({} chunks)",
                        archive_target, chunks
                    );
                    // The build configuration settles the vector part before anything else runs:
                    // an EXPLICIT opt-in in an ONNX-free build is refused as an error, an IMPLICIT
                    // request in that build stays a success with a note, and a feature-on build
                    // reports `None` here and runs the real vector pass below.
                    if let Some((note, is_error)) = vector_opt_in_report(semantic_opt_in) {
                        msg.push_str(if is_error { ". " } else { ", " });
                        msg.push_str(note);
                        return (msg, is_error);
                    }

                    if semantic_opt_in {
                        // Feature-on build: the vector pass genuinely runs, so its failure is a
                        // NON-fatal note (the BM25 stage above succeeded), and `Ok(0)` below is the
                        // case where vectors are expected to already exist in the on-disk index.
                        // Per-source targeting: exactly one archive per call, i.e. the equivalent of
                        // SemanticScope::Only(vec![archive_target]).
                        match manager.populate_tantivy_semantic_for_archive(
                            archive_target,
                            false,
                            &semantic_opts,
                        ) {
                            Ok(0) => msg.push_str(
                                ", semantic vectors: 0 generated (this archive already carries vectors on disk)",
                            ),
                            Ok(vec_count) => {
                                msg.push_str(&format!(", {} semantic vectors generated", vec_count))
                            }
                            Err(e) => msg.push_str(&format!(", {}", semantic_failure_note(&e))),
                        }
                    } else {
                        // Feature-on build with no explicit opt-in: lexical-only by the client's own
                        // choice, so the vector pass is simply skipped (non-fatal, unchanged).
                        msg.push_str(
                            ", semantic vectors: skipped (BM25 only); set \"semantic\": true to generate dense embeddings for this archive",
                        );
                    }
                    (msg, false)
                }
                Err(e) => {
                    // The BM25 stage itself failed, and that is the primary error. In an ONNX-free
                    // build an explicit vector opt-in is additionally named, so the client learns in
                    // one round trip that the requested vectors could never have been produced here.
                    let mut msg = format!("Indexing failed: {}", e);
                    if let Some((note, true)) = vector_opt_in_report(semantic_opt_in) {
                        msg.push_str(" ; ");
                        msg.push_str(note);
                    }
                    (msg, true)
                }
            }
        }

        "get_archive_info" => {
            let archive_target = arguments
                .get("archive")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if archive_target.is_empty() {
                return ("Error: 'archive' parameter is required".to_string(), true);
            }

            match manager.get_archive(archive_target) {
                Ok(archive) => {
                    let meta = archive.metadata();
                    let json_text =
                        serde_json::to_string_pretty(&meta).unwrap_or_else(|_| "{}".to_string());
                    (json_text, false)
                }
                Err(e) => (format!("Failed to get archive info: {}", e), true),
            }
        }

        unknown => (format!("Unknown tool: '{}'", unknown), true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns the advertised definition for a tool name, panicking if it is missing.
    fn tool(name: &str) -> Value {
        get_tool_definitions()
            .into_iter()
            .find(|t| t.get("name").and_then(|v| v.as_str()) == Some(name))
            .unwrap_or_else(|| panic!("tool '{}' must be advertised", name))
    }

    /// An [`ArchiveManager`] over an empty, throw-away ZIM/index directory pair.
    fn test_manager() -> (ArchiveManager, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ArchiveManager::new(dir.path().join("zims"), dir.path().join("indices"))
            .expect("manager");
        (manager, dir)
    }

    #[test]
    fn index_archive_schema_declares_semantic_opt_in_properties() {
        let t = tool("index_archive");
        let props = t["inputSchema"]["properties"].clone();

        assert_eq!(props["semantic"]["type"].as_str(), Some("boolean"));
        let semantic_desc = props["semantic"]["description"].as_str().unwrap();
        assert!(semantic_desc.contains("Default false"), "{}", semantic_desc);

        let sem_opts = &props["semantic_options"];
        assert_eq!(sem_opts["type"].as_str(), Some("object"));
        assert_eq!(
            sem_opts["properties"]["lead_only"]["type"].as_str(),
            Some("boolean")
        );
        assert_eq!(
            sem_opts["properties"]["max_chunks"]["type"].as_str(),
            Some("integer")
        );
        assert_eq!(
            sem_opts["properties"]["batch_size"]["type"].as_str(),
            Some("integer")
        );
    }

    #[test]
    fn index_archive_schema_no_longer_defaults_to_hybrid() {
        let t = tool("index_archive");
        let blob = t.to_string();
        assert!(!blob.contains("default: 'hybrid'"));
        assert!(!blob.contains("hybrid' for neural vector"));

        let mode_props = &t["inputSchema"]["properties"]["mode"];
        assert_eq!(mode_props["enum"][0].as_str(), Some("bm25"));
        let mode_desc = mode_props["description"].as_str().unwrap();
        assert!(mode_desc.contains("default: 'bm25'"), "{}", mode_desc);
    }

    #[cfg(feature = "semantic")]
    #[test]
    fn search_schema_advertises_auto_detect_instead_of_semantic_default() {
        let t = tool("search");
        let description = t["description"].as_str().unwrap();
        let mode_desc = t["inputSchema"]["properties"]["mode"]["description"]
            .as_str()
            .unwrap();

        assert!(!description.contains("semantic vector embeddings (default)"));
        assert!(description.contains("auto-detected"), "{}", description);
        assert!(mode_desc.contains("Omit for auto-detect"), "{}", mode_desc);
        assert!(mode_desc.contains("degrades to BM25"), "{}", mode_desc);
        assert!(
            !mode_desc.contains("dense vectors, default"),
            "{}",
            mode_desc
        );
    }

    /// Feature-off build (the default): the `search` tool must advertise lexical-only retrieval and
    /// must never promise vector auto-detection this binary cannot perform.
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn search_schema_states_lexical_only_retrieval_in_an_onnx_free_build() {
        let t = tool("search");
        let description = t["description"].as_str().unwrap();
        let mode_desc = t["inputSchema"]["properties"]["mode"]["description"]
            .as_str()
            .unwrap();

        assert!(!description.contains("semantic vector embeddings (default)"));
        assert!(
            description.contains("LEXICALLY (BM25) only"),
            "{}",
            description
        );
        assert!(
            !description.contains("auto-detected"),
            "vector auto-detect must not be advertised without the feature: {}",
            description
        );
        assert!(
            description.contains("cargo build --features semantic"),
            "{}",
            description
        );
        assert!(
            mode_desc.contains("BM25 lexical search in THIS build"),
            "{}",
            mode_desc
        );
    }

    /// Feature-off build: the `semantic` property keeps its shape but is documented as unavailable.
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn index_archive_schema_reports_semantic_as_unavailable_in_an_onnx_free_build() {
        let t = tool("index_archive");
        let props = t["inputSchema"]["properties"].clone();

        // Schema shape stays stable across builds: the property is still a boolean.
        assert_eq!(props["semantic"]["type"].as_str(), Some("boolean"));
        let semantic_desc = props["semantic"]["description"].as_str().unwrap();
        assert!(
            semantic_desc.contains("UNAVAILABLE in this build"),
            "{semantic_desc}"
        );
        assert!(
            semantic_desc.contains("cargo build --features semantic"),
            "{semantic_desc}"
        );
        assert!(semantic_desc.contains("Default false"), "{semantic_desc}");

        let description = t["description"].as_str().unwrap();
        assert!(
            description.contains("cannot generate dense semantic vector embeddings"),
            "{description}"
        );
        assert!(
            description.contains("cargo build --features semantic"),
            "{description}"
        );

        let mode_desc = props["mode"]["description"].as_str().unwrap();
        assert!(mode_desc.contains("default: 'bm25'"), "{mode_desc}");
        assert!(mode_desc.contains("no dense-vector support"), "{mode_desc}");
    }

    #[test]
    fn vector_unavailable_note_names_the_rebuild_and_keeps_bm25_usable() {
        assert!(VECTOR_UNAVAILABLE_NOTE.contains("NOT generated"));
        assert!(VECTOR_UNAVAILABLE_NOTE.contains("without the 'semantic' Cargo feature"));
        assert!(
            VECTOR_UNAVAILABLE_NOTE.contains(crate::embed::SEMANTIC_REBUILD_COMMAND),
            "{VECTOR_UNAVAILABLE_NOTE}"
        );
        assert!(VECTOR_UNAVAILABLE_NOTE.contains("BM25 index above remains usable"));
    }

    /// Audit A11 drift guard: every hand-maintained MCP banner/notice literal must keep quoting the
    /// canonical rebuild command owned by [`crate::embed`]. These are build-selected `const`s, and a
    /// Rust `const &str` cannot be assembled out of another `const` (no `concat!` over consts, no
    /// `format!` in const context), so the pin is a test rather than a composition: reword one of these
    /// messages so that it drops the command, and this test fails instead of the CLI, the library and
    /// the protocol wording silently drifting apart.
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn every_mcp_build_honesty_literal_quotes_the_canonical_rebuild_command() {
        // The protocol refusal must be the canonical sentence, word for word (audit A1 agreement).
        assert_eq!(
            SEMANTIC_OPT_IN_UNSUPPORTED,
            crate::embed::SEMANTIC_FEATURE_UNAVAILABLE
        );

        for (site, text) in [
            ("DEFAULT_MODE_BANNER", DEFAULT_MODE_BANNER),
            ("STARTUP_SEMANTIC_NOTE", STARTUP_SEMANTIC_NOTE),
            ("SEARCH_TOOL_DESCRIPTION", SEARCH_TOOL_DESCRIPTION),
            ("SEARCH_MODE_DESCRIPTION", SEARCH_MODE_DESCRIPTION),
            ("INDEX_TOOL_DESCRIPTION", INDEX_TOOL_DESCRIPTION),
            (
                "SEMANTIC_PROPERTY_DESCRIPTION",
                SEMANTIC_PROPERTY_DESCRIPTION,
            ),
            ("VECTOR_UNAVAILABLE_NOTE", VECTOR_UNAVAILABLE_NOTE),
        ] {
            assert!(
                text.contains(crate::embed::SEMANTIC_REBUILD_COMMAND),
                "{site} no longer quotes the canonical rebuild command: {text}"
            );
        }

        // `INDEX_MODE_DESCRIPTION` is the one literal that never named the command (it explains that
        // the legacy alias still yields a BM25-only index); it must still say why.
        assert!(
            INDEX_MODE_DESCRIPTION.contains("no dense-vector support"),
            "{INDEX_MODE_DESCRIPTION}"
        );
    }

    /// Feature-off build: an explicit `"semantic": true` is refused with the CLI's fail-fast sentence
    /// instead of being reported as a success payload (audit A1 — CLI, library and protocol agree).
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn an_explicit_vector_opt_in_is_refused_rather_than_reported_as_success() {
        // The wording is the CLI's own sentence, on one line, and it names the rebuild. Pinned to the
        // canonical sentence rather than re-typed here (audit A11): the equality is what keeps the CLI,
        // the library and the protocol saying the same thing.
        assert_eq!(
            SEMANTIC_OPT_IN_UNSUPPORTED,
            crate::embed::SEMANTIC_FEATURE_UNAVAILABLE
        );
        assert!(
            SEMANTIC_OPT_IN_UNSUPPORTED.contains(crate::embed::SEMANTIC_REBUILD_COMMAND),
            "{SEMANTIC_OPT_IN_UNSUPPORTED}"
        );
        assert!(!SEMANTIC_OPT_IN_UNSUPPORTED.contains('\n'));

        // What an ONNX-free build really offers: no embedder at all. The handler no longer *probes*
        // for one (an injected embedder is refused as well — see
        // `an_injected_embedder_does_not_downgrade_the_vector_refusal`), but this pins why the vectors
        // named in the refusal sentence can never exist in this build.
        let (manager, _dir) = test_manager();
        let err = match manager.get_or_init_embedder() {
            Ok(_) => panic!("an ONNX-free build must not construct the built-in embedder"),
            Err(err) => err,
        };
        let note = semantic_failure_note(&err);
        assert!(
            note.contains("compiled without the 'semantic' Cargo feature"),
            "{note}"
        );
        assert!(
            note.contains(crate::embed::SEMANTIC_REBUILD_COMMAND),
            "{note}"
        );

        // Refusing the call must not change the advertised schema shape: the property is still a
        // boolean, documented as unavailable in this build.
        let props = tool("index_archive")["inputSchema"]["properties"].clone();
        assert_eq!(props["semantic"]["type"].as_str(), Some("boolean"));
        let desc = props["semantic"]["description"].as_str().unwrap();
        assert!(desc.contains("UNAVAILABLE in this build"), "{desc}");
    }

    #[test]
    fn semantic_failure_note_separates_a_missing_feature_from_a_model_failure() {
        // What `default_embedder()` reports in an ONNX-free build (the wording is its own, anchored to
        // `crate::embed::SEMANTIC_REBUILD_COMMAND`, and pinned verbatim by the `embed`/`manager`
        // tests): never rendered as a download issue.
        let feature_note = semantic_failure_note(&ZimSearchError::Embedding(format!(
            "this binary was built without the 'semantic' feature; rebuild with `{}` to enable dense vector embeddings",
            crate::embed::SEMANTIC_REBUILD_COMMAND
        )));
        assert!(
            feature_note.contains("compiled without the 'semantic' Cargo feature"),
            "{feature_note}"
        );
        assert!(
            feature_note.contains(crate::embed::SEMANTIC_REBUILD_COMMAND),
            "{feature_note}"
        );
        assert!(
            feature_note.contains("BM25 index above remains usable"),
            "{feature_note}"
        );
        assert!(
            !feature_note.contains("offline model download failure"),
            "{feature_note}"
        );

        // A genuine model failure keeps the ONNX/download wording.
        let model_note = semantic_failure_note(&ZimSearchError::Embedding(
            "Failed to initialize embedder: network unreachable".to_string(),
        ));
        assert!(model_note.contains("ONNX"), "{model_note}");
        assert!(model_note.contains("network unreachable"), "{model_note}");
    }

    #[test]
    fn startup_strings_describe_the_build_that_is_actually_running() {
        if SEMANTIC_SUPPORTED {
            assert_eq!(
                DEFAULT_MODE_BANNER,
                "BM25 lexical (semantic vectors are opt-in per archive)"
            );
            assert!(
                STARTUP_SEMANTIC_NOTE.contains("opt in per archive"),
                "{STARTUP_SEMANTIC_NOTE}"
            );
        } else {
            assert!(
                DEFAULT_MODE_BANNER.contains("NO dense-vector support"),
                "{DEFAULT_MODE_BANNER}"
            );
            assert!(
                DEFAULT_MODE_BANNER.contains(crate::embed::SEMANTIC_REBUILD_COMMAND),
                "{DEFAULT_MODE_BANNER}"
            );
            assert!(
                STARTUP_SEMANTIC_NOTE.contains("no embedding model"),
                "{STARTUP_SEMANTIC_NOTE}"
            );
            assert!(
                STARTUP_SEMANTIC_NOTE.contains(crate::embed::SEMANTIC_REBUILD_COMMAND),
                "{STARTUP_SEMANTIC_NOTE}"
            );
        }
    }

    #[test]
    fn semantic_options_map_onto_manager_semantic_options() {
        let args = json!({
            "semantic_options": { "lead_only": true, "max_chunks": 12, "batch_size": 4 }
        });
        let opts = parse_semantic_options(&args);
        assert!(opts.lead_only);
        assert_eq!(opts.max_chunks, Some(12));
        assert_eq!(opts.batch_size, Some(4));
    }

    #[test]
    fn semantic_options_default_when_absent_or_malformed() {
        let absent = parse_semantic_options(&json!({}));
        assert!(!absent.lead_only);
        assert_eq!(absent.max_chunks, None);
        assert_eq!(absent.batch_size, None);

        let malformed = parse_semantic_options(&json!({
            "semantic_options": { "lead_only": "yes", "max_chunks": -3, "batch_size": 0 }
        }));
        assert!(!malformed.lead_only);
        assert_eq!(malformed.max_chunks, None);
        assert_eq!(
            malformed.batch_size, None,
            "a zero batch size must fall back to the default"
        );
    }

    #[test]
    fn semantic_failure_note_is_actionable_and_keeps_bm25_usable() {
        let embedder_note = semantic_failure_note(&ZimSearchError::Embedding(
            "Failed to initialize embedder: network unreachable".to_string(),
        ));
        assert!(embedder_note.contains("NOT generated"));
        assert!(embedder_note.contains("ONNX"));
        assert!(embedder_note.contains("network unreachable"));
        assert!(embedder_note.contains("BM25 index above remains usable"));

        let other_note = semantic_failure_note(&ZimSearchError::Other("boom".to_string()));
        assert!(other_note.contains("boom"));
        assert!(other_note.contains("BM25 index above remains usable"));
    }

    #[test]
    fn index_archive_still_validates_the_archive_argument() {
        let (manager, _dir) = test_manager();
        let (text, is_error) = handle_tool_call(&manager, "index_archive", json!({}), false);
        assert!(is_error);
        assert!(text.contains("'archive' parameter is required"), "{}", text);
    }

    #[test]
    fn index_archive_without_semantic_opt_in_never_touches_the_embedder() {
        let (manager, _dir) = test_manager();
        let (text, is_error) = handle_tool_call(
            &manager,
            "index_archive",
            json!({ "archive": "does_not_exist.zim" }),
            false,
        );
        assert!(is_error);
        assert!(text.contains("Indexing failed"), "{}", text);
        assert!(
            !text.contains("embedder"),
            "no embedding work expected: {}",
            text
        );
    }

    /// Both builds: what counts as an EXPLICIT dense-vector opt-in, i.e. the requests that an
    /// ONNX-free build must refuse as errors rather than answer with a note.
    #[test]
    fn explicit_vector_opt_in_covers_the_semantic_flag_and_the_legacy_mode_alias() {
        assert!(explicit_vector_opt_in(&json!({ "semantic": true })));
        assert!(explicit_vector_opt_in(&json!({ "mode": "semantic" })));
        assert!(explicit_vector_opt_in(&json!({ "mode": "hybrid" })));
        assert!(explicit_vector_opt_in(
            &json!({ "semantic": false, "mode": "hybrid" })
        ));

        // Everything else is an implicit, lexical-only request and must stay non-fatal.
        assert!(!explicit_vector_opt_in(&json!({})));
        assert!(!explicit_vector_opt_in(&json!({ "semantic": false })));
        assert!(!explicit_vector_opt_in(&json!({ "semantic": "yes" })));
        assert!(!explicit_vector_opt_in(&json!({ "mode": "bm25" })));
        assert!(!explicit_vector_opt_in(&json!({ "mode": "nonsense" })));
    }

    /// Feature-off build: the build itself settles the vector part of an `index_archive` result. An
    /// EXPLICIT opt-in is an error carrying the rebuild hint; an IMPLICIT request keeps the existing
    /// non-fatal note and stays a success (audit A1).
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn an_onnx_free_build_flags_an_explicit_opt_in_as_an_error_and_a_note_as_success() {
        let (text, is_error) =
            vector_opt_in_report(true).expect("an ONNX-free build must settle the vector part");
        assert!(is_error, "an explicit opt-in is an error, never a note");
        assert_eq!(text, SEMANTIC_OPT_IN_UNSUPPORTED);
        assert!(text.contains("cargo build --features semantic"), "{text}");

        let (note, is_error) =
            vector_opt_in_report(false).expect("an ONNX-free build must settle the vector part");
        assert!(!is_error, "an implicit request stays a successful result");
        assert_eq!(note, VECTOR_UNAVAILABLE_NOTE);
        assert!(note.contains("NOT generated"), "{note}");
        assert!(note.contains("BM25 index above remains usable"), "{note}");
    }

    /// Feature-on build: nothing is pre-judged by the build policy, because the vector pass really
    /// runs and reports its own outcome — including the non-fatal case of an index that already
    /// carries vectors on disk.
    #[cfg(feature = "semantic")]
    #[test]
    fn a_feature_on_build_reports_its_real_vector_pass_instead_of_a_build_policy() {
        assert!(vector_opt_in_report(true).is_none());
        assert!(vector_opt_in_report(false).is_none());
    }

    /// Feature-off build, through `handle_tool_call`: an explicit `"semantic": true` is answered with
    /// `isError: true` and the rebuild hint, never with a success payload.
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn an_explicit_semantic_true_reaches_the_client_as_a_tool_error_with_the_rebuild_hint() {
        let (manager, _dir) = test_manager();
        let (text, is_error) = handle_tool_call(
            &manager,
            "index_archive",
            json!({ "archive": "does_not_exist.zim", "semantic": true }),
            false,
        );

        assert!(
            is_error,
            "an explicit vector opt-in this build cannot honour must be an error: {text}"
        );
        assert!(text.contains(SEMANTIC_OPT_IN_UNSUPPORTED), "{text}");
        assert!(text.contains("cargo build --features semantic"), "{text}");
    }

    /// Feature-off build: the legacy `mode` alias is an explicit opt-in too, so both vector modes get
    /// the same refusal; `bm25` and an omitted `mode` do not.
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn a_legacy_semantic_or_hybrid_mode_is_refused_the_same_way_as_the_semantic_flag() {
        let (manager, _dir) = test_manager();

        for mode in ["semantic", "hybrid"] {
            let (text, is_error) = handle_tool_call(
                &manager,
                "index_archive",
                json!({ "archive": "does_not_exist.zim", "mode": mode }),
                false,
            );
            assert!(is_error, "mode '{mode}' must be refused: {text}");
            assert!(text.contains(SEMANTIC_OPT_IN_UNSUPPORTED), "{text}");
            assert!(text.contains("cargo build --features semantic"), "{text}");
        }

        let (text, is_error) = handle_tool_call(
            &manager,
            "index_archive",
            json!({ "archive": "does_not_exist.zim", "mode": "bm25" }),
            false,
        );
        assert!(is_error, "the missing archive is still the error: {text}");
        assert!(
            !text.contains(SEMANTIC_OPT_IN_UNSUPPORTED),
            "an explicit 'bm25' request is not a vector opt-in: {text}"
        );
    }

    /// Feature-off build: an embedder injected through [`ArchiveManager::set_embedder`] (a test mock
    /// or a library hook) does not make the request honourable. The refusal is about the build, not
    /// about the embedder, so the payload stays an error instead of degrading to a note.
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn an_injected_embedder_does_not_downgrade_the_vector_refusal() {
        use crate::embed::MockEmbedder;

        let (manager, _dir) = test_manager();
        manager.set_embedder(Arc::new(MockEmbedder::new(32)));

        let (text, is_error) = handle_tool_call(
            &manager,
            "index_archive",
            json!({ "archive": "does_not_exist.zim", "semantic": true }),
            false,
        );
        assert!(
            is_error,
            "an injected embedder must not turn the refusal into a success: {text}"
        );
        assert!(text.contains(SEMANTIC_OPT_IN_UNSUPPORTED), "{text}");

        // The policy itself never looks at an embedder, so no injection can switch it back to the
        // non-fatal note path.
        let (note, is_error) = vector_opt_in_report(true).expect("settled by the build");
        assert!(is_error);
        assert_eq!(note, SEMANTIC_OPT_IN_UNSUPPORTED);
    }
}
