//! t-071 — steering-history correlation must be keyed by identity, never by a
//! bidirectional substring test (`a.contains(b) || b.contains(a)`).
//!
//! `crate::orchestrator::record_steering_exchange` (implemented in
//! `src/orchestrator/bus.rs`) merges an incoming steer exchange into the
//! recorded steering conversation history so a specialist's later
//! `reply_to_arbitrator` can still be attributed to the entry that is awaiting
//! it. The correlation is therefore an *identity lookup*:
//!
//! * the `notice_id` argument addresses the entry whose recorded arbitrator
//!   text names **that** notice id, and
//! * the `user_inquiry` argument addresses the entry whose recorded inquiry is
//!   **the same inquiry**.
//!
//! A substring test breaks both: `t-1` addresses `t-10`, `notice-1` addresses
//! `notice-10`, and the prose prefix "Deploy the new" addresses the entry
//! recorded for "Deploy the new allocator". The rewrite then erases the
//! `awaiting specialist reply` marker of the entry that was actually awaiting a
//! reply, so the specialist's real reply is appended as a duplicate/mis-attributed
//! entry.
//!
//! These tests drive the public bus API only (no LLM, no PTY, no filesystem),
//! and never assert process-global counters — each test installs its own
//! history and asserts on that history. The correlation state lives in a
//! process-global registry (`STEERING_HISTORY` in `src/orchestrator/bus.rs`), so
//! the tests are serialized with a local mutex and must be run with
//! `cargo test --test test_steer_history -- --test-threads=1`.

use marmennill::orchestrator::{
    SharedSteeringHistory, record_steering_exchange, set_steering_history,
};
use std::sync::{Arc, Mutex, MutexGuard};

/// Serializes the tests in this file: the active steering history is a
/// process-global registration, so installs must not interleave.
static HISTORY_BUS_LOCK: Mutex<()> = Mutex::new(());

fn lock_history_bus() -> MutexGuard<'static, ()> {
    HISTORY_BUS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Install a private, empty steering history as the active session history and
/// hand back the handle the test asserts on.
fn install_history() -> SharedSteeringHistory {
    let history: SharedSteeringHistory = Arc::new(std::sync::RwLock::new(Vec::new()));
    set_steering_history(Arc::clone(&history));
    history
}

/// Install a private steering history pre-seeded with `entries`
/// (`(user inquiry, arbitrator response)` pairs).
fn install_history_with(entries: &[(&str, &str)]) -> SharedSteeringHistory {
    let history: SharedSteeringHistory = Arc::new(std::sync::RwLock::new(
        entries
            .iter()
            .map(|(q, resp)| (q.to_string(), resp.to_string()))
            .collect(),
    ));
    set_steering_history(Arc::clone(&history));
    history
}

/// The shape `ui::bridge::recorded_response` records for a forwarded notice:
/// pending, and addressed to the notice it names.
fn pending(notice_id: &str, target: &str) -> String {
    format!("Forwarded notice {notice_id} to {target} (awaiting specialist reply)")
}

fn snapshot(history: &SharedSteeringHistory) -> Vec<(String, String)> {
    history.read().expect("history readable").clone()
}

/// A short inquiry must NOT be attributed to a longer recorded inquiry that
/// merely contains it — the `t-1` / `t-10` class of the removed
/// `inq.contains(q) || q.contains(inq)` test.
#[test]
fn short_inquiry_never_matches_a_longer_recorded_inquiry_that_contains_it() {
    let _guard = lock_history_bus();
    let history = install_history_with(&[(
        "t-10 investigate flaky parser test",
        &pending("notice-10", "coder"),
    )]);

    record_steering_exchange(
        None,
        "t-1",
        "Coder: t-1 is a different steer, already answered.",
    );

    let hist = snapshot(&history);
    assert_eq!(
        hist.len(),
        2,
        "an unrelated short inquiry must be recorded as its own entry instead of being \
         merged into the t-10 entry: {hist:?}"
    );
    // The entry that is genuinely awaiting the coder's reply must stay intact.
    assert_eq!(hist[0].0, "t-10 investigate flaky parser test");
    assert!(
        hist[0].1.contains("awaiting specialist reply"),
        "the pending t-10 entry must not be rewritten by a t-1 lookup: {:?}",
        hist[0].1
    );
    assert_eq!(hist[1].0, "t-1");
}

/// The same identity hazard on the *notice id* key: `notice-1` must not address
/// the entry recorded for `notice-10`.
#[test]
fn notice_id_is_matched_as_a_whole_token_not_as_a_prefix() {
    let _guard = lock_history_bus();
    let history = install_history_with(&[
        ("Summarise t-1", &pending("notice-1", "coder")),
        ("Summarise t-10", &pending("notice-10", "debugger")),
    ]);

    record_steering_exchange(Some("notice-1"), "Summarise t-1", "Coder: t-1 landed.");

    let hist = snapshot(&history);
    assert_eq!(hist.len(), 2, "no entry may be appended or lost: {hist:?}");
    assert_eq!(
        hist[0].1, "Coder: t-1 landed.",
        "the entry naming notice-1 is the one that must be updated"
    );
    assert!(
        hist[1].1.contains("awaiting specialist reply"),
        "the entry naming notice-10 is a different steer and must still be awaiting \
         its reply: {:?}",
        hist[1].1
    );
    assert_eq!(hist[1].0, "Summarise t-10");
}

/// The happy path of the exact lookup: an entry addressed by its exact notice id
/// is updated in place, so the history keeps one entry per steer.
#[test]
fn exact_notice_id_updates_the_recorded_entry_in_place() {
    let _guard = lock_history_bus();
    let history =
        install_history_with(&[("Please use async I/O", &pending("notice-async-1", "coder"))]);

    record_steering_exchange(
        Some("notice-async-1"),
        "Please use async I/O",
        "Coder: switched to tokio::fs.",
    );

    let hist = snapshot(&history);
    assert_eq!(hist.len(), 1, "one steer, one entry: {hist:?}");
    assert_eq!(hist[0].0, "Please use async I/O");
    assert_eq!(hist[0].1, "Coder: switched to tokio::fs.");
    assert!(
        !hist[0].1.contains("awaiting specialist reply"),
        "the reply replaces the pending marker: {:?}",
        hist[0].1
    );
}

/// The prose inquiry is a legitimate lookup key, but only as a *whole string*
/// (documented normalization: trim + ASCII case-fold). A prefix of a recorded
/// inquiry must never suppress the incoming entry.
#[test]
fn inquiry_prefix_does_not_suppress_a_new_entry() {
    let _guard = lock_history_bus();
    let history =
        install_history_with(&[("Deploy the new allocator", &pending("notice-7", "coder"))]);

    // Recon M3 scenario: a longer/different user message that merely contains the
    // recorded inquiry (here: a shorter prefix of it) must not rewrite it.
    record_steering_exchange(
        None,
        "Deploy the new",
        "Arbitrator: different steer entirely.",
    );

    let hist = snapshot(&history);
    assert_eq!(
        hist.len(),
        2,
        "the incoming steer must be appended, not folded into the entry it only \
         partially overlaps: {hist:?}"
    );
    assert!(
        hist[0].1.contains("awaiting specialist reply"),
        "the forwarded entry keeps its pending marker: {:?}",
        hist[0].1
    );
    assert_eq!(hist[1].0, "Deploy the new");
}

/// A duplicate/suppression decision must never be taken on an incidental
/// substring overlap: two steers whose texts overlap keep two entries, and the
/// entry that is awaiting a reply stays correlatable.
#[test]
fn duplicate_suppression_is_not_triggered_by_incidental_substring_overlap() {
    let _guard = lock_history_bus();
    let history = install_history();

    record_steering_exchange(
        Some("notice-1"),
        "Fix the parser",
        &pending("notice-1", "coder"),
    );
    // A different user message that contains the recorded inquiry verbatim as a
    // substring — the old `q.contains(inq)` branch merged these two steers.
    record_steering_exchange(
        None,
        "Fix the parser now",
        "Arbitrator: acknowledged the follow-up.",
    );

    let hist = snapshot(&history);
    assert_eq!(hist.len(), 2, "two steers, two entries: {hist:?}");
    assert_eq!(hist[0].0, "Fix the parser");
    assert_eq!(
        hist[0].1,
        pending("notice-1", "coder"),
        "the pending notice-1 entry must be untouched by an overlapping inquiry"
    );
    assert_eq!(hist[1].0, "Fix the parser now");

    // ... and the coder's real reply for notice-1 still lands on its own entry.
    record_steering_exchange(Some("notice-1"), "Fix the parser", "Coder: parser fixed.");
    let hist = snapshot(&history);
    assert_eq!(
        hist.len(),
        2,
        "the reply must not create a duplicate: {hist:?}"
    );
    assert_eq!(hist[0].1, "Coder: parser fixed.");
    assert_eq!(hist[1].1, "Arbitrator: acknowledged the follow-up.");
}

/// The documented normalization for the prose key: trim + ASCII case-fold only.
/// Everything else stays byte-for-byte significant.
#[test]
fn same_inquiry_matches_after_trim_and_case_fold_only() {
    let _guard = lock_history_bus();
    let history = install_history_with(&[("Where are we?", &pending("notice-3", "coder"))]);

    record_steering_exchange(None, "  WHERE ARE WE? ", "Coder: 12/17 tests pass.");

    let hist = snapshot(&history);
    assert_eq!(hist.len(), 1, "same inquiry, same entry: {hist:?}");
    assert_eq!(hist[0].1, "Coder: 12/17 tests pass.");
}

/// A reply for a notice id that no recorded entry names is a *miss*: nothing is
/// rewritten, the exchange is appended. (The removed stage-3 fallback rewrote
/// the most recent pending entry instead, which is what silently re-attributed
/// answers to unrelated steers.)
#[test]
fn unknown_notice_id_appends_instead_of_hijacking_the_latest_pending_entry() {
    let _guard = lock_history_bus();
    let history = install_history_with(&[("Waiting on the coder", &pending("notice-5", "coder"))]);

    record_steering_exchange(
        Some("notice-99"),
        "What about the debugger?",
        "Arbitrator: nothing recorded for notice-99.",
    );

    let hist = snapshot(&history);
    assert_eq!(hist.len(), 2, "a lookup miss must append: {hist:?}");
    assert_eq!(hist[0].0, "Waiting on the coder");
    assert!(
        hist[0].1.contains("awaiting specialist reply"),
        "the unrelated pending entry must survive a lookup miss: {:?}",
        hist[0].1
    );
    assert_eq!(hist[1].0, "What about the debugger?");
}

/// Empty/whitespace inquiries are never a wildcard: they address nothing, so the
/// exchange is appended (the old `contains("")` branch matched *every* entry).
#[test]
fn empty_inquiry_matches_nothing_and_appends() {
    let _guard = lock_history_bus();
    let history = install_history_with(&[("A recorded steer", &pending("notice-8", "coder"))]);

    record_steering_exchange(None, "   ", "Arbitrator: empty inquiry.");

    let hist = snapshot(&history);
    assert_eq!(
        hist.len(),
        2,
        "an empty inquiry addresses no entry: {hist:?}"
    );
    assert!(
        hist[0].1.contains("awaiting specialist reply"),
        "the recorded entry must be untouched: {:?}",
        hist[0].1
    );
}

/// Source guard: the correlation in `src/orchestrator/bus.rs` must keep the
/// exact keyed matchers and must never reintroduce a substring fallback.
/// Needles are assembled at runtime (style of the guards in
/// `orchestrator::steer_tests` / `crate::markers`) and comment lines are
/// excluded, so neither this test's own text nor the explanatory doc comments
/// can satisfy them.
#[test]
fn bus_correlation_keeps_no_substring_fallback() {
    let src = include_str!("../src/orchestrator/bus.rs");
    let code: String = src
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<&str>>()
        .join("\n");

    for needle in [
        &["contains(q", "_"].concat(),
        &["contains(qu", "ery"].concat(),
        &["contains(in", "q"].concat(),
        &["inq.", "contains("].concat(),
        &["q.", "contains(inq"].concat(),
        &["contains(n", "id"].concat(),
        &["contains(not", "ice_id"].concat(),
    ] {
        assert!(
            !code.contains(needle),
            "bus.rs must not reintroduce a substring lookup (`{needle}`): steering \
             history is correlated by identity key only"
        );
    }

    for owner in [
        &["entry_names_notice", "_id("].concat(),
        &["same_steering", "_inquiry("].concat(),
    ] {
        assert!(
            code.contains(owner),
            "bus.rs must keep delegating to the single exact-match owner (`{owner}`)"
        );
    }
}
