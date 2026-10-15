//! Sleep / wait tool handlers.
//!
//! ## Duration authority
//! The whole duration pipeline (duration-key aliases, JSON number shapes, the
//! fallback default and the min/max clamp) lives in one place:
//! [`crate::tool_args::sleep_duration_secs`] (dedup gate t-044, sweep C removed
//! the three divergent copies that used to be re-typed in this file). Both
//! entry points below are deliberately thin wrappers over that owner, so the
//! sync and the async tool path can never disagree about what an argument
//! means — any input accepted by one is accepted with the same value by the
//! other.
//!
//! What still differs between the two entry points is only *how* the wait is
//! performed (blocking vs. async) and which cancellation sources they observe
//! (the async path additionally honours the worker's task-local token).

use super::common::{ToolError, ToolResult};
use crate::tool_args::sleep_duration_secs;
use serde_json::Value;

/// Sync sleep entry point (used by the blocking dispatch paths).
pub(crate) fn handle_sleep(args: &Value) -> Result<ToolResult, ToolError> {
    let secs = sleep_duration_secs(args);
    let reason_clause = sleep_reason_clause(args);

    let cancel = crate::orchestrator::bus::global_cancellation_token();
    if cancel.is_cancelled() {
        return Ok(sleep_cancelled_before_start());
    }

    let completed = if let Ok(handle) = tokio::runtime::Handle::try_current() {
        match handle.runtime_flavor() {
            // Multi-thread runtime: hand the blocking wait to the blocking pool
            // and drive the cancellable async sleep from there.
            tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(|| {
                handle.block_on(async {
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(secs)) => true,
                        _ = cancel.cancelled() => false,
                    }
                })
            }),
            // Current-thread runtime: `block_in_place` is unavailable there, so
            // poll the token on a blocking thread.
            _ => wait_with_polling(secs, &cancel),
        }
    } else {
        wait_with_polling(secs, &cancel)
    };

    if completed {
        Ok(sleep_completed(secs, &reason_clause))
    } else {
        Ok(sleep_interrupted())
    }
}

/// Async sleep entry point (used by the async dispatch paths).
pub async fn handle_sleep_async(arguments: &Value) -> Result<ToolResult, ToolError> {
    let secs = sleep_duration_secs(arguments);
    let reason_clause = sleep_reason_clause(arguments);

    if crate::orchestrator::is_current_or_global_cancelled() {
        return Ok(sleep_cancelled_before_start());
    }

    let cancel = crate::orchestrator::bus::global_cancellation_token();
    let worker_token = crate::orchestrator::CURRENT_WORKER_TOKEN
        .try_with(|t| t.clone())
        .ok();

    let completed = tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_secs(secs)) => true,
        _ = cancel.cancelled() => false,
        _ = async {
            if let Some(ref t) = worker_token {
                t.cancelled().await
            } else {
                std::future::pending::<()>().await
            }
        } => false,
    };

    if completed {
        Ok(sleep_completed(secs, &reason_clause))
    } else {
        Ok(sleep_interrupted())
    }
}

/// Blocking poll-based wait, used when no multi-thread runtime is available.
fn wait_with_polling(secs: u64, cancel: &tokio_util::sync::CancellationToken) -> bool {
    let start = std::time::Instant::now();
    let dur = std::time::Duration::from_secs(secs);
    while start.elapsed() < dur {
        if cancel.is_cancelled() {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    !cancel.is_cancelled()
}

/// The ` (reason)` suffix appended to a completed sleep, from the `reason` key.
fn sleep_reason_clause(args: &Value) -> String {
    let reason = args.get("reason").and_then(Value::as_str).unwrap_or("");
    if reason.is_empty() {
        String::new()
    } else {
        format!(" ({reason})")
    }
}

/// Success result shared by both sleep paths.
fn sleep_completed(secs: u64, reason_clause: &str) -> ToolResult {
    ToolResult::ok(format!("Slept for {secs} seconds{reason_clause}."))
}

/// Result shared by both sleep paths when the call never started.
fn sleep_cancelled_before_start() -> ToolResult {
    ToolResult::err("Sleep cancelled before starting.")
}

/// Result shared by both sleep paths when the wait was cut short.
fn sleep_interrupted() -> ToolResult {
    ToolResult::err("Sleep interrupted by cancellation signal.")
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_args::{
        SLEEP_DEFAULT_SECS, SLEEP_MAX_SECS, SLEEP_MIN_SECS, sleep_duration_secs,
    };
    use crate::tool_names::TOOL_SLEEP;
    use serde_json::{Value, json};
    use std::time::{Duration, Instant};
    use tokio::sync::oneshot;
    use tokio_util::sync::CancellationToken;

    /// How long the parity test watches both paths before it requires the
    /// ceiling-clamped rows to be *still* waiting.
    ///
    /// It sits strictly above the resolved duration of every row that is run to
    /// completion (including the [`SLEEP_DEFAULT_SECS`] fallback) and strictly
    /// below the [`SLEEP_MAX_SECS`] ceiling, so one window proves both that the
    /// fallback rows really slept the default and that a ceiling row was never
    /// silently shortened to the default — which is exactly the class of
    /// sync/async divergence this task removed.
    const PROBE_SECS: u64 = SLEEP_DEFAULT_SECS + 1;
    /// Slack accepted when asserting that a completed wait really took its
    /// resolved duration (the sync path polls the token every 50 ms; both paths
    /// are observed by a 100 ms sampling loop).
    const TIMING_SLACK: Duration = Duration::from_millis(250);
    /// Maximum allowed difference between the two paths' measured wait for the
    /// same input.
    const PATH_DRIFT: Duration = Duration::from_millis(600);

    const _: () = assert!(PROBE_SECS > SLEEP_DEFAULT_SECS && PROBE_SECS < SLEEP_MAX_SECS);

    /// The shared sleep-argument input table: `(label, arguments, seconds each
    /// path is expected to actually wait)`.
    ///
    /// It is driven through *both* sleep paths so the copies of the
    /// canonicalization this task merged can never diverge again unnoticed: the
    /// async copy used to be missing the `as_i64` branch the sync copy had, and
    /// a third copy in the tool-argument preview defaulted without clamping.
    fn parity_table() -> Vec<(&'static str, Value, u64)> {
        vec![
            // missing / null / wrong type -> the shared default
            ("missing key", json!({}), SLEEP_DEFAULT_SECS),
            ("null value", json!({"seconds": null}), SLEEP_DEFAULT_SECS),
            ("bool", json!({"seconds": true}), SLEEP_DEFAULT_SECS),
            ("object", json!({"seconds": {"n": 1}}), SLEEP_DEFAULT_SECS),
            ("array", json!({"seconds": [1]}), SLEEP_DEFAULT_SECS),
            (
                "unparseable string",
                json!({"seconds": "5s"}),
                SLEEP_DEFAULT_SECS,
            ),
            // negative numbers are not durations -> the shared default
            ("negative int", json!({"seconds": -3}), SLEEP_DEFAULT_SECS),
            (
                "negative string",
                json!({"seconds": "-3"}),
                SLEEP_DEFAULT_SECS,
            ),
            (
                "negative float",
                json!({"seconds": -2.5}),
                SLEEP_DEFAULT_SECS,
            ),
            (
                "float below min",
                json!({"seconds": 0.5}),
                SLEEP_DEFAULT_SECS,
            ),
            // floats outside the tool's own range stay unusable -> the default
            // (they were never parsed by any pre-existing copy either)
            ("float 1e9", json!({"seconds": 1e9}), SLEEP_DEFAULT_SECS),
            (
                "float above max",
                json!({"seconds": 300.9}),
                SLEEP_DEFAULT_SECS,
            ),
            // string-encoded numbers were accepted by every copy already
            ("string number", json!({"seconds": "5"}), 5),
            // plain integers
            ("int 5", json!({"seconds": 5}), 5),
            ("int 0 -> min", json!({"seconds": 0}), SLEEP_MIN_SECS),
            ("int 1", json!({"seconds": 1}), SLEEP_MIN_SECS),
            // fractional durations inside the range truncate toward zero
            ("float 2.5 truncates", json!({"seconds": 2.5}), 2),
            ("float 6.0 exact", json!({"seconds": 6.0}), 6),
            // duration-key aliases
            ("alias duration", json!({"duration": 2}), 2),
            (
                "alias duration_seconds string",
                json!({"duration_seconds": "1"}),
                1,
            ),
            // the `reason` payload must never shift the duration
            (
                "reason does not change duration",
                json!({"seconds": 1, "reason": "waiting for build"}),
                SLEEP_MIN_SECS,
            ),
            // rows clamped to the ceiling: probed, never waited out
            ("int 300", json!({"seconds": 300}), SLEEP_MAX_SECS),
            ("int 301 -> max", json!({"seconds": 301}), SLEEP_MAX_SECS),
            (
                "u64-sized int -> max",
                json!({"seconds": 18_446_744_073_709_551_615u64}),
                SLEEP_MAX_SECS,
            ),
            ("float 299.9 truncates", json!({"seconds": 299.9}), 299),
        ]
    }

    /// One input driven through both sleep paths at the same time, with the
    /// result and completion time of each side recorded as soon as it arrives.
    struct InFlight {
        label: &'static str,
        args: Value,
        expected: u64,
        started: Instant,
        async_task: tokio::task::JoinHandle<()>,
        async_rx: oneshot::Receiver<Result<ToolResult, ToolError>>,
        async_elapsed: Option<Duration>,
        sync_rx: oneshot::Receiver<Result<ToolResult, ToolError>>,
        sync_res: Option<Result<ToolResult, ToolError>>,
        sync_elapsed: Option<Duration>,
    }

    /// Start `handle_sleep_async` and report its result through a channel, so
    /// the sampling loop below never has to consume the [`JoinHandle`] — that
    /// stays available to abort a ceiling-length wait.
    fn spawn_async_sleeper(
        args: &Value,
    ) -> (
        oneshot::Receiver<Result<ToolResult, ToolError>>,
        tokio::task::JoinHandle<()>,
    ) {
        let (tx, rx) = oneshot::channel();
        let args = args.clone();
        let task = tokio::spawn(async move {
            let _ = tx.send(handle_sleep_async(&args).await);
        });
        (rx, task)
    }

    /// Start `handle_sleep` on a plain thread and report its result through a
    /// channel.
    ///
    /// A dedicated thread is deliberate: with no runtime in scope the handler
    /// takes its polling wait branch, so it never blocks or parks a tokio
    /// worker, and the thread is *detached* on purpose — an unfinished
    /// ceiling-length wait must not be parked as a blocking task, because
    /// dropping the test runtime would then wait out the full five minutes and
    /// hang the suite. Leftovers are killed when the test binary exits.
    fn spawn_sync_sleeper(args: &Value) -> oneshot::Receiver<Result<ToolResult, ToolError>> {
        let (tx, rx) = oneshot::channel();
        let args = args.clone();
        std::thread::spawn(move || {
            let _ = tx.send(handle_sleep(&args));
        });
        rx
    }

    /// Table-driven parity: every input shape must resolve to the *same*
    /// duration, the *same* result and the *same* wait on the sync path and on
    /// the async path.
    ///
    /// Cancellation is never touched here: the sync path's cancellation surface
    /// is the process-global token, which belongs to the whole test binary and
    /// is therefore driven end-to-end by `tests/test_harness.rs`
    /// (`test_sleep_execution_and_cancellation`). Everything asserted here is
    /// hermetic.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_and_async_sleep_paths_agree_on_every_input_shape() {
        let mut rows: Vec<InFlight> = Vec::new();
        for (label, args, expected) in parity_table() {
            let (async_rx, async_task) = spawn_async_sleeper(&args);
            let sync_rx = spawn_sync_sleeper(&args);
            rows.push(InFlight {
                label,
                args,
                expected,
                started: Instant::now(),
                async_task,
                async_rx,
                async_elapsed: None,
                sync_rx,
                sync_res: None,
                sync_elapsed: None,
            });
        }

        // Sample both paths until the probe window closes.
        let deadline = Instant::now() + Duration::from_secs(PROBE_SECS);
        while Instant::now() < deadline {
            for row in rows.iter_mut() {
                if row.sync_res.is_none()
                    && let Ok(res) = row.sync_rx.try_recv()
                {
                    row.sync_res = Some(res);
                    row.sync_elapsed = Some(row.started.elapsed());
                }
                if row.async_elapsed.is_none() && row.async_task.is_finished() {
                    row.async_elapsed = Some(row.started.elapsed());
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        for row in rows {
            let what = format!("{} ({})", row.label, row.args);
            let expected = row.expected;
            let expected_msg = sleep_completed(expected, &sleep_reason_clause(&row.args)).content;

            // Drain the async result now that the sampling window is closed: a
            // finished task returns instantly, an unfinished one is aborted so
            // no ceiling-length timer outlives the test.
            let async_res = if row.async_elapsed.is_some() {
                Some(
                    tokio::time::timeout(Duration::from_secs(2), row.async_rx)
                        .await
                        .unwrap_or_else(|_| {
                            panic!("async sleep finished but reported nothing: {what}")
                        })
                        .expect("async sleep reporter alive")
                        .expect("the async sleep handler is infallible"),
                )
            } else {
                row.async_task.abort();
                None
            };

            if expected < PROBE_SECS {
                let sync_res = match row.sync_res {
                    Some(res) => res.expect("the sync sleep handler is infallible"),
                    None => {
                        panic!("sync sleep never returned for {what}; expected a {expected}s wait")
                    }
                };
                let async_res = async_res.unwrap_or_else(|| {
                    panic!(
                        "async sleep never returned for {what}; expected a {}s wait",
                        expected
                    )
                });
                let sync_elapsed = row.sync_elapsed.expect("sync completion recorded above");
                let async_elapsed = row.async_elapsed.expect("async completion recorded above");

                assert_eq!(
                    async_res.content, sync_res.content,
                    "sync/async divergence for {what}"
                );
                assert_eq!(
                    async_res.is_error, sync_res.is_error,
                    "{what}: the error flag diverged"
                );
                assert!(
                    !sync_res.is_error,
                    "{what} should have completed, got: {}",
                    sync_res.content
                );
                assert_eq!(
                    sync_res.content, expected_msg,
                    "{what}: wrong duration resolved"
                );
                for (path, elapsed) in [("sync", sync_elapsed), ("async", async_elapsed)] {
                    assert!(
                        elapsed
                            >= Duration::from_millis(expected * 1_000).saturating_sub(TIMING_SLACK),
                        "{path} sleep returned after {elapsed:?} for {what}; expected a {}s wait",
                        expected
                    );
                }
                let drift = sync_elapsed.max(async_elapsed) - sync_elapsed.min(async_elapsed);
                assert!(
                    drift <= PATH_DRIFT,
                    "{what}: the two paths drifted by {drift:?} (sync {sync_elapsed:?}, async {async_elapsed:?})"
                );
            } else {
                if let Some(res) = row.sync_res {
                    panic!(
                        "sync sleep shortened {what} to `{}`; expected a {expected}s wait",
                        res.expect("the sync sleep handler is infallible").content
                    );
                }
                if let Some(elapsed) = row.async_elapsed {
                    panic!(
                        "async sleep shortened {what} to {elapsed:?}; expected a {}s wait",
                        expected
                    );
                }
            }
        }
    }

    /// The exact numbers behind every table row, asserted against the single
    /// owner both handlers call — and against the tool-argument preview that
    /// calls the same owner. This is what pins the ceiling rows that the parity
    /// test cannot wait out for five minutes.
    #[test]
    fn sleep_owner_table_matches_both_paths_contract() {
        for (label, args, expected) in parity_table() {
            assert_eq!(
                sleep_duration_secs(&args),
                expected,
                "owner disagrees for {label}: {args}"
            );
            assert_eq!(
                crate::tool_args::preview_tool_args(TOOL_SLEEP, &args),
                format!("{expected}s"),
                "the tool-argument preview disagrees with the handler owner for {label}: {args}"
            );
        }
    }

    /// The async path honours the worker's task-local token: a call whose token
    /// is already cancelled must be refused before the wait starts (hermetic —
    /// no process-global state involved).
    #[tokio::test]
    async fn async_sleep_refuses_a_call_with_a_cancelled_worker_token() {
        let token = CancellationToken::new();
        token.cancel();
        let args = json!({"seconds": 1, "reason": "parity"});
        let scoped = crate::orchestrator::CURRENT_WORKER_TOKEN
            .scope(token, async move { handle_sleep_async(&args).await });

        let res = tokio::time::timeout(Duration::from_secs(2), scoped)
            .await
            .expect("a cancelled call must be refused immediately")
            .expect("infallible");
        assert!(res.is_error);
        assert_eq!(res.content, sleep_cancelled_before_start().content);
    }

    /// The async path also interrupts an in-flight wait when the worker token
    /// is cancelled (hermetic: task-local token only).
    #[tokio::test]
    async fn async_sleep_is_interrupted_by_the_worker_token() {
        let token = CancellationToken::new();
        let args = json!({"seconds": 30});
        let scoped = crate::orchestrator::CURRENT_WORKER_TOKEN.scope(token.clone(), async move {
            handle_sleep_async(&args).await
        });
        let task = tokio::spawn(scoped);
        // Let the wait actually start, so this exercises the mid-flight
        // interruption and not the pre-flight guard.
        tokio::task::yield_now().await;
        token.cancel();

        let res = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("the wait must be cut short, not run to 30s")
            .expect("sleep task panicked")
            .expect("infallible");
        assert!(res.is_error);
        assert_eq!(res.content, sleep_interrupted().content);
    }

    /// The sync path's wait primitive is cancellation-aware too, asserted with a
    /// *local* token so the process-global token stays untouched by this suite.
    /// The end-to-end sync cancellation is covered by
    /// `tests/test_harness.rs::test_sleep_execution_and_cancellation`.
    #[test]
    fn sync_polling_wait_is_interrupted_by_a_local_token() {
        let token = CancellationToken::new();
        let killer = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            killer.cancel();
        });

        let start = Instant::now();
        assert!(
            !wait_with_polling(30, &token),
            "the sync wait loop must observe cancellation"
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "the sync wait loop kept sleeping for {:?}",
            start.elapsed()
        );

        // An uncancelled token lets the wait run its full course (zero-length
        // here, so the assertion costs nothing).
        assert!(wait_with_polling(0, &CancellationToken::new()));
    }

    /// Structural guard (style of the source-shape checks in
    /// `agents::runner::fix_loop`): the sleep handlers must not re-type the
    /// default/clamp literals they used to own — they must go through the single
    /// owner — and they must share the same outcome helpers, which is what makes
    /// the two paths' refusals identical by construction. The needles are
    /// assembled at runtime so this test's own text cannot satisfy them.
    #[test]
    fn sleep_handlers_retype_no_default_or_clamp_literals() {
        let src = include_str!("sleep.rs");
        // Only the production part of the file: the table above legitimately
        // spells out the bound values as expectations.
        let code = src
            .split(&["#[cfg(", "test)", "]"].concat())
            .next()
            .expect("sleep.rs contains the test attribute");

        for needle in [
            &["unwrap_or(", "5"].concat(),
            &["clamp(", "1"].concat(),
            &["clamp(", "300"].concat(),
            "300",
            &["DEFAULT", "_SLEEP"].concat(),
            &["max_sleep"].concat(),
        ] {
            assert!(
                !code.contains(needle),
                "sleep.rs must not re-type the sleep bound `{needle}`: it belongs to tool_args"
            );
        }

        // The owner's name is spelled by the shared table (`TOOL_SLEEP` + the
        // suffix), never as a raw tool-name literal (t-069).
        let owner_call = [TOOL_SLEEP, "_duration_secs("].concat();
        assert_eq!(
            code.matches(&owner_call).count(),
            2,
            "both sleep handlers must call the single owner exactly once each"
        );
        for shared_outcome in [
            &["Ok(sleep", "_cancelled_before_start()"].concat(),
            &["Ok(sleep", "_interrupted()"].concat(),
            &["Ok(sleep", "_completed("].concat(),
        ] {
            assert_eq!(
                code.matches(shared_outcome).count(),
                2,
                "both sleep handlers must produce `{shared_outcome}` identically"
            );
        }
    }
}
