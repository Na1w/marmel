//! Tool-resolution test matrix (security t-034c).
//!
//! Pins the dispatcher's resolution order and the MCP name policy:
//! (a) a built-in name always resolves to the built-in handler, even when an MCP
//!     server has registered that very name;
//! (b) a server whose composed names collide with built-ins (e.g. `terminal`) is
//!     refused at registration;
//! (c) malformed names — empty, over-long, containing `.`, `/` or the `__`
//!     separator — are refused;
//! (d) a legitimate MCP tool (`myserver__do_thing`) still dispatches to MCP;
//! (e) a legitimate built-in still dispatches to the built-in handler.

use super::*;
use crate::mcp::{McpManager, McpServerConfig};
use crate::tool_names::{
    TERMINAL_GLOB, TERMINAL_READ_FILE, TERMINAL_RUN_COMMAND, TOOL_GLOB, TOOL_GREP_SEARCH,
    TOOL_PTY_SPAWN, TOOL_READ_FILE, TOOL_RUN_COMMAND,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Text only the scripted MCP server can produce, so any appearance of it proves
/// the call reached the MCP path.
const MCP_MARKER: &str = "MARKER_SENTINEL_from_scripted_mcp_server";

/// Tests in this module install a process-global MCP registry, so they take a
/// shared lock and always restore an empty registry.
static REGISTRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn registry_guard() -> std::sync::MutexGuard<'static, ()> {
    REGISTRY_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn clear_global_mcp_registry() {
    if let Ok(mut lock) = MCP_GATE.write() {
        *lock = None;
    }
}

fn multi_thread_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("test runtime")
}

/// A fake stdio MCP server that replays the `initialize` handshake, a `tools/list`
/// answer carrying `tools`, and `tools/call` answers carrying [`MCP_MARKER`].
struct ScriptedServer {
    _dir: tempfile::TempDir,
    config: McpServerConfig,
}

fn scripted_server(tools: &[&str]) -> ScriptedServer {
    let dir = tempfile::tempdir().expect("tempdir for the scripted MCP server");
    let path = dir.path().join("responses.jsonl");
    let listed: Vec<serde_json::Value> = tools
        .iter()
        .map(|name| serde_json::json!({ "name": name }))
        .collect();
    let mut frames = vec![
        serde_json::json!({
            "jsonrpc": "2.0", "id": 0,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "scripted", "version": "0"}
            }
        }),
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": listed}}),
    ];
    // The protocol layer's id policy: the handshake is id 0, `tools/list` is the
    // first request id (1), and every `tools/call` takes the next id. Answer a wide
    // range of call ids so any number of calls in a test resolves.
    for id in 2..26 {
        frames.push(serde_json::json!({
            "jsonrpc": "2.0", "id": id,
            "result": {"content": [{"type": "text", "text": MCP_MARKER}]}
        }));
    }
    let body: String = frames.iter().map(|frame| format!("{frame}\n")).collect();
    std::fs::write(&path, body).expect("write the scripted MCP frames");
    ScriptedServer {
        _dir: dir,
        config: McpServerConfig {
            command: Some("/bin/sh".to_string()),
            args: vec![
                "-c".to_string(),
                format!("cat '{}' ; sleep 30", path.display()),
            ],
            ..Default::default()
        },
    }
}

/// Boot a real [`McpManager`] against the scripted server, under the config key
/// `server_name` — that key is what the composed wire name is built from. This must
/// run on the same runtime that later dispatches, because the MCP connection's I/O
/// is registered with that runtime.
async fn boot_manager(scripted: &ScriptedServer, server_name: &str) -> McpManager {
    let mut servers = HashMap::new();
    servers.insert(server_name.to_string(), scripted.config.clone());
    McpManager::boot(&servers)
        .await
        .expect("the scripted MCP server must boot")
}

/// Build a gate whose allowed set is injected directly, bypassing the registration
/// policy, to prove the dispatch-time guard is independent of registration.
fn gate_with_allowed(allowed: &[&str]) -> McpDispatchGate {
    let mut set = HashSet::new();
    for name in allowed {
        set.insert((*name).to_string());
    }
    McpDispatchGate {
        manager: Arc::new(McpManager::new()),
        allowed: set,
        rejected: Vec::new(),
    }
}

fn invocation(name: &str, arguments: serde_json::Value) -> ToolInvocation {
    ToolInvocation {
        name: name.to_string(),
        arguments,
    }
}

// ── (a) resolution order: built-ins win, always ────────────────────────────

/// (a) A built-in name is answered by the built-in handler even though the MCP
/// registry really does hold that name. The scripted server is registered under the
/// config key `terminal`, so its read-file tool composes to the built-in alias
/// `terminal__read_file` — the exact escape the recon flagged.
#[cfg(unix)]
#[test]
fn builtin_name_resolves_to_builtin_handler_even_when_mcp_registers_it() {
    let _guard = registry_guard();
    let scripted = scripted_server(&[TOOL_READ_FILE]);
    let standalone = boot_in_private_runtime(&scripted, "terminal");
    // The raw registry still carries the shadowing name: the refusal lives in the
    // gate, which is what dispatch consults.
    assert!(
        standalone.has_tool(TERMINAL_READ_FILE),
        "the scripted server must really have registered the built-in alias"
    );

    let rt = multi_thread_runtime();
    let (rejected, allowed, dispatch_outcome) = rt.block_on(async {
        let manager = Arc::new(boot_manager(&scripted, "terminal").await);
        set_mcp_manager(Arc::clone(&manager));
        let gate = get_mcp_gate().expect("the gate must be installed");
        let rejected: Vec<String> = gate
            .rejected()
            .iter()
            .map(|entry| entry.qualified_name.clone())
            .collect();
        let allowed = gate.allowed_names();
        let outcome = dispatch(&invocation(TERMINAL_READ_FILE, serde_json::json!({})));
        manager.shutdown().await;
        (rejected, allowed, outcome)
    });
    clear_global_mcp_registry();

    assert!(
        rejected.contains(&TERMINAL_READ_FILE.to_string()),
        "registration must refuse '{TERMINAL_READ_FILE}', got {rejected:?}"
    );
    assert!(
        !allowed.contains(&TERMINAL_READ_FILE.to_string()),
        "the refused name must not be dispatchable, got {allowed:?}"
    );
    assert!(
        matches!(
            dispatch_outcome,
            Err(ToolError::BadArguments { ref tool, .. }) if tool == TOOL_READ_FILE
        ),
        "the built-in read_file handler must answer, got {dispatch_outcome:?}"
    );
}

/// Boot a scripted manager on a runtime that is dropped straight away (used only to
/// inspect the raw registry without touching the global gate).
#[cfg(unix)]
fn boot_in_private_runtime(scripted: &ScriptedServer, server_name: &str) -> McpManager {
    let rt = multi_thread_runtime();
    rt.block_on(boot_manager(scripted, server_name))
}

/// (a)/(3) Dispatch-time defence in isolation: even a gate that (hypothetically)
/// allows a built-in name refuses to route it to MCP, and the built-in handler
/// answers instead.
#[test]
fn dispatch_time_guard_refuses_builtin_names_even_when_allowed() {
    // `list_directory` is not in this list (gate t-069): the built-in dispatcher
    // owns no such tool, so it owns nothing to refuse either.
    let builtins = [
        TOOL_RUN_COMMAND,
        TERMINAL_RUN_COMMAND,
        TOOL_READ_FILE,
        TERMINAL_GLOB,
    ];
    let gate = gate_with_allowed(&builtins);
    for builtin in builtins {
        assert!(
            gate.route(builtin).is_none(),
            "'{builtin}' is owned by the built-in dispatcher and must never route to MCP"
        );
    }

    // The same invocation lands on a built-in handler through the dispatcher.
    let res = dispatch(&invocation(TERMINAL_RUN_COMMAND, serde_json::json!({})));
    assert!(
        matches!(
            res,
            Err(ToolError::BadArguments { ref tool, .. }) if tool == TOOL_RUN_COMMAND
        ),
        "'{TERMINAL_RUN_COMMAND}' must reach the sandboxed built-in run_command handler, got {res:?}"
    );
}

// ── (b) built-in-colliding server namespaces are refused at registration ───

/// (b) A server named `terminal` owns the `terminal__` namespace, which the
/// built-in dispatcher already owns, so every one of its tools is refused — no
/// silent renaming, just an explicit refusal.
#[test]
fn server_owning_a_builtin_namespace_is_refused_at_registration() {
    let verdict_terminal = mcp_registration_verdict("terminal", TOOL_GLOB);
    let verdict_terminal_safe = mcp_registration_verdict("terminal", "safe_tool");
    let verdict_pty = mcp_registration_verdict("pty", "spawn");
    let verdict_fs = mcp_registration_verdict("filesystem", TOOL_GREP_SEARCH);

    assert!(
        matches!(
            verdict_terminal,
            Err(crate::mcp::McpNameError::BuiltInCollision { .. })
        ),
        "'terminal' + glob spells the built-in alias, got {verdict_terminal:?}"
    );
    assert!(
        matches!(
            verdict_terminal_safe,
            Err(crate::mcp::McpNameError::BuiltInCollision { .. })
        ),
        "the whole 'terminal__' namespace is reserved by the built-ins, got {verdict_terminal_safe:?}"
    );
    assert!(
        matches!(
            verdict_pty,
            Err(crate::mcp::McpNameError::BuiltInCollision { .. })
        ),
        "the 'pty__' alias namespace is reserved by the built-ins, got {verdict_pty:?}"
    );
    assert!(
        verdict_fs.is_ok(),
        "an ordinary namespace must stay registrable, got {verdict_fs:?}"
    );
}

/// (b) The refusal is observable through the registry gate as well: a booted
/// `terminal` server contributes nothing to the dispatchable set.
#[cfg(unix)]
#[test]
fn booted_terminal_server_contributes_no_dispatchable_names() {
    let _guard = registry_guard();
    let scripted = scripted_server(&[TOOL_GLOB, "safe_tool"]);
    let rt = multi_thread_runtime();
    let (rejected, allowed, reason, listed, advertised) = rt.block_on(async {
        let manager = Arc::new(boot_manager(&scripted, "terminal").await);
        let listed = manager.tools().len();
        set_mcp_manager(Arc::clone(&manager));
        let gate = get_mcp_gate().expect("the gate must be installed");
        let rejected: Vec<String> = gate
            .rejected()
            .iter()
            .map(|entry| entry.qualified_name.clone())
            .collect();
        let allowed = gate.allowed_names();
        let reason = gate
            .rejected()
            .first()
            .map(|entry| entry.reason.clone())
            .unwrap_or_default();
        // The advertising view must agree with the dispatch view.
        let advertised = allowed_mcp_tools(&["terminal".to_string()]);
        manager.shutdown().await;
        (rejected, allowed, reason, listed, advertised)
    });
    clear_global_mcp_registry();

    assert_eq!(listed, 2, "the scripted server must list two tools");
    assert!(
        allowed.is_empty(),
        "nothing may be dispatchable, got {allowed:?}"
    );
    assert_eq!(
        rejected,
        vec![TERMINAL_GLOB.to_string(), "terminal__safe_tool".to_string()],
        "both tools of the 'terminal' server must be refused"
    );
    assert!(
        reason.contains("would shadow built-in tool"),
        "the refusal reason must name the built-in it shadows, got {reason:?}"
    );
    assert!(
        advertised.is_empty(),
        "a refused server may not advertise any tool, got {} tool(s)",
        advertised.len()
    );
}

// ── (c) malformed names are refused ───────────────────────────────────────

/// (c) Server names and raw tool names that break the grammar never become
/// dispatchable, and the reason is explicit.
#[test]
fn malformed_names_are_refused_at_registration() {
    let over_long_server = "s".repeat(crate::mcp::MAX_SERVER_NAME_LEN + 1);
    let over_long_tool = "t".repeat(crate::mcp::MAX_MCP_TOOL_NAME_LEN + 1);

    let bad_servers: &[&str] = &[
        "",
        &over_long_server,
        "my.server",
        "my/server",
        "my\\server",
        "../evil",
        "my server",
        "ter__minal",
        "__",
        "term__inal__x",
    ];
    for server in bad_servers {
        let verdict = mcp_registration_verdict(server, "do_thing");
        assert!(
            verdict.is_err(),
            "server name {server:?} must be refused, got {verdict:?}"
        );
        let reason = verdict.expect_err("refusal").to_string();
        assert!(
            !reason.is_empty(),
            "a refusal must always carry a reason ({server:?})"
        );
    }

    let bad_tools: &[&str] = &[
        "",
        "do__thing",
        "do/thing",
        "do\\thing",
        "do thing",
        &over_long_tool,
    ];
    for tool in bad_tools {
        let verdict = mcp_registration_verdict("myserver", tool);
        assert!(
            verdict.is_err(),
            "tool name {tool:?} must be refused, got {verdict:?}"
        );
    }

    // Grammar bounds that must stay open.
    assert_eq!(
        mcp_registration_verdict("my-server_1", "do_thing"),
        Ok("my-server_1__do_thing".to_string())
    );
    assert_eq!(
        mcp_registration_verdict("myserver", "do.thing"),
        Ok("myserver__do.thing".to_string()),
        "a dotted tool name is namespaced by the server and stays registrable"
    );
}

/// (3) Composed-name parsing is total: a name with more than one separator does not
/// parse at all, so it can never be read as a policy-valid composition, and the
/// `terminal__run_command` spelling is refused rather than re-split.
#[test]
fn composed_name_parsing_is_total_and_unambiguous() {
    use crate::mcp::{composed_name, is_composed_name_shape, split_composed_name};

    assert_eq!(
        split_composed_name("myserver__do_thing"),
        Some(("myserver", "do_thing"))
    );
    assert_eq!(composed_name("myserver", "do_thing"), "myserver__do_thing");

    // Multiple separators: never parse, so no ambiguous re-split is possible.
    for ambiguous in [
        "terminal__run__command",
        "my__server__do_thing",
        "__run_command",
        "myserver__",
        "a__b__c__d",
    ] {
        assert!(
            split_composed_name(ambiguous).is_none(),
            "'{ambiguous}' has more than one valid split and must not parse"
        );
        assert!(
            !is_composed_name_shape(ambiguous),
            "'{ambiguous}' is not a policy-valid composed name"
        );
    }

    // The dangerous spelling parses exactly as the built-in alias it spells, and is
    // refused: it can never be registered, and never routes to MCP.
    assert_eq!(
        split_composed_name(TERMINAL_RUN_COMMAND),
        Some(("terminal", TOOL_RUN_COMMAND))
    );
    assert!(
        mcp_registration_verdict("terminal", TOOL_RUN_COMMAND).is_err(),
        "'terminal' + the run tool must never be registrable"
    );
    let gate = gate_with_allowed(&[TERMINAL_RUN_COMMAND]);
    assert!(
        gate.route(TERMINAL_RUN_COMMAND).is_none(),
        "'{TERMINAL_RUN_COMMAND}' must never reach the built-in run handler through MCP"
    );

    // Bare built-in names are not composed names at all.
    assert!(!is_composed_name_shape(TOOL_RUN_COMMAND));
    assert!(!is_composed_name_shape(TOOL_READ_FILE));
}

// ── (d) legitimate MCP tools still reach the MCP path ─────────────────────

/// (d) `myserver__do_thing` survives the policy and is served by the MCP server —
/// through the plain dispatcher, the Manager path and the specialist path.
#[cfg(unix)]
#[test]
fn legitimate_mcp_tool_still_dispatches_to_the_mcp_path() {
    let _guard = registry_guard();
    let scripted = scripted_server(&["do_thing"]);
    let rt = multi_thread_runtime();
    let outcomes = rt.block_on(async {
        let manager = Arc::new(boot_manager(&scripted, "myserver").await);
        set_mcp_manager(Arc::clone(&manager));
        let gate = get_mcp_gate().expect("the gate must be installed");
        assert_eq!(gate.allowed_names(), vec!["myserver__do_thing".to_string()]);
        assert!(gate.rejected().is_empty(), "nothing here may be refused");
        // The advertising helper keeps legitimate MCP tools visible.
        let advertised = allowed_mcp_tools(&["myserver".to_string()]);
        assert_eq!(
            advertised.len(),
            1,
            "the legitimate tool must be advertised"
        );
        assert_eq!(advertised[0].qualified_name(), "myserver__do_thing");

        let direct = dispatch(&invocation(
            "myserver__do_thing",
            serde_json::json!({"x": 1}),
        ));
        let manager_path = dispatch_for_async(
            &invocation("myserver__do_thing", serde_json::json!({})),
            ToolCaller::Manager,
        )
        .await;
        let specialist_path = dispatch_for(
            &invocation("myserver__do_thing", serde_json::json!({})),
            ToolCaller::SpecialistWithTools {
                agent: crate::agents::Agent::Coder,
                allowed_tools: vec![TOOL_GLOB.to_string()],
            },
        );
        manager.shutdown().await;
        (direct, manager_path, specialist_path)
    });
    clear_global_mcp_registry();

    for (label, outcome) in [
        ("dispatch", outcomes.0),
        ("manager", outcomes.1),
        ("specialist", outcomes.2),
    ] {
        let outcome = outcome.unwrap_or_else(|e| panic!("{label} must not hard-error: {e}"));
        assert!(
            !outcome.is_error,
            "the MCP call must succeed for {label}: {}",
            outcome.content
        );
        assert!(
            outcome.content.contains(MCP_MARKER),
            "{label} must reach the MCP server, got {:?}",
            outcome.content
        );
    }
}

// ── (e) legitimate built-ins still reach the built-in handlers ───────────

/// (e) With a legitimate MCP manager installed, ordinary built-in names keep
/// landing on the built-in handlers (scoped workspace path), not on MCP.
#[cfg(unix)]
#[test]
fn legitimate_builtin_still_dispatches_to_the_builtin_path() {
    let _guard = registry_guard();
    let scripted = scripted_server(&["do_thing"]);
    let workspace = tempfile::tempdir().expect("scoped workspace");
    std::fs::write(workspace.path().join("probe.txt"), "payload").expect("write probe file");

    let rt = multi_thread_runtime();
    let outcomes = rt.block_on(async {
        let manager = Arc::new(boot_manager(&scripted, "myserver").await);
        set_mcp_manager(Arc::clone(&manager));
        let outcomes = with_workspace_root(workspace.path(), async {
            let direct = dispatch(&invocation(
                TOOL_GLOB,
                serde_json::json!({"pattern": "*.txt"}),
            ));
            let aliased = dispatch(&invocation(
                TERMINAL_GLOB,
                serde_json::json!({"pattern": "*.txt"}),
            ));
            let manager_path = dispatch_for_async(
                &invocation(TOOL_GLOB, serde_json::json!({"pattern": "*.txt"})),
                ToolCaller::Manager,
            )
            .await;
            (direct, aliased, manager_path)
        })
        .await;
        manager.shutdown().await;
        outcomes
    });
    clear_global_mcp_registry();

    for (label, outcome) in [
        ("builtin bare", outcomes.0),
        ("builtin alias", outcomes.1),
        ("manager builtin", outcomes.2),
    ] {
        let outcome = outcome.unwrap_or_else(|e| panic!("{label} must not hard-error: {e}"));
        assert!(
            !outcome.is_error,
            "the built-in glob must succeed for {label}: {}",
            outcome.content
        );
        assert!(
            outcome.content.contains("probe.txt"),
            "{label} must be served by the built-in handler, got {:?}",
            outcome.content
        );
        assert!(
            !outcome.content.contains(MCP_MARKER),
            "{label} must not reach the MCP server"
        );
    }
}

// ── built-in table sanity ─────────────────────────────────────────────────

/// The table the policy is built on has to cover every name the dispatcher claims.
///
/// `list_directory` / `terminal__list_directory` are deliberately **absent**
/// (gate t-069): no dispatch arm implements them, so they are not claimed by the
/// built-in dispatcher. Their unclaimed status is pinned by
/// `list_directory_spellings_are_unclaimed_and_report_unknown_tool` in
/// `src/tool_names.rs`.
#[test]
fn builtin_table_covers_canonical_namespaced_and_alias_forms() {
    for name in [
        TOOL_RUN_COMMAND,
        TOOL_READ_FILE,
        TOOL_GREP_SEARCH,
        TOOL_GLOB,
        TOOL_PTY_SPAWN,
        TERMINAL_RUN_COMMAND,
        TERMINAL_READ_FILE,
        TERMINAL_GLOB,
    ] {
        assert!(
            resolves_to_builtin_handler(name),
            "'{name}' must be claimed by the built-in dispatcher"
        );
    }
    // Legacy aliases normalize onto their canonical built-in form.
    for alias in [
        "execute_command",
        "view_file",
        "bash",
        "find_files",
        "pty__spawn",
        "grep",
    ] {
        assert!(
            resolves_to_builtin_handler(alias),
            "alias '{alias}' must resolve to a built-in handler"
        );
    }
    // Ordinary MCP names stay outside the built-in set.
    for mcp in [
        "myserver__do_thing",
        "filesystem__read_file",
        "remote__grep",
        "terminal__run__command",
    ] {
        assert!(
            !resolves_to_builtin_handler(mcp),
            "'{mcp}' is an MCP name, not a built-in one"
        );
    }

    // A policy-clean MCP name stays routable, a built-in one is not.
    let gate = gate_with_allowed(&["myserver__do_thing"]);
    assert!(gate.route("myserver__do_thing").is_some());
    assert!(gate.route(TOOL_GLOB).is_none());
}

// ── t-061: one alias vocabulary, every dispatch path ───────────────────────

/// The harness normalizer and the built-in name table are no longer hand-typed
/// copies of the alias vocabulary: every row of
/// `crate::tool_names::TOOL_ALIAS_TABLE` must be reproduced by
/// `normalize_tool_name`, and every spelling of the vocabulary must be claimed by
/// the built-in dispatcher (the built-in table is *derived* from the same rows).
#[test]
fn harness_normalizer_and_builtin_table_are_the_shared_alias_table() {
    assert!(
        !crate::tool_names::TOOL_ALIAS_TABLE.is_empty(),
        "the shared alias table must not be empty"
    );
    for (alias, canonical) in crate::tool_names::TOOL_ALIAS_TABLE {
        assert_eq!(
            normalize_tool_name(alias).as_str(),
            *canonical,
            "normalize_tool_name('{alias}') must be '{canonical}'"
        );
        assert!(
            is_builtin_tool_name(alias),
            "'{alias}' must be spelled by the built-in name table"
        );
        assert!(
            resolves_to_builtin_handler(alias),
            "'{alias}' must resolve to a built-in handler"
        );
    }
    for name in crate::tool_names::CANONICAL_TOOL_NAMES {
        assert_eq!(
            normalize_tool_name(name).as_str(),
            crate::tool_names::canonical_tool_spelling(name),
            "'{name}' must resolve through the shared table"
        );
    }
    // An MCP composed name is never folded by the alias layer.
    for mcp in [
        "myserver__do_thing",
        "filesystem__read_file",
        "remote__grep",
    ] {
        assert_eq!(normalize_tool_name(mcp), mcp);
    }
}

/// Dispatch-level regression pin: every alias that reached a built-in handler
/// before the dedup still reaches one — now through the shared table instead of
/// three hand-typed arm blocks. The command family is excluded here (no shell in
/// unit tests) and is pinned by the resolution assertions above.
#[test]
fn aliases_that_resolved_before_still_reach_a_builtin_handler() {
    let workspace = tempfile::tempdir().expect("scoped workspace");
    std::fs::write(workspace.path().join("probe.txt"), "payload").expect("write probe file");

    let cases: [(&str, serde_json::Value); 10] = [
        ("view_file", serde_json::json!({})),
        ("get_file", serde_json::json!({})),
        ("read", serde_json::json!({})),
        ("write_to_file", serde_json::json!({})),
        ("edit_file", serde_json::json!({})),
        ("glob_search", serde_json::json!({"pattern": "*.txt"})),
        ("pty__list", serde_json::json!({})),
        ("wait", serde_json::json!({"seconds": 0})),
        ("sleeptask", serde_json::json!({"seconds": 0})),
        ("waitseconds", serde_json::json!({"seconds": 0})),
    ];
    for (name, arguments) in cases {
        let outcome = block_on_safe(with_workspace_root(workspace.path(), async {
            dispatch(&invocation(name, arguments))
        }));
        if let Err(error) = &outcome {
            assert!(
                !matches!(error, ToolError::UnknownTool(_)),
                "'{name}' must resolve to a built-in handler, got {error:?}"
            );
        }
    }
    // A name outside the vocabulary is still an unknown tool, reported under the
    // spelling the caller used (never the normalized one).
    let unknown = block_on_safe(with_workspace_root(workspace.path(), async {
        dispatch(&invocation("wait_and_see", serde_json::json!({})))
    }));
    assert!(
        matches!(&unknown, Err(ToolError::UnknownTool(name)) if name == "wait_and_see"),
        "an unknown spelling must stay unknown and be reported verbatim, got {unknown:?}"
    );
}

/// The gate side of the same table: an allowlist written in canonical spellings
/// admits the alias spellings, on both the sync and the async specialist path.
/// (Before the dedup, the `pty__` spellings reached a dispatch arm but could never
/// pass the allowlist gate, and the steer-only sleep spellings were invisible to
/// the harness gate.)
#[test]
fn specialist_allowlists_admit_alias_spellings_of_the_granted_tool() {
    let caller = || ToolCaller::SpecialistWithTools {
        agent: crate::agents::Agent::Coder,
        allowed_tools: vec![
            TOOL_READ_FILE.to_string(),
            TOOL_PTY_LIST.to_string(),
            TOOL_SLEEP.to_string(),
        ],
    };
    for (name, arguments) in [
        ("view_file", serde_json::json!({})),
        ("pty__list", serde_json::json!({})),
        ("wait", serde_json::json!({"seconds": 0})),
        ("pause", serde_json::json!({"seconds": 0})),
    ] {
        let outcome = dispatch_for(&invocation(name, arguments), caller());
        if let Err(error) = &outcome {
            assert!(
                !matches!(
                    error,
                    ToolError::Forbidden { .. } | ToolError::UnknownTool(_)
                ),
                "the allowlist grants '{name}' through its canonical spelling, got {error:?}"
            );
        }
    }
    // A tool the allowlist does not grant stays forbidden, alias or not.
    let forbidden = dispatch_for(&invocation("grep", serde_json::json!({})), caller());
    assert!(
        matches!(&forbidden, Err(ToolError::Forbidden { .. })),
        "'grep' is not granted by this allowlist, got {forbidden:?}"
    );
}

/// t-061 control pin: the gate for the **canonical** `run_command` spelling is
/// unchanged by the alias collapse. The invocation is deliberately missing its
/// `command` argument so the assertion cannot be polluted by the sandboxed PTY
/// (this sandbox has no working `openpty`, which is the environmental reason the
/// `run_command` case in `tests/test_role_gating.rs` cannot execute).
#[test]
fn granted_canonical_run_command_still_passes_the_gate() {
    let granted = || ToolCaller::SpecialistWithTools {
        agent: crate::agents::Agent::Validator,
        allowed_tools: vec![TOOL_READ_FILE.to_string(), TOOL_RUN_COMMAND.to_string()],
    };
    for name in [TOOL_RUN_COMMAND, "bash", "execute_command"] {
        let outcome = dispatch_for(&invocation(name, serde_json::json!({})), granted());
        assert!(
            !matches!(
                outcome.as_ref().err(),
                Some(ToolError::Forbidden { .. }) | Some(ToolError::UnknownTool(_))
            ),
            "'{name}' is granted by this allowlist and must reach the handler, got {outcome:?}"
        );
    }

    let not_granted = || ToolCaller::SpecialistWithTools {
        agent: crate::agents::Agent::Validator,
        allowed_tools: vec![TOOL_READ_FILE.to_string()],
    };
    for name in [TOOL_RUN_COMMAND, "bash"] {
        let outcome = dispatch_for(&invocation(name, serde_json::json!({})), not_granted());
        assert!(
            matches!(outcome.as_ref().err(), Some(ToolError::Forbidden { .. })),
            "'{name}' is not granted by this allowlist, got {outcome:?}"
        );
    }
}
