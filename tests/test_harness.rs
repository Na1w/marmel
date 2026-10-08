//! Phase C integration tests (harness dispatch, FS, and replace).

use marmennill::agents::Agent;
use marmennill::harness::fs::{
    READ_FILE_MAX_BYTES, READ_FILE_TRUNCATION_MARKER, read_file, replace, write_file_atomic,
};
use marmennill::harness::search::{GLOB_TRUNCATION_MARKER, GREP_TRUNCATION_MARKER};
use marmennill::harness::{ToolCaller, ToolInvocation, dispatch_for};
use marmennill::tool_names::{TOOL_GLOB, TOOL_GREP_SEARCH, TOOL_READ_FILE};
use serde_json::json;

#[test]
fn test_integration_harness_fs_and_dispatch() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let file_path = tmp_dir.path().join("test_integration.txt");
    let path_str = file_path.to_str().unwrap();

    // 1. Write file via harness
    let write_inv = ToolInvocation {
        name: "write_file".to_string(),
        arguments: json!({
            "path": path_str,
            "content": "Line 1: Hello World\nLine 2: Integration Test\nLine 3: Goodbye\n"
        }),
    };
    let res = dispatch_for(&write_inv, ToolCaller::Specialist(Agent::Coder)).unwrap();
    assert!(!res.is_error);

    // 2. Read file with pagination
    let read_inv = ToolInvocation {
        name: "read_file".to_string(),
        arguments: json!({
            "path": path_str,
            "offset": 0,
            "limit": 100
        }),
    };
    let read_res = dispatch_for(&read_inv, ToolCaller::Specialist(Agent::Coder)).unwrap();
    assert!(read_res.content.contains("Hello World"));

    // 3. Replace in file
    let replace_inv = ToolInvocation {
        name: "replace".to_string(),
        arguments: json!({
            "path": path_str,
            "old_str": "Line 2: Integration Test\n",
            "new_str": "Line 2: Replaced Content\n"
        }),
    };
    let replace_res = dispatch_for(&replace_inv, ToolCaller::Specialist(Agent::Coder)).unwrap();
    assert!(!replace_res.is_error);

    // 4. Verify replace took effect
    let verify_res = dispatch_for(&read_inv, ToolCaller::Specialist(Agent::Coder)).unwrap();
    assert!(verify_res.content.contains("Line 2: Replaced Content"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_sleep_execution_and_cancellation() {
    // 1. Normal sleep execution with reason
    let sleep_inv = ToolInvocation {
        name: "sleep".to_string(),
        arguments: json!({
            "seconds": 1,
            "reason": "waiting for build"
        }),
    };
    let start = std::time::Instant::now();
    let res = dispatch_for(&sleep_inv, ToolCaller::Specialist(Agent::Coder)).unwrap();
    let elapsed = start.elapsed();
    assert!(!res.is_error);
    assert!(res.content.contains("Slept for 1 seconds"));
    assert!(res.content.contains("waiting for build"));
    assert!(elapsed >= std::time::Duration::from_millis(900));

    // 2. Sleep parameter aliases (duration / duration_seconds)
    let alias_inv = ToolInvocation {
        name: "sleep".to_string(),
        arguments: json!({
            "duration": 1
        }),
    };
    let alias_res = dispatch_for(&alias_inv, ToolCaller::Manager).unwrap();
    assert!(!alias_res.is_error);
    assert!(alias_res.content.contains("Slept for 1 seconds"));

    // 3. Sleep cancelled mid-flight via cancellation token
    marmennill::orchestrator::reset_cancellation();
    let cancel_inv = ToolInvocation {
        name: "sleep".to_string(),
        arguments: json!({
            "seconds": 10
        }),
    };

    // Trigger cancellation after 100ms on a dedicated async task
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        marmennill::orchestrator::cancel_all();
    });

    let cancel_res = dispatch_for(&cancel_inv, ToolCaller::Specialist(Agent::Debugger)).unwrap();
    assert!(cancel_res.is_error);
    assert!(cancel_res.content.contains("interrupted by cancellation"));

    // Reset cancellation token after test
    marmennill::orchestrator::reset_cancellation();
}

// ---- read_file byte ceiling (recon bug H2) --------------------------------

/// The `.`-prefixed staging leftovers currently inside `dir`.
fn staging_leftovers(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read test directory")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with('.'))
        .collect();
    names.sort();
    names
}

/// (a) An oversized file must return the requested window *plus* an explicit
/// `truncated: true` report — never an opaque failure — and must never panic or
/// split a multi-byte character, whatever byte the cap lands on.
#[test]
fn read_file_oversized_returns_truncated_report_and_never_splits_utf8() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("huge_multibyte.txt");
    // 21 bytes per repetition, one of them inside a 4-byte emoji, so the
    // 262 144-byte cap lands in the middle of a character.
    let payload = "🦀 åäö αβγ".repeat(30_000); // 630 000 bytes > READ_FILE_MAX_BYTES
    assert!(payload.len() > READ_FILE_MAX_BYTES);
    std::fs::write(&path, &payload).expect("write oversized fixture");

    let res = read_file(&json!({
        "path": path.to_string_lossy(),
        "offset": 0,
        "limit": 2000,
    }))
    .expect("oversized read must not error");
    assert!(
        !res.is_error,
        "oversized read must succeed, got: {}",
        res.content
    );

    // Machine-visible truncation report.
    assert!(res.content.contains(READ_FILE_TRUNCATION_MARKER));
    assert!(res.content.contains("truncated: true"));
    assert!(
        res.content
            .contains(&format!("read_cap_bytes: {READ_FILE_MAX_BYTES}"))
    );
    assert!(
        res.content
            .contains(&format!("bytes_read: {READ_FILE_MAX_BYTES}"))
    );
    assert!(res.content.contains("re-run with offset="));

    // The window itself is intact and bounded; a split character would have
    // surfaced as the Unicode replacement character.
    let head: String = payload.chars().take(2000).collect();
    assert!(res.content.starts_with(&head));
    assert!(
        !res.content.contains('\u{FFFD}'),
        "the byte cap must never split a multi-byte character"
    );
    assert!(
        res.content.chars().count() < 2_600,
        "the report must stay compact: {}",
        res.content
    );

    // A second page inside the loaded head still works.
    let page2 = read_file(&json!({
        "path": path.to_string_lossy(),
        "offset": 2000,
        "limit": 2000,
    }))
    .expect("second page must not error");
    assert!(!page2.is_error);
    let expected: String = payload.chars().skip(2000).take(2000).collect();
    assert!(page2.content.starts_with(&expected));
    assert!(page2.content.contains(READ_FILE_TRUNCATION_MARKER));

    // An offset far past the loaded head stays bounded and explicit — no panic.
    let far = read_file(&json!({
        "path": path.to_string_lossy(),
        "offset": 10_000_000_u64,
        "limit": 2000,
    }))
    .expect("deep offset must not error");
    assert!(!far.is_error);
    assert!(
        far.content
            .contains("requested_offset_beyond_loaded_head: true")
    );
    assert!(far.content.contains("truncated: true"));
    assert!(staging_leftovers(tmp.path()).is_empty());
}

/// (b) Under the cap the success path stays byte-identical: same bytes, no
/// truncation report, no footer.
#[test]
fn read_file_under_cap_returns_bytes_verbatim() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("small_multibyte.txt");
    let payload = "héllo wörld 🦀\n".repeat(100); // 1 400 chars / 1 900 bytes
    std::fs::write(&path, &payload).expect("write fixture");

    let res = read_file(&json!({ "path": path.to_string_lossy() })).expect("read must succeed");
    assert!(!res.is_error);
    assert_eq!(
        res.content.as_bytes(),
        payload.as_bytes(),
        "an under-cap read must be byte-identical to the file"
    );
    assert!(!res.content.contains(READ_FILE_TRUNCATION_MARKER));
    assert!(!res.content.contains("[Showing characters"));
}

/// The cap boundary is exact: at `READ_FILE_MAX_BYTES` the classic pagination
/// footer is still produced, one byte more switches to the truncation report.
#[test]
fn read_file_cap_boundary_exact_and_one_byte_over() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let exact = tmp.path().join("exact_cap.txt");
    std::fs::write(&exact, "a".repeat(READ_FILE_MAX_BYTES)).expect("write exact-cap fixture");
    let res = read_file(&json!({
        "path": exact.to_string_lossy(),
        "offset": 0,
        "limit": 8000,
    }))
    .expect("read must succeed");
    assert!(!res.is_error);
    assert!(
        !res.content.contains(READ_FILE_TRUNCATION_MARKER),
        "a file exactly at the cap must not be reported as truncated"
    );
    assert!(
        res.content.contains(&format!(
            "[Showing characters 0-8000 of {READ_FILE_MAX_BYTES}. Use offset=8000 to read next chunk]"
        )),
        "the exact-cap file must keep the classic pagination footer"
    );

    let over = tmp.path().join("over_cap.txt");
    std::fs::write(&over, "a".repeat(READ_FILE_MAX_BYTES + 1)).expect("write over-cap fixture");
    let over_res = read_file(&json!({
        "path": over.to_string_lossy(),
        "offset": 0,
        "limit": 2000,
    }))
    .expect("read must succeed");
    assert!(over_res.content.contains(READ_FILE_TRUNCATION_MARKER));
    assert!(
        over_res
            .content
            .contains(&format!("file_bytes: {}", READ_FILE_MAX_BYTES + 1))
    );
    assert!(staging_leftovers(tmp.path()).is_empty());
}

/// The ceiling and its report survive the real dispatch/role plumbing.
#[test]
fn read_file_truncation_report_survives_dispatch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("dispatch_big.txt");
    std::fs::write(&path, "x".repeat(READ_FILE_MAX_BYTES + 4096)).expect("write fixture");

    let inv = ToolInvocation {
        name: TOOL_READ_FILE.to_string(),
        arguments: json!({"path": path.to_string_lossy(), "offset": 0, "limit": 2000}),
    };
    let res = dispatch_for(&inv, ToolCaller::Specialist(Agent::Coder)).expect("dispatch must work");
    assert!(!res.is_error);
    assert!(res.content.contains(READ_FILE_TRUNCATION_MARKER));
    assert!(res.content.contains("truncated: true"));
}

// ---- collision-proof staging + atomic rename (recon bug H4) ---------------

/// (c) Eight `replace` calls racing on the SAME target: none may error, none may
/// collide on a staging name, no edit may be lost, and no `.`-prefixed staging
/// file may survive.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_replace_on_same_target_keeps_every_edit() {
    const SLOTS: usize = 8;
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("shared_target.txt");
    let base: String = (0..SLOTS).map(|i| format!("slot{i}: original\n")).collect();
    std::fs::write(&path, &base).expect("write fixture");

    let path_str = path.to_string_lossy().to_string();
    let mut handles = Vec::new();
    for slot in 0..SLOTS {
        let path_str = path_str.clone();
        handles.push(tokio::spawn(async move {
            let join = tokio::task::spawn_blocking(move || {
                replace(&json!({
                    "path": path_str,
                    "old_str": format!("slot{slot}: original"),
                    "new_str": format!("slot{slot}: edited"),
                }))
            })
            .await;
            match join {
                Ok(result) => result,
                Err(err) => panic!("blocking replace task failed to run: {err}"),
            }
        }));
    }
    for handle in handles {
        let outcome = handle.await.expect("task joined");
        let res = match outcome {
            Ok(res) => res,
            Err(err) => panic!("concurrent replace raised a tool error: {err}"),
        };
        assert!(
            !res.is_error,
            "concurrent replace must not error: {}",
            res.content
        );
    }

    let final_content = std::fs::read_to_string(&path).expect("read final content");
    for slot in 0..SLOTS {
        assert!(
            final_content.contains(&format!("slot{slot}: edited")),
            "edit {slot} was lost:\n{final_content}"
        );
    }
    assert_eq!(
        final_content,
        base.replace("original", "edited"),
        "every concurrent edit must be present, in order"
    );
    let leftovers = staging_leftovers(tmp.path());
    assert!(
        leftovers.is_empty(),
        "concurrent replace() leaked staging files: {leftovers:?}"
    );
}

/// (c2) Collision-proof staging names under a raw race (no per-path lock in
/// `write_file_atomic`): 24 threads staging into the same directory at once.
/// With the old `.{name}.tmp.{pid}` shape they all picked one name and
/// clobbered/renamed each other's staged content.
#[test]
fn concurrent_atomic_writes_never_collide_on_staging_names() {
    const WRITERS: usize = 24;
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("contended_target.txt");
    std::fs::write(&path, "seed").expect("write seed");

    let mut handles = Vec::new();
    for writer in 0..WRITERS {
        let path = path.clone();
        handles.push(std::thread::spawn(move || {
            write_file_atomic(&path, format!("writer-{writer}").as_bytes())
        }));
    }
    for handle in handles {
        handle
            .join()
            .expect("writer thread must not panic")
            .expect("concurrent atomic write must succeed");
    }

    let final_content = std::fs::read_to_string(&path).expect("read final content");
    assert!(
        (0..WRITERS).any(|writer| final_content == format!("writer-{writer}")),
        "the file must hold exactly one complete payload, got: {final_content}"
    );
    let leftovers = staging_leftovers(tmp.path());
    assert!(
        leftovers.is_empty(),
        "concurrent staging leaked files: {leftovers:?}"
    );
}

/// A staging name that already exists is never clobbered: the exclusive create
/// picks a fresh name instead, and the decoy file keeps its content.
#[test]
fn replace_does_not_clobber_preexisting_staging_name() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("clobber_target.txt");
    std::fs::write(&path, "alpha one\n").expect("write fixture");
    // The pre-fix deterministic temp name, planted as a decoy.
    let decoy = tmp
        .path()
        .join(format!(".clobber_target.txt.tmp.{}", std::process::id()));
    std::fs::write(&decoy, "DO NOT TOUCH").expect("write decoy");

    let res = replace(&json!({
        "path": path.to_string_lossy(),
        "old_str": "one",
        "new_str": "two",
    }))
    .expect("replace must succeed");
    assert!(!res.is_error, "{}", res.content);

    assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha two\n");
    assert_eq!(
        std::fs::read_to_string(&decoy).unwrap(),
        "DO NOT TOUCH",
        "staging must never overwrite an existing file"
    );
    assert_eq!(
        staging_leftovers(tmp.path()),
        vec![decoy.file_name().unwrap().to_string_lossy().into_owned()],
        "the decoy must be the only dot-prefixed entry left"
    );
}

/// (d) A forced `rename` failure must remove the staged file: renaming a regular
/// file onto a *non-empty directory* always fails, which exercises the failure
/// path *after* staging succeeded.
#[test]
fn atomic_write_rename_failure_leaves_no_staging_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("directory_target");
    std::fs::create_dir(&target).expect("create target dir");
    std::fs::write(target.join("inner.txt"), "keep").expect("write inner");

    let err = write_file_atomic(&target, b"payload").expect_err("rename onto a dir must fail");
    let detail = err.to_string();
    assert!(
        detail.contains("renaming staging file"),
        "expected a rename failure, got: {detail}"
    );

    assert_eq!(
        std::fs::read_to_string(target.join("inner.txt")).expect("inner must survive"),
        "keep",
        "a failed atomic write must not touch the target"
    );
    let leftovers = staging_leftovers(tmp.path());
    assert!(
        leftovers.is_empty(),
        "a failed rename must not orphan a staging file: {leftovers:?}"
    );
}

/// A failure *before* staging (missing parent directory) is reported explicitly
/// and leaves nothing behind either.
#[test]
fn atomic_write_staging_create_failure_is_explicit_and_leaves_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("no_such_dir/target.txt");

    let err =
        write_file_atomic(&target, b"payload").expect_err("staging in a missing dir must fail");
    let detail = err.to_string();
    assert!(
        detail.contains("creating staging file"),
        "expected a staging-create failure, got: {detail}"
    );
    assert!(staging_leftovers(tmp.path()).is_empty());
    assert!(!target.exists());
}

// ---- search truncation contract + brace globs (recon bugs H8 and H10) ----

/// A capped `grep_search` answer carries the machine-visible truncation report
/// through the real dispatch/role plumbing; a complete answer carries neither
/// the marker nor the flag.
#[test]
fn grep_search_truncation_report_survives_dispatch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    for n in 0..4 {
        let body: String = (0..3).map(|m| format!("needle {n}-{m}\n")).collect();
        std::fs::write(tmp.path().join(format!("f{n}.txt")), body).expect("write fixture");
    }

    let clipped = dispatch_for(
        &ToolInvocation {
            name: TOOL_GREP_SEARCH.to_string(),
            arguments: json!({
                "pattern": "needle",
                "path": tmp.path().to_string_lossy(),
                "max_results": 5,
            }),
        },
        ToolCaller::Specialist(Agent::Researcher),
    )
    .expect("dispatch must work");
    assert!(!clipped.is_error, "{}", clipped.content);
    assert!(
        clipped.content.contains(GREP_TRUNCATION_MARKER),
        "marker must survive dispatch: {}",
        clipped.content
    );
    assert!(clipped.content.contains("truncated: true"));
    assert!(clipped.content.contains("total_matches: 12"));
    assert!(clipped.content.contains("returned_matches: 5"));
    assert!(clipped.content.contains("hidden_matches: 7"));

    let whole = dispatch_for(
        &ToolInvocation {
            name: TOOL_GREP_SEARCH.to_string(),
            arguments: json!({"pattern": "needle", "path": tmp.path().to_string_lossy()}),
        },
        ToolCaller::Specialist(Agent::Researcher),
    )
    .expect("dispatch must work");
    assert!(
        !whole.content.contains("truncated"),
        "a complete answer must not claim truncation: {}",
        whole.content
    );
    assert_eq!(whole.content.lines().count(), 12, "every match is shown");
}

/// Brace alternation works end to end (`src/**/*.{rs,md}` matches both
/// extensions), a capped `glob` reports it, and a malformed brace group is an
/// explicit error rather than a confident empty answer.
#[tokio::test(flavor = "multi_thread")]
async fn glob_brace_pattern_and_truncation_survive_dispatch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("src")).expect("mkdir");
    for name in ["src/a.rs", "src/a.md", "src/a.toml", "b.rs"] {
        std::fs::write(tmp.path().join(name), "x\n").expect("write fixture");
    }

    let researcher = ToolCaller::Specialist(Agent::Researcher);
    let (braced, capped, broken) = marmennill::harness::with_workspace_root(tmp.path(), async {
        let braced = dispatch_for(
            &ToolInvocation {
                name: TOOL_GLOB.to_string(),
                arguments: json!({"pattern": "src/**/*.{rs,md}"}),
            },
            researcher.clone(),
        );
        let capped = dispatch_for(
            &ToolInvocation {
                name: TOOL_GLOB.to_string(),
                arguments: json!({"pattern": "**/*.{rs,md,toml}", "max_results": 2}),
            },
            researcher.clone(),
        );
        let broken = dispatch_for(
            &ToolInvocation {
                name: TOOL_GLOB.to_string(),
                arguments: json!({"pattern": "src/**/*.{rs,md"}),
            },
            researcher,
        );
        (braced, capped, broken)
    })
    .await;

    let braced = braced.expect("a brace pattern must be honoured, not answered as no matches");
    assert_eq!(braced.content, "src/a.md\nsrc/a.rs");

    let capped = capped.expect("capped glob must succeed");
    assert!(capped.content.contains(GLOB_TRUNCATION_MARKER));
    assert!(capped.content.contains("truncated: true"));
    assert!(capped.content.contains("total_matches: 4"));
    assert!(capped.content.contains("hidden_matches: 2"));
    assert!(capped.content.contains("pattern_alternatives: 3"));

    let broken = broken.expect_err("an unbalanced brace group must be an explicit error");
    assert!(
        broken.to_string().contains("brace"),
        "the error must name the brace defect, got: {broken}"
    );
}

// ---------------------------------------------------------------------------
// Gate t-046 — task ids may never escape the directory they are joined onto.
//
// Every on-disk path derived from a task id (synthesized prompt files, per-task
// `-validation.md` verdict files) must route through the crate's single
// validator in `src/task_id.rs`. These tests run against a temp workspace via
// `harness::with_workspace_root` and never touch the repository's real
// `.marmel/`.
// ---------------------------------------------------------------------------

/// SSE body carrying one tool call, shaped like the OpenAI stream the client
/// parses (local helper for the t-046 end-to-end test).
fn t046_tool_call_sse(call_id: &str, tool_name: &str, args_json: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-t046",
            "choices": [{
                "delta": {
                    "content": null,
                    "tool_calls": [{
                        "index": 0,
                        "id": call_id,
                        "type": "function",
                        "function": { "name": tool_name, "arguments": args_json }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })
    )
}

/// Plain-text SSE body (local helper for the t-046 end-to-end test).
fn t046_text_sse(text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-t046",
            "choices": [{ "delta": { "content": text }, "finish_reason": "stop" }]
        })
    )
}

/// Public path builder: an accepted task id resolves to one file name *inside*
/// the prompts directory, and a hostile id is refused with a typed reason
/// instead of becoming a path. Nothing is created for a refused id, and the
/// escape target a raw join would have reached stays absent.
#[test]
fn task_id_prompt_paths_are_gated_to_the_prompts_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("gated_ws");
    let ws = marmennill::harness::workspace::Workspace::at(&root);
    ws.ensure_writable().expect("workspace must be writable");

    for (raw, file_name) in [
        ("t-046", "t-046.md"),
        ("  [t-046]  ", "t-046.md"),
        ("task-t-046", "task-t-046.md"),
        ("t_val_046", "t_val_046.md"),
    ] {
        let path = ws
            .prompt_path_for_task(raw)
            .unwrap_or_else(|err| panic!("{raw:?} must be accepted: {err}"));
        assert_eq!(path, ws.prompts_dir().join(file_name), "for {raw:?}");
        assert!(
            path.starts_with(ws.prompts_dir()),
            "path for {raw:?} must stay under the prompts dir: {}",
            path.display()
        );
    }

    for hostile in [
        "../../etc/x",
        "../escape",
        "a/b",
        "..\\..\\escape",
        "..",
        ".",
        ".hidden",
        "t-046/extra",
        "t 046",
        "",
        "   ",
    ] {
        let err = ws
            .prompt_path_for_task(hostile)
            .err()
            .unwrap_or_else(|| panic!("{hostile:?} must be refused, never resolved to a path"));
        assert!(
            !err.to_string().is_empty(),
            "the rejection of {hostile:?} must carry a reason"
        );
    }

    // Control: a raw join of the same id does escape (a `..` component), and the
    // resolved escape target was never created.
    let would_be = ws.prompts_dir().join("../../escape.md");
    assert!(
        would_be
            .components()
            .any(|c| c == std::path::Component::ParentDir),
        "control: an ungated join must carry a '..' component: {would_be:?}"
    );
    assert!(
        !tmp.path().join("escape.md").exists(),
        "the escape target must not exist"
    );

    let created: Vec<String> = std::fs::read_dir(&root)
        .expect("read workspace root")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        created.is_empty(),
        "a rejected task id must create nothing at all: {created:?}"
    );
}

/// End-to-end proof through the live specialist loop: with a per-task prompts
/// directory present and a verdict file planted one directory **above** it, a
/// hostile task id must not be able to read that file as its validation brief.
/// The run has to fail closed (hard verdict gap), the plan marker must be
/// revoked, and the workspace must hold no new files.
#[tokio::test]
async fn hostile_task_id_fails_closed_instead_of_reading_a_verdict_outside_prompts() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let marmel = root.join(marmennill::manager::phase::MARMEL_DIR);
    let prompts = marmel.join("prompts");
    std::fs::create_dir_all(&prompts).expect("prompts dir");
    // Reachable only through `..` in the task id: `prompts/../escape-validation.md`.
    let planted = "PLANTED-VERDICT-BRIEF must never be read as a validator brief";
    std::fs::write(marmel.join("escape-validation.md"), planted).expect("plant verdict file");

    let server = wiremock::MockServer::start().await;
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with({
            let calls = calls.clone();
            move |_req: &wiremock::Request| {
                let idx = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body = match idx {
                    // Turn 0: the specialist does real work.
                    0 => t046_tool_call_sse(
                        "call_write_t046",
                        "write_file",
                        &serde_json::json!({"path": "notes.md", "content": "hostile id run\n"})
                            .to_string(),
                    ),
                    // Turn 1: specialist concludes; the verdict gate then runs.
                    1 => t046_text_sse("Work finished.\n\nMISSION COMPLETE (t-046)"),
                    // Only an escaped verdict read would let validation start.
                    _ => t046_text_sse("UNEXPECTED-VALIDATOR-CALL"),
                };
                wiremock::ResponseTemplate::new(200).set_body_string(body)
            }
        })
        .mount(&server)
        .await;

    let backend_url = format!("{}/v1", server.uri());
    let cfg = marmennill::config::Config {
        backend_url: backend_url.clone(),
        model: "test-model".to_string(),
        ..Default::default()
    };
    let ctx = marmennill::agents::IsolatedContext {
        role_system_prompt: "You are the Coder specialist.".to_string(),
        brief: "Write notes.md.".to_string(),
        snippets: vec![],
        task_id: Some("../escape".to_string()),
        image_urls: vec![],
        audio_urls: vec![],
        blueprint: None,
    };

    let deliverable = marmennill::harness::with_workspace_root(root.clone(), async {
        let client = marmennill::llm::ChatClient::new(&backend_url, "test-model");
        let token = tokio_util::sync::CancellationToken::new();
        marmennill::agents::run_specialist_live(
            &client,
            marmennill::agents::Agent::Coder,
            &ctx,
            &cfg,
            &token,
        )
        .await
        .expect("the run must finish and report its verdict, not abort")
    })
    .await;

    // (a) The failure is surfaced, not swallowed: a hard verdict gap naming the
    // rejected id, a failed trailer, and no completion marker left to check a
    // plan line off.
    assert!(
        deliverable.contains("Validation was not performed"),
        "the deliverable must carry the verdict gap: {deliverable}"
    );
    assert!(
        deliverable.contains("rejected as a single path segment"),
        "the verdict gap must name the rejected task id: {deliverable}"
    );
    assert!(
        deliverable.contains(marmennill::markers::MARKER_FAILED),
        "a rejected run must surface a failure marker: {deliverable}"
    );
    assert!(
        !deliverable.contains(marmennill::markers::MARKER_COMPLETE),
        "the completion marker must be revoked: {deliverable}"
    );
    assert!(
        !matches!(
            marmennill::agents::MissionMarker::parse(&deliverable),
            Some(marmennill::agents::MissionMarker::Complete { .. })
        ),
        "a rejected task id must never parse as a completed task: {deliverable}"
    );

    // (b) The planted verdict outside the prompts directory was never read as a
    // brief: the validator was never contacted, so exactly two LLM calls happen.
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "validation must not start for a rejected task id: {deliverable}"
    );
    assert!(
        !deliverable.contains("PLANTED-VERDICT-BRIEF")
            && !deliverable.contains("UNEXPECTED-VALIDATOR-CALL"),
        "the escaped verdict file must never reach the deliverable: {deliverable}"
    );

    // (c) Nothing escaped into the workspace: the specialist's own file plus the
    // untouched planted fixture are the only files, and the prompts directory
    // stays empty.
    assert!(
        root.join("notes.md").is_file(),
        "the specialist work itself must still have happened"
    );
    let mut marmel_entries: Vec<String> = std::fs::read_dir(&marmel)
        .expect("read .marmel")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    marmel_entries.sort();
    assert_eq!(
        marmel_entries,
        vec!["escape-validation.md".to_string(), "prompts".to_string()],
        "no file may appear in the workspace's .marmel directory"
    );
    let prompt_entries: Vec<String> = std::fs::read_dir(&prompts)
        .expect("read prompts dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        prompt_entries.is_empty(),
        "the prompts directory must stay empty: {prompt_entries:?}"
    );
    assert_eq!(
        std::fs::read_to_string(marmel.join("escape-validation.md")).expect("re-read planted file"),
        planted,
        "the file outside the prompts directory must be untouched"
    );

    // (d) Nothing landed next to the workspace either (the escape path resolves
    // into the parent of the temp root).
    let parent = tmp.path().parent().expect("temp parent");
    let stray: Vec<String> = std::fs::read_dir(parent)
        .expect("read temp parent")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("escape"))
        .collect();
    assert!(
        stray.is_empty(),
        "no stray escape/validation file may exist outside the workspace: {stray:?}"
    );
}
