//! The in-memory Manager capture holder (t-009).
//!
//! Design A §1 **D2** deleted the second journal: `ContextEngine::save_transcript`
//! used to write a whole second transcript to `.marmel/.session_transcript.json`,
//! alongside the UI transcript. Two independent journals of overlapping state is
//! exactly the failure D2 removes, and `session/` is the only owner of durable
//! session state (D1). So the legacy save sites in `src/ui/session.rs` no longer
//! write a file: each one calls [`capture_into_session_state`], which snapshots the
//! live engine into the **durable vocabulary** ([`ManagerSection`]) and keeps that
//! snapshot as the session's in-memory manager state.
//!
//! That is the point of the holder: the manager transcript survives the deletion of
//! its journal without losing what the journal was for. Every call site keeps its
//! event semantics — it marks a boundary where the manager state is worth
//! remembering — and only its sink changes. The capture-trigger surface and the
//! bundle commit (flow §4.3, t-011/t-012/t-013) replace this holder with
//! [`session::commit::write_bundle`] calls; until then, a committer reads the
//! newest snapshot through [`last_capture`].
//!
//! Capture is a **read** (flow R-TRIG-0): [`capture_into_session_state`] takes
//! `&ContextEngine`, and [`ContextEngine::capture_manager_section`] never mutates,
//! repairs, prunes, re-roles or re-orders the transcript. The pairing grammar is
//! not re-implemented here — the `dangling` witness is
//! [`DanglingToolCallState::observe`], the same observer the loader cross-checks
//! (A D1, §8.2 N6).
//!
//! Like [`session::commit::LAST_COMMIT`], the holder is process-global by design:
//! there is one live manager transcript per process, and it holds the **newest**
//! snapshot only — nothing is appended, so it cannot grow without bound.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};

use crate::manager::ContextEngine;
use crate::session::format::ManagerSection;

/// Name of the **superseded** manager journal (A §1 D2).
///
/// `ContextEngine::save_transcript` and `Plan::transcript_path()` are gone: this
/// binary neither writes nor reads that file. The name survives only so the two
/// places that must still recognize a stale copy can name it honestly —
/// `session::restore`'s "legacy journals found, not restorable" warning (A §7.4:
/// ignored must not mean invisible) and `Plan::clear`'s file sweep for a leftover
/// from an older release. It lives in `session/` because D1 makes this module the
/// only place in the crate that names a session file.
pub const LEGACY_TRANSCRIPT_FILE: &str = ".session_transcript.json";

/// The newest Manager capture of this process.
///
/// Poisoned-lock handling follows [`session::commit::LAST_COMMIT`]: the state is a
/// plain snapshot cache, so `into_inner` recovery is safe — a stale snapshot costs
/// one extra capture, never a wrong bundle.
pub static LAST_MANAGER_CAPTURE: LazyLock<Mutex<Option<ManagerSection>>> =
    LazyLock::new(|| Mutex::new(None));

/// How many manager captures this process has recorded. Monotonic; the cheap
/// witness a caller can use to tell "capture ran" from "capture is stale".
static CAPTURE_COUNT: AtomicU64 = AtomicU64::new(0);

fn capture_lock() -> MutexGuard<'static, Option<ManagerSection>> {
    LAST_MANAGER_CAPTURE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Snapshot `engine` into the durable vocabulary and make it the session's
/// in-memory manager state.
///
/// The one call the legacy `save_transcript` sites use. It cannot fail: capture is
/// a read of a live engine, and no I/O is involved.
pub fn capture_into_session_state(engine: &ContextEngine) {
    record_capture(engine.capture_manager_section());
}

/// Record an already-captured section — the entry point for a trigger that
/// captured the section itself (e.g. to hand it to a bundle commit). Returns the
/// running capture count.
pub fn record_capture(section: ManagerSection) -> u64 {
    *capture_lock() = Some(section);
    CAPTURE_COUNT.fetch_add(1, Ordering::SeqCst) + 1
}

/// The newest captured manager section, or `None` if nothing has been captured.
pub fn last_capture() -> Option<ManagerSection> {
    capture_lock().clone()
}

/// The newest captured manager section, leaving the holder empty.
pub fn take_capture() -> Option<ManagerSection> {
    capture_lock().take()
}

/// How many manager captures this process has recorded.
pub fn capture_count() -> u64 {
    CAPTURE_COUNT.load(Ordering::SeqCst)
}

/// Drop the held snapshot: a `/reset`, whose ending says the session MUST NOT be
/// resurrected (A §3.4 `ProcessEnding::Reset`).
pub fn clear_capture() {
    *capture_lock() = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::ContextEngine;
    use crate::manager::context::{
        INJECTED_ADVISORY_PREFIXES, REBIRTH_CHECKPOINT_PREFIX, is_injected_advisory,
    };
    use crate::session::commit::{CapturePayload, CommitLock, CommitPolicy};
    use crate::session::format::{
        ABORTED_TOOL_RESULT, DanglingCall, DanglingToolCallState, IdentityFloors, MessageRecord,
        PinStage, ProcessEnding, RestoreOutcome, SESSION_RESTORE_NOTICE_PREFIX, witness_of,
    };
    use crate::session::restore::{load_bundle, verify_pins};
    use crate::tool_names::{TOOL_READ_FILE, TOOL_RUN_COMMAND};
    use crate::types::{Message, ToolCall, ToolFunction};

    /// Serializes the holder tests: the holder is process-global and the lib test
    /// binary runs tests on parallel threads.
    static HOLDER_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// The four markers that existed before t-009, spelled out again **on
    /// purpose**: requirement (d) is that their text is untouched, so the
    /// expectation must not be derived from the constant it checks.
    const PRE_EXISTING_ADVISORY_PREFIXES: &[&str] = &[
        "(SYSTEM: CONTEXT BUDGET ADVISORY",
        "(SYSTEM: CONTEXT LIMIT EXCEEDED",
        "(SYSTEM: Rebirth checkpoint accepted",
        "(SYSTEM: REBIRTH CHECKPOINT. The previous turn-by-turn history has been compacted. \
Summarized progress: ",
    ];

    /// A transcript carrying every shape the wire type collapses or rewrites: an
    /// assistant with `content: None`, an assistant with `content: Some("")`,
    /// `reasoning_content`, verbatim (malformed, unsorted) tool arguments, an
    /// `"(aborted)"` tool result, a rebirth checkpoint in the **historic `System`
    /// spelling**, and a legacy advisory this binary no longer emits.
    fn live_engine() -> ContextEngine {
        let mut ctx = ContextEngine::new(8_000);
        ctx.set_system_prompt("SYSTEM PROMPT — pinned bytes (REQ-CORE-001)".to_string());
        ctx.set_goal("goal: pinned bytes, verbatim (REQ-CORE-002)".to_string());
        ctx.append(Message::System {
            content: format!("{REBIRTH_CHECKPOINT_PREFIX}checkpoint written by an older build)"),
        });
        ctx.append(Message::User {
            content: "(SYSTEM: CONTEXT LIMIT EXCEEDED legacy notice, treated as opaque bytes"
                .to_string(),
        });
        ctx.append(Message::User {
            content: "keep going, then report".to_string(),
        });
        ctx.append(Message::Assistant {
            content: None,
            reasoning_content: Some("  thinking, spaces included  ".to_string()),
            tool_calls: vec![ToolCall {
                id: "call-1".to_string(),
                kind: "function".to_string(),
                function: ToolFunction {
                    name: TOOL_RUN_COMMAND.to_string(),
                    // Not valid JSON, deliberately unsorted: the capture must carry
                    // these bytes verbatim.
                    arguments: "{ \"z\": 1, \"a\": 2, broken".to_string(),
                },
            }],
        });
        ctx.append(Message::Tool {
            tool_call_id: "call-1".to_string(),
            content: ABORTED_TOOL_RESULT.to_string(),
        });
        ctx.append(Message::Assistant {
            content: Some(String::new()),
            reasoning_content: Some(String::new()),
            tool_calls: Vec::new(),
        });
        ctx
    }

    // ------------------------------------------------- (a) the vector survives

    /// Capture → restore round-trips the manager vector **element by element**.
    #[test]
    fn capture_and_restore_round_trip_the_vector_element_by_element() {
        let engine = live_engine();
        let section = engine.capture_manager_section();
        assert_eq!(
            section.messages.len(),
            engine.messages().len(),
            "capture must carry exactly one record per live message"
        );

        let mut restored = ContextEngine::new(1);
        restored.install_restored_messages(section.messages.clone());

        assert_eq!(restored.messages().len(), section.messages.len());
        for (index, record) in section.messages.iter().enumerate() {
            let installed = MessageRecord::from_wire(&restored.messages()[index]);
            assert_eq!(
                &installed, record,
                "message {index} is not byte-identical after restore: captured {record:?}, \
                 installed {installed:?}"
            );
        }
        // Nothing was re-role'd or re-ordered — the role sequence is the fixture's.
        let roles: Vec<&str> = restored.messages().iter().map(Message::role).collect();
        assert_eq!(
            roles,
            vec![
                "system",
                "user",
                "system",
                "user",
                "user",
                "assistant",
                "tool",
                "assistant"
            ]
        );
    }

    /// `Assistant.content == None` stays distinct from `Some("")` — in the durable
    /// value and in the durable bytes. The wire serializer
    /// (`src/types/wire.rs:30-41`, `serialize_assistant_content`) collapses the two,
    /// which is exactly why `MessageRecord` exists.
    #[test]
    fn null_and_empty_assistant_content_survive_as_two_different_facts() {
        let engine = live_engine();
        let section = engine.capture_manager_section();
        let json = serde_json::to_string(&section.messages).expect("records serialize");
        assert!(
            json.contains(r#""content":null"#),
            "a content-less assistant turn must serialize as `\"content\": null`, got {json}"
        );

        let mut restored = ContextEngine::new(1);
        restored.install_restored_messages(section.messages.clone());

        let with_calls = restored
            .messages()
            .iter()
            .find(|m| matches!(m, Message::Assistant { tool_calls, .. } if !tool_calls.is_empty()))
            .expect("the assistant turn carrying the tool call");
        match with_calls {
            Message::Assistant {
                content,
                reasoning_content,
                tool_calls,
            } => {
                assert!(
                    content.is_none(),
                    "`content: None` must come back as `None`, got {content:?}"
                );
                assert_eq!(
                    reasoning_content.as_deref(),
                    Some("  thinking, spaces included  "),
                    "`reasoning_content` must survive verbatim"
                );
                assert_eq!(
                    tool_calls[0].function.arguments, "{ \"z\": 1, \"a\": 2, broken",
                    "`ToolCall.function.arguments` must be verbatim: never re-serialized, \
                     never run through `ensure_valid_json_arguments`"
                );
                assert!(
                    tool_calls[0].is_malformed(),
                    "the captured arguments are still the malformed bytes the model sent"
                );
            }
            other => panic!("expected an assistant turn, got {other:?}"),
        }

        let empty = restored
            .messages()
            .iter()
            .next_back()
            .expect("the trailing assistant turn");
        match empty {
            Message::Assistant {
                content,
                reasoning_content,
                tool_calls,
            } => {
                assert!(tool_calls.is_empty());
                assert_eq!(
                    content.as_deref(),
                    Some(""),
                    "`content: Some(\"\")` must come back as an empty string, never `None`"
                );
                assert_eq!(
                    reasoning_content.as_deref(),
                    Some(""),
                    "an empty reasoning string is a different fact from no reasoning at all"
                );
            }
            other => panic!("expected an assistant turn, got {other:?}"),
        }
    }

    // ------------------------------------------------- (b) the pins and window

    /// The pins are byte-identical after restore, the witness still matches, and
    /// `pinned_prefix_end` still resolves — including through the loader's own
    /// pin rules.
    #[test]
    fn restored_pins_are_byte_identical_and_the_window_still_resolves() {
        let engine = live_engine();
        let section = engine.capture_manager_section();
        // A capture must be something the loader accepts (A §4 R-PIN-5).
        verify_pins(&section, PinStage::Stored).expect("a capture must satisfy the pin rules");
        assert_eq!(
            witness_of(&section.messages),
            Some(section.pin_witness),
            "the capture side and the load side must measure the pins identically"
        );
        let prompt = section.messages[0].clone();
        let goal = section.messages[1].clone();
        // The checkpoint at index 2 pins everything through index 2.
        assert_eq!(section.pinned_prefix_end, 3);
        assert_eq!(
            section.pin_witness.system_prompt_len,
            "SYSTEM PROMPT — pinned bytes (REQ-CORE-001)".len() as u32,
            "the witness is a **byte** length, not a char count"
        );
        assert_eq!(
            section.pin_witness.goal_len,
            "goal: pinned bytes, verbatim (REQ-CORE-002)".len() as u32
        );

        let mut restored = ContextEngine::new(1);
        restored.install_restored_messages(section.messages.clone());

        assert_eq!(
            MessageRecord::from_wire(&restored.messages()[0]),
            prompt,
            "messages[0] must be byte-identical after restore (REQ-CORE-001)"
        );
        assert_eq!(
            MessageRecord::from_wire(&restored.messages()[1]),
            goal,
            "messages[1] must be byte-identical after restore (REQ-CORE-002)"
        );
        assert_eq!(
            restored.pinned_prefix_end(),
            section.pinned_prefix_end as usize,
            "the pinned window must resolve to the same exclusive end after restore"
        );
        assert_eq!(restored.pin_witness(), section.pin_witness);
        verify_pins(&restored.capture_manager_section(), PinStage::Installed)
            .expect("the installed vector must satisfy the pin rules too");
        // The historic `System`-spelled checkpoint keeps its role: re-role'ing it
        // would silently move the window and change which turns compaction keeps.
        assert!(
            matches!(restored.messages()[2], Message::System { .. }),
            "the checkpoint at index 2 must keep its `System` role, got {:?}",
            restored.messages()[2]
        );
    }

    /// The scalars that change the next request round-trip as well (A §3.5), and a
    /// restore must not re-arm a latch the stored state said was spent.
    #[test]
    fn the_engine_scalars_round_trip() {
        let mut engine = live_engine();
        engine.inject_rebirth_advisory();
        let section = engine.capture_manager_section();
        assert_eq!(section.max_context_tokens, 8_000);
        assert!(
            section.rebirth_advisory_emitted,
            "the advisory latch must be captured"
        );

        let mut restored = ContextEngine::new(1);
        restored.install_restored_messages(section.messages.clone());
        assert!(
            !restored.rebirth_advisory_emitted(),
            "installing the vector alone must not touch the latches"
        );
        assert!(
            restored.should_advise_rebirth(),
            "an engine without the stored latches would advise again"
        );
        restored.restore_engine_flags(section.rebirth_advisory_emitted, 1);
        assert!(restored.rebirth_advisory_emitted());
        assert_eq!(restored.consecutive_rebirths(), 1);
        assert!(
            !restored.should_advise_rebirth(),
            "the advisory latch must not be re-armed by a restore"
        );
    }

    /// Capture is a **read**: it never heals, prunes, synthesizes or reorders, and
    /// two captures of an untouched engine are identical.
    #[test]
    fn capture_never_mutates_the_engine_and_witnesses_dangling_calls() {
        let engine = live_engine();
        let before = engine.messages().len();
        let first = engine.capture_manager_section();
        let second = engine.capture_manager_section();
        assert_eq!(
            first, second,
            "two captures of an untouched engine must be identical"
        );
        assert_eq!(
            engine.messages().len(),
            before,
            "capture must not add or drop messages"
        );
        // The fixture's tool result already exists, so the pairing state is `Healed`
        // (the `(aborted)` placeholder is present) — witnessed, not re-synthesized.
        assert_eq!(
            first.dangling,
            DanglingToolCallState::Healed {
                aborted_tool_call_ids: vec!["call-1".to_string()]
            }
        );

        let mut dangling = ContextEngine::new(8_000);
        dangling.set_system_prompt("s".to_string());
        dangling.set_goal("g".to_string());
        dangling.append(Message::Assistant {
            content: None,
            reasoning_content: None,
            tool_calls: vec![ToolCall {
                id: "call-x".to_string(),
                kind: "function".to_string(),
                function: ToolFunction {
                    name: TOOL_READ_FILE.to_string(),
                    arguments: "{}".to_string(),
                },
            }],
        });
        let captured = dangling.capture_manager_section();
        assert_eq!(
            captured.dangling,
            DanglingToolCallState::Dangling {
                calls: vec![DanglingCall {
                    tool_call_id: "call-x".to_string(),
                    function_name: TOOL_READ_FILE.to_string(),
                    arguments: "{}".to_string(),
                    dispatched: false,
                }]
            },
            "the capture must record the dangling call verbatim"
        );
        assert_eq!(
            dangling.dangling_state(),
            captured.dangling,
            "`dangling_state` must agree with the captured witness (one pairing rule, A D1)"
        );
        assert_eq!(
            dangling.messages().len(),
            3,
            "capture must not synthesize the missing tool result"
        );
    }

    // ------------------------------------------------- (c) no pin-setter clobber

    /// The restore seam never routes through a pin setter (A §4 R-PIN-2): both
    /// setters write **by index**, and the boot path calls them unconditionally, so
    /// a restore that used either one would clobber the pins with the freshly
    /// composed prompt and goal.
    #[test]
    fn install_restored_messages_never_calls_the_pin_setters() {
        let engine = live_engine();
        let section = engine.capture_manager_section();

        let mut restored = ContextEngine::new(8_000);
        restored.set_system_prompt("freshly composed prompt".to_string());
        restored.set_goal("freshly typed input".to_string());
        assert_eq!(
            restored.pin_setter_calls(),
            2,
            "the fixture must have exercised both by-index pin setters"
        );

        restored.install_restored_messages(section.messages.clone());

        assert_eq!(
            restored.pin_setter_calls(),
            2,
            "install_restored_messages must never call set_system_prompt / set_goal"
        );
        // The observable consequence (AC-P4): the installed pins are the STORED
        // bytes, not the freshly composed ones.
        assert_eq!(
            restored.messages()[0].content(),
            Some("SYSTEM PROMPT — pinned bytes (REQ-CORE-001)"),
            "the stored system prompt must win over the freshly composed one"
        );
        assert_eq!(
            restored.messages()[1].content(),
            Some("goal: pinned bytes, verbatim (REQ-CORE-002)"),
            "the stored goal must win over the freshly typed input"
        );
    }

    /// Source-level belt for (c). The runtime witness can only see the setters that
    /// exist today; this catches a future edit that re-routes the install through a
    /// setter, or through a helper that reaches one.
    #[test]
    fn the_restore_seam_source_never_routes_through_a_pin_setter() {
        let body = seam_body("pub fn install_restored_messages(");
        for forbidden in ["set_system_prompt", "set_goal", "append(", "push("] {
            assert!(
                !body.contains(forbidden),
                "the install seam body must not mention `{forbidden}`; body: {body}"
            );
        }
        assert!(
            body.contains("self.messages ="),
            "the install seam must assign the vector wholesale; body: {body}"
        );
    }

    /// Extract one fn body from `manager/context.rs` for a source-level assertion.
    fn seam_body(signature: &str) -> String {
        let source = include_str!("../manager/context.rs");
        let start = source
            .find(signature)
            .unwrap_or_else(|| panic!("`{signature}` must exist in manager/context.rs"));
        let tail = &source[start..];
        let open = tail.find('{').expect("a fn body opens with a brace");
        let mut depth = 0usize;
        for (index, ch) in tail.char_indices() {
            if index < open {
                continue;
            }
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return tail[open..=index].to_string();
                    }
                }
                _ => {}
            }
        }
        panic!("unbalanced braces after `{signature}`");
    }

    /// A vector without the pinned pair is a caller bug, not something to pad: the
    /// loader already refused it (R-PIN-5 step 1).
    #[test]
    #[should_panic(expected = "at least the pinned pair")]
    fn installing_a_vector_without_the_pinned_pair_panics() {
        let mut restored = ContextEngine::new(8_000);
        restored.install_restored_messages(vec![MessageRecord::System {
            content: "only a prompt".to_string(),
        }]);
    }

    /// `set_goal`'s empty-`System` padding is never reachable from a restore: a
    /// restored transcript with no real system prompt is the loader's `Refused`.
    #[test]
    fn install_never_pads_a_missing_system_prompt() {
        let mut restored = ContextEngine::new(8_000);
        restored.install_restored_messages(vec![
            MessageRecord::User {
                content: "a goal with no prompt before it".to_string(),
            },
            MessageRecord::User {
                content: "the next turn".to_string(),
            },
        ]);
        assert_eq!(restored.messages().len(), 2, "no record was inserted");
        assert_eq!(restored.pin_witness().system_prompt_len, 0);
        let refusal = verify_pins(&restored.capture_manager_section(), PinStage::Installed)
            .expect_err("messages[0] is not the system prompt: that is a refusal, not a pad case");
        assert!(
            refusal.message.contains("messages[0]"),
            "the refusal must name the pin that is wrong, got {}",
            refusal.message
        );
    }

    // ------------------------------------------------- (d) advisory grammar

    /// The injected-advisory text keeps its exact bytes, before and after a restore
    /// (A §4 R-PIN-4: the pinned window is found by a content match on these
    /// markers).
    #[test]
    fn injected_advisory_prefix_text_survives_untouched() {
        for marker in PRE_EXISTING_ADVISORY_PREFIXES {
            assert!(
                INJECTED_ADVISORY_PREFIXES.contains(marker),
                "the marker `{marker}` must stay registered in INJECTED_ADVISORY_PREFIXES, \
                 byte for byte"
            );
        }
        // The checkpoint entry IS the constant, not a paraphrase of it.
        assert!(INJECTED_ADVISORY_PREFIXES.contains(&REBIRTH_CHECKPOINT_PREFIX));

        let engine = live_engine();
        let section = engine.capture_manager_section();
        let mut restored = ContextEngine::new(8_000);
        restored.install_restored_messages(section.messages.clone());

        let legacy = restored
            .messages()
            .iter()
            .find(|m| {
                m.content()
                    .is_some_and(|c| c.starts_with("(SYSTEM: CONTEXT LIMIT EXCEEDED"))
            })
            .expect("the legacy notice must survive the round trip");
        assert_eq!(
            legacy.content(),
            Some("(SYSTEM: CONTEXT LIMIT EXCEEDED legacy notice, treated as opaque bytes"),
            "an advisory's text must not be trimmed, wrapped, normalized or re-rendered"
        );
        assert!(
            is_injected_advisory(legacy.content().unwrap_or_default()),
            "the legacy notice must still be recognized as injected"
        );

        let checkpoint = restored.messages()[2].content().unwrap_or_default();
        assert!(
            checkpoint.starts_with(REBIRTH_CHECKPOINT_PREFIX),
            "the checkpoint marker must survive verbatim"
        );
        assert_eq!(
            restored.pinned_prefix_end(),
            3,
            "the pinned window is found by content match: if the marker text moved, the \
             window would silently move with it"
        );
    }

    /// The restore notice is registered in the advisory grammar, so `perform_rebirth`
    /// can never promote it into the `messages[2]` instruction slot (bug M6), and it
    /// is always a `User` turn at index `>= 2` (bug M12).
    #[test]
    fn a_restore_notice_is_never_promoted_to_the_instruction_slot() {
        assert!(
            INJECTED_ADVISORY_PREFIXES.contains(&SESSION_RESTORE_NOTICE_PREFIX),
            "the restore notice prefix must be part of the advisory grammar (flow §4.6)"
        );
        let mut engine = live_engine();
        engine.append_restore_notice(&format!(
            "{SESSION_RESTORE_NOTICE_PREFIX} restored after an unclean ending) your last \
             instruction stands; these workers were interrupted"
        ));
        let index = engine.messages().len() - 1;
        assert!(index >= 2, "the notice must never land on a pin slot");
        assert!(
            matches!(engine.messages()[index], Message::User { .. }),
            "the notice must be a `User` turn (bug M12)"
        );
        assert!(
            is_injected_advisory(engine.messages()[index].content().unwrap_or_default()),
            "the notice must read as injected, never as a user instruction"
        );

        engine.perform_rebirth("summary of the work so far");
        let collapsed = engine.messages();
        assert_eq!(
            collapsed[0].content(),
            Some("SYSTEM PROMPT — pinned bytes (REQ-CORE-001)"),
            "rebirth must still pin the stored system prompt"
        );
        assert_eq!(
            collapsed[1].content(),
            Some("goal: pinned bytes, verbatim (REQ-CORE-002)"),
            "rebirth must still pin the stored goal"
        );
        assert_eq!(
            collapsed[2].content(),
            Some("keep going, then report"),
            "the restore notice must not displace the last genuine instruction"
        );
        assert!(
            collapsed[3]
                .content()
                .unwrap_or_default()
                .starts_with(REBIRTH_CHECKPOINT_PREFIX)
        );
    }

    /// A transcript without the pinned pair never gets a notice — a notice is never
    /// allowed to become a pin.
    #[test]
    fn append_restore_notice_refuses_without_the_pinned_pair() {
        let mut engine = ContextEngine::new(8_000);
        engine.set_system_prompt("only a prompt".to_string());
        engine.append_restore_notice(&format!("{SESSION_RESTORE_NOTICE_PREFIX} notice)"));
        assert_eq!(engine.messages().len(), 1, "nothing may be appended");

        engine.set_goal("now the pinned pair exists".to_string());
        engine.append_restore_notice(&format!("{SESSION_RESTORE_NOTICE_PREFIX} notice)"));
        assert_eq!(engine.messages().len(), 3);
        assert_eq!(engine.pinned_prefix_end(), 2);
        assert!(
            engine.messages()[2]
                .content()
                .unwrap_or_default()
                .starts_with(SESSION_RESTORE_NOTICE_PREFIX)
        );
    }

    // ------------------------------------------- the durable bundle round trip

    /// A capture is commit-ready: writing it through the one writer and reading it
    /// back through the loader yields the same vector, which `install_restored_messages`
    /// then installs byte-identically.
    #[test]
    fn a_capture_commits_and_reloads_through_the_bundle_byte_exactly() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let plan = crate::manager::phase::Plan::at(tmp.path());
        let bundle = plan.session_bundle_path();

        let engine = live_engine();
        let captured = engine.capture_manager_section();
        let payload = CapturePayload {
            manager: captured.clone(),
            ui: None,
            workers: Vec::new(),
            identity_floors: IdentityFloors {
                worker_id_seq_floor: 0,
                completed_seq_floor: 0,
                notice_id_floor: 0,
            },
        };
        std::fs::create_dir_all(&bundle).expect("bundle dir");
        let lock = CommitLock::new();
        let policy = CommitPolicy {
            min_interval: std::time::Duration::from_millis(0),
            max_bundle_bytes: 32 * 1024 * 1024,
            allow_throttled: true,
        };
        crate::session::commit::write_bundle(
            &bundle,
            "t009-session",
            &payload,
            ProcessEnding::InProgress,
            &policy,
            &lock,
        )
        .expect("a capture must commit");

        let outcome = load_bundle(&bundle);
        let restored_section = match &outcome {
            RestoreOutcome::Restored {
                session, report, ..
            } => {
                assert_eq!(
                    (
                        report.orphan_tool_results_pruned,
                        report.aborted_results_synthesized,
                        report.tool_results_regrouped
                    ),
                    (0, 0, 0),
                    "a pair-valid capture must need no normalization: {report:?}"
                );
                session.manager.clone()
            }
            other => panic!("the capture must restore, got {other:?}"),
        };
        assert_eq!(
            restored_section.messages, captured.messages,
            "the bundle must give back the captured vector element for element"
        );
        assert_eq!(restored_section.pin_witness, captured.pin_witness);
        assert_eq!(
            restored_section.pinned_prefix_end,
            captured.pinned_prefix_end
        );

        let mut restored = ContextEngine::new(1);
        restored.install_restored_messages(restored_section.messages.clone());
        for (index, record) in captured.messages.iter().enumerate() {
            assert_eq!(
                &MessageRecord::from_wire(&restored.messages()[index]),
                record,
                "message {index} changed across commit → load → install"
            );
        }
        assert!(matches!(restored.messages()[6], Message::Tool { .. }));
    }

    // ------------------------------------------------- the in-memory holder

    #[test]
    fn the_holder_keeps_the_newest_capture_for_the_committer() {
        let _guard = HOLDER_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_capture();
        let baseline = capture_count();
        assert!(last_capture().is_none(), "the holder starts empty");

        let engine = live_engine();
        capture_into_session_state(&engine);
        assert_eq!(capture_count(), baseline + 1, "one capture, one increment");
        let held = last_capture().expect("the capture is held in memory");
        assert_eq!(held, engine.capture_manager_section());

        let plain = ContextEngine::new(8_000);
        record_capture(plain.capture_manager_section());
        assert_eq!(
            last_capture().expect("the newest capture"),
            plain.capture_manager_section(),
            "the holder keeps the newest snapshot, not the oldest"
        );
        assert_eq!(capture_count(), baseline + 2);
        assert!(take_capture().is_some());
        assert!(last_capture().is_none(), "take empties the holder");
        clear_capture();
    }

    /// The superseded journal name is shared, not re-derived: it lives in
    /// `session/` (A §1 D1) and `manager::phase` only re-exports it for the readers
    /// that must still recognize a stale file.
    #[test]
    fn the_legacy_journal_name_is_shared_not_rederived() {
        assert_eq!(
            LEGACY_TRANSCRIPT_FILE,
            crate::manager::phase::TRANSCRIPT_FILE,
            "`manager::phase` must not keep its own copy of the superseded journal name"
        );
    }
}
