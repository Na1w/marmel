//! Generic retry-with-linear-backoff policy — the single owner of the retry
//! contract shared by the LLM chat client and the MCP HTTP/SSE transport.
//!
//! The policy is: up to [`MAX_ATTEMPTS`] total attempts, sleeping
//! `BACKOFF_BASE_MS × attempt` between retryable failures, retrying only on
//! errors whose implementor of [`Retryable`] marks retryable (for the LLM
//! client: HTTP 503/429/502/504, transport failures, stream errors and the
//! watchdog timeouts). Application-level errors are never retried.
//!
//! **Consumers (cluster C2, see `docs/recon_duplication_net.md`):**
//! `crate::llm::client::ChatClient::chat_stream` and — since the MCP
//! unification — both MCP HTTP/SSE request paths,
//! `crate::mcp::http::HttpSseConnection::post` and
//! `HttpSseConnection::send_notification`.

use std::time::Duration;

/// Maximum total attempts (initial + up to 2 retries for 503/429/timeouts).
pub const MAX_ATTEMPTS: u32 = 3;
/// Backoff base: sleep = `BACKOFF_BASE_MS × attempt`.
pub const BACKOFF_BASE_MS: u64 = 1000;

/// Marker trait for errors that know whether they are worth retrying.
///
/// Implemented by `crate::llm::client::ChatError` (LLM backend) and
/// `crate::mcp::http::McpError` (MCP HTTP/SSE transport), so both clients share
/// one policy with a per-client retryability classification. Both types are
/// crate-internal, hence referenced by path instead of as an intra-doc link.
pub trait Retryable: std::fmt::Display {
    /// Whether a new attempt is likely to succeed for this error class.
    fn is_retryable(&self) -> bool;
}

/// A single retried operation, run once per attempt by [`retry_with_backoff`].
///
/// This is a trait rather than a closure on purpose: an attempt future has to
/// borrow the per-call `state` (e.g. the LLM client's mutable `on_delta`
/// callback) for the lifetime of *that* attempt. A `FnMut(&mut S) -> Fut`
/// closure cannot express it — rustc demands one lifetime-independent `Fut`
/// ("`'1` must outlive `'2`") — and a boxed `FnMut(&'s mut S) ->
/// Pin<Box<dyn Future + 's>>` forces `Send` onto the state, which would make
/// [`crate::llm::client::ChatClient::chat_stream`] reject the (conditionally
/// `Send`) callbacks it already accepts. With `attempt` returning a future that
/// captures the lifetimes of `self` and `state`, the retry future stays `Send`
/// exactly when the state and operation are `Send`, so callers keep spawning
/// their turns.
///
/// The signature is written as `fn ... -> impl Future<Output = ...>` rather than
/// `async fn` on purpose: it says out loud that the hidden future borrows
/// `self` and `state`, it keeps the trait free of any `async_fn_in_trait`
/// waiver, and implementors stay free to write the `async fn attempt` form —
/// both spellings lower to the same hidden future type.
pub trait RetryOp<S> {
    /// Error type carrying the per-client retryability classification.
    type Error: Retryable;
    /// Value produced by a successful attempt.
    type Output;

    /// Run one attempt, borrowing `state` for the duration of that attempt.
    fn attempt(
        &mut self,
        state: &mut S,
    ) -> impl std::future::Future<Output = Result<Self::Output, Self::Error>>;
}

/// Run `op` up to `max_attempts` times, sleeping `base_ms × attempt` between
/// retryable failures. Returns the first success or the last error.
///
/// `tag` is used in the warn/error log lines (e.g. `"LLM backend call"` or
/// `"MCP call to '<server>' for '<method>'"`).
///
/// `state` is handed to `op` on every attempt so the operation can carry
/// mutable per-call state (e.g. the LLM client's `on_delta` callback) across
/// retries. Pass `&mut ()` when no state is needed.
pub async fn retry_with_backoff<O, S>(
    max_attempts: u32,
    base_ms: u64,
    tag: &str,
    state: &mut S,
    mut op: O,
) -> Result<O::Output, O::Error>
where
    O: RetryOp<S>,
{
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match op.attempt(&mut *state).await {
            Ok(v) => return Ok(v),
            Err(e) if e.is_retryable() && attempt < max_attempts => {
                let ms = base_ms * attempt as u64;
                tracing::warn!(
                    "{tag} attempt {attempt}/{max_attempts} failed ({e}), retrying in {ms}ms..."
                );
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
            Err(e) => {
                tracing::error!("{tag} failed after {attempt} attempts: {e}");
                return Err(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal `Retryable` error for exercising the policy in isolation.
    #[derive(Debug, PartialEq)]
    struct TestError {
        label: String,
        retriable: bool,
    }

    impl std::fmt::Display for TestError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.label)
        }
    }

    impl Retryable for TestError {
        fn is_retryable(&self) -> bool {
            self.retriable
        }
    }

    /// Test operation: fails `fail_times` times (then succeeds), counting its
    /// own invocations and the `state` it is handed on each attempt.
    struct TestOp {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        fail_times: usize,
        retriable: bool,
    }

    impl TestOp {
        fn new(
            fail_times: usize,
            retriable: bool,
        ) -> (Self, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
            let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            (
                Self {
                    calls: calls.clone(),
                    fail_times,
                    retriable,
                },
                calls,
            )
        }

        fn attempt_no(&self) -> usize {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
        }
    }

    impl RetryOp<usize> for TestOp {
        type Error = TestError;
        type Output = usize;

        async fn attempt(&mut self, state: &mut usize) -> Result<usize, TestError> {
            let attempt = self.attempt_no();
            *state += 1;
            if attempt <= self.fail_times {
                Err(TestError {
                    label: format!("fail {attempt}"),
                    retriable: self.retriable,
                })
            } else {
                Ok(*state)
            }
        }
    }

    /// Operation that always fails, stamping the clock at the start of every
    /// attempt so the *shape* of the backoff schedule can be inspected without
    /// depending on wall-clock jitter of a single total measurement.
    struct StampOp;

    impl RetryOp<Vec<tokio::time::Instant>> for StampOp {
        type Error = TestError;
        type Output = ();

        async fn attempt(
            &mut self,
            stamps: &mut Vec<tokio::time::Instant>,
        ) -> Result<(), TestError> {
            stamps.push(tokio::time::Instant::now());
            Err(TestError {
                label: format!("fail {}", stamps.len()),
                retriable: true,
            })
        }
    }

    /// Success path: the first attempt is returned, nothing is retried.
    #[tokio::test]
    async fn succeeds_on_first_attempt() {
        let (op, calls) = TestOp::new(0, true);
        let mut state = 0usize;
        let got = retry_with_backoff(MAX_ATTEMPTS, BACKOFF_BASE_MS, "test op", &mut state, op)
            .await
            .unwrap();

        assert_eq!(got, 1);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Retry-on-retriable-error: transient failures are retried until success,
    /// and the same mutable `state` is threaded through every attempt.
    #[tokio::test]
    async fn retries_retriable_error_and_threads_state() {
        let (op, calls) = TestOp::new(2, true);
        let mut state = 0usize;
        let got = retry_with_backoff(MAX_ATTEMPTS, BACKOFF_BASE_MS, "test op", &mut state, op)
            .await
            .unwrap();

        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "two transient failures then a success"
        );
        assert_eq!(got, 3);
        assert_eq!(state, 3, "state mutated by every attempt is preserved");
    }

    /// Exhaustion: exactly `max_attempts` runs and the *last* error is returned.
    #[tokio::test]
    async fn exhausts_attempts_and_returns_last_error() {
        let (op, calls) = TestOp::new(10, true);
        let mut state = 0usize;
        let err = retry_with_backoff(MAX_ATTEMPTS, BACKOFF_BASE_MS, "test op", &mut state, op)
            .await
            .unwrap_err();

        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            MAX_ATTEMPTS as usize
        );
        assert_eq!(err.label, "fail 3");
        assert!(err.retriable);
    }

    /// Non-retriable errors abort immediately: one attempt, no backoff sleep.
    #[tokio::test]
    async fn non_retriable_error_fails_fast() {
        let (op, calls) = TestOp::new(10, false);
        let mut state = 0usize;
        let err = retry_with_backoff(MAX_ATTEMPTS, BACKOFF_BASE_MS, "test op", &mut state, op)
            .await
            .unwrap_err();

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(err.label, "fail 1");
        assert!(!err.retriable);
    }

    /// Backoff schedule: the sleep before each retry grows linearly with the
    /// attempt number (`base_ms × attempt`); nothing is slept after the last one.
    #[tokio::test]
    async fn backoff_is_linear_in_attempt_number() {
        const BASE_MS: u64 = 500;
        let mut stamps: Vec<tokio::time::Instant> = Vec::new();
        let err = retry_with_backoff(3, BASE_MS, "test op", &mut stamps, StampOp)
            .await
            .unwrap_err();

        assert_eq!(err.label, "fail 3", "the last error is returned");
        assert_eq!(stamps.len(), 3, "exactly max_attempts attempts");

        let first_gap = stamps[1] - stamps[0];
        let second_gap = stamps[2] - stamps[1];
        // Never shorter than the policy prescribes...
        assert!(
            first_gap >= Duration::from_millis(BASE_MS),
            "attempt 2 started after {first_gap:?}, expected at least {BASE_MS}ms"
        );
        assert!(
            second_gap >= Duration::from_millis(2 * BASE_MS),
            "attempt 3 started after {second_gap:?}, expected at least {}ms",
            2 * BASE_MS
        );
        // ...and never longer than a single backoff sleep.
        assert!(
            first_gap < Duration::from_millis(2 * BASE_MS),
            "only one sleep before attempt 2, got {first_gap:?}"
        );
        assert!(
            second_gap > first_gap,
            "backoff must grow with the attempt number: {first_gap:?} -> {second_gap:?}"
        );
    }

    /// `max_attempts = 1` disables retrying entirely.
    #[tokio::test]
    async fn single_attempt_policy_never_retries() {
        let (op, calls) = TestOp::new(10, true);
        let mut state = 0usize;
        let err = retry_with_backoff(1, BACKOFF_BASE_MS, "test op", &mut state, op)
            .await
            .unwrap_err();

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(err.label, "fail 1");
    }

    /// Guard for the callers that `tokio::spawn` retried turns: the retry
    /// future must stay `Send` whenever state and operation are `Send`.
    #[tokio::test]
    async fn retry_future_is_send_for_send_state() {
        fn assert_send<T: Send>(_: &T) {}

        let (op, _calls) = TestOp::new(1, true);
        let mut state = 0usize;
        let fut = retry_with_backoff(MAX_ATTEMPTS, BACKOFF_BASE_MS, "test op", &mut state, op);
        assert_send(&fut);
        assert_eq!(fut.await.unwrap(), 2);
    }

    /// Pins the trait surface itself: `RetryOp::attempt` is declared as
    /// `fn ... -> impl Future<Output = ...>`, so generic code can call the
    /// attempt directly (exactly as [`retry_with_backoff`] does), the returned
    /// future keeps borrowing the caller's `state`, and it is still `Send` for
    /// `Send` state.
    #[tokio::test]
    async fn attempt_returns_a_state_borrowing_send_future() {
        fn assert_send<T: Send>(_: &T) {}

        // Generic consumer, mirroring the shape of `retry_with_backoff`.
        async fn run_once<O, S>(op: &mut O, state: &mut S) -> Result<O::Output, O::Error>
        where
            O: RetryOp<S>,
        {
            op.attempt(state).await
        }

        let (mut op, calls) = TestOp::new(0, true);
        let mut state = 41usize;
        let fut = run_once(&mut op, &mut state);
        assert_send(&fut);
        assert_eq!(fut.await.unwrap(), 42, "the attempt saw the borrowed state");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(state, 42, "the borrow wrote through to the caller's state");
    }

    /// The policy as seen through the new surface: a transient failure is
    /// retried and succeeds within the budget, and a persistent one makes the
    /// policy give up after exactly `MAX_ATTEMPTS` attempts.
    #[tokio::test]
    async fn transient_failure_is_recovered_but_persistent_one_gives_up() {
        let (op, calls) = TestOp::new(1, true);
        let mut state = 0usize;
        let got = retry_with_backoff(MAX_ATTEMPTS, 0, "test op", &mut state, op)
            .await
            .expect("one transient failure is inside the budget");
        assert_eq!(got, 2);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);

        let (op, calls) = TestOp::new(MAX_ATTEMPTS as usize + 5, true);
        let mut state = 0usize;
        let err = retry_with_backoff(MAX_ATTEMPTS, 0, "test op", &mut state, op)
            .await
            .expect_err("a persistent failure is not swallowed");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert_eq!(err.label, "fail 3", "the policy gives up after 3 attempts");
    }
}
