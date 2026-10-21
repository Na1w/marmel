//! Reqwest SSE chat client with retry and timeout watchdogs.

use crate::orchestrator::pool;
use crate::types::{ChatChunk, ChatRequest};
use anyhow::Result;
use eventsource_stream::Eventsource;
// Only for the trait methods (`source()`) needed to walk a reqwest build
// failure's cause chain; kept anonymous so it cannot shadow the `Error` derive
// macro imported from `thiserror` below.
use std::error::Error as _;
use std::time::Duration;
use thiserror::Error;

/// First SSE event must arrive within this window or the request fails (5 minutes for long prefill).
pub const INITIAL_RESPONSE_WATCHDOG_SECS: u64 = 300;
/// Maximum silent pause allowed between stream chunks once streaming has started (5 minutes for long prefill or generation pause).
pub const INTER_CHUNK_WATCHDOG_SECS: u64 = 300;
/// Upper bound on the entire streaming read (20 minutes safety watchdog for up to 32k tokens).
pub const OVERALL_READ_TIMEOUT_SECS: u64 = 1200;
// Retry policy has one owner, `crate::net::retry` (see `src/net/retry.rs`):
// this client consumes from it the attempt count, the backoff base and the
// retryability classification (`ChatError::is_retryable`).

static GLOBAL_TOKENS_IN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static GLOBAL_TOKENS_OUT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn record_tokens_in(count: usize) {
    GLOBAL_TOKENS_IN.fetch_add(count as u64, std::sync::atomic::Ordering::Relaxed);
}

pub fn record_tokens_out(count: usize) {
    GLOBAL_TOKENS_OUT.fetch_add(count as u64, std::sync::atomic::Ordering::Relaxed);
}

pub fn get_global_token_counts() -> (usize, usize) {
    (
        GLOBAL_TOKENS_IN.load(std::sync::atomic::Ordering::Relaxed) as usize,
        GLOBAL_TOKENS_OUT.load(std::sync::atomic::Ordering::Relaxed) as usize,
    )
}

fn count_reply_tokens(
    content: &str,
    reasoning: &str,
    tool_calls: &[crate::types::ToolCall],
) -> usize {
    crate::manager::context::count_assistant_tokens(
        if content.is_empty() {
            None
        } else {
            Some(content)
        },
        if reasoning.is_empty() {
            None
        } else {
            Some(reasoning)
        },
        tool_calls,
    )
}

/// Why one streamed reply reached its terminal state.
///
/// This is the single terminal-state vocabulary for one LLM reply. It is carried
/// on [`StreamedReply::outcome`] and is never inferred from the reply being
/// empty: several providers legitimately answer with empty content plus a
/// terminal `finish_reason`, which is [`ReplyOutcome::Complete`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyOutcome {
    /// The stream reached a terminal marker: the SSE `[DONE]` sentinel or a
    /// terminal `finish_reason` (`stop`, `tool_calls`, `end_turn`, …).
    /// The only state that may be consumed as a model answer.
    Complete,
    /// The **caller** stopped the stream: its `on_delta` hook returned false.
    /// That hook is where the `CancellationToken`, the preemption path,
    /// `cancel_all()`, a sink abort and the local stream guards
    /// (repetition detector, token budgets) all surface. Partial output is
    /// preserved, the reply is not an answer.
    Cancelled,
    /// The stream ended **without** a terminal marker: a clean SSE EOF with no
    /// `[DONE]` and no `finish_reason`, a dropped connection, a watchdog cut, or
    /// a transport failure that arrived after deltas had already been delivered
    /// (which must never be re-sent — see the mid-stream guard in
    /// [`ChatClient::chat_stream`]). Partial output is preserved, the reply is
    /// not an answer.
    Truncated,
    /// The attempt failed at the HTTP/transport/protocol level **before**
    /// anything was delivered. This state travels on the error channel (the
    /// `Err` arm of every public entry point); see
    /// [`ChatError::terminal_outcome`].
    Error,
}

impl ReplyOutcome {
    /// Whether this terminal state may be consumed as a model answer.
    /// `Cancelled`, `Truncated` and `Error` are all `false` here.
    #[must_use]
    pub fn is_success(self) -> bool {
        matches!(self, ReplyOutcome::Complete)
    }

    /// The complement of [`ReplyOutcome::is_success`]: the turn was interrupted,
    /// so any content it carries is partial and must not be written to the
    /// conversation as if the model had answered.
    #[must_use]
    pub fn is_interrupted(self) -> bool {
        !self.is_success()
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ReplyOutcome::Complete => "complete",
            ReplyOutcome::Cancelled => "cancelled",
            ReplyOutcome::Truncated => "truncated",
            ReplyOutcome::Error => "error",
        }
    }
}

/// Which cause put a reply into its [`ReplyOutcome`]; the two are never collapsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalCause {
    /// The SSE `[DONE]` sentinel terminated the stream.
    Done,
    /// The provider sent a terminal `finish_reason` and the body ended cleanly.
    FinishReason,
    /// The caller's `on_delta` hook returned false (cancellation token,
    /// preemption, `cancel_all()`, sink abort, or a local stream guard).
    CallerAbort,
    /// The SSE stream ended without `[DONE]` and without a `finish_reason`.
    StreamEnd,
    /// A connection/transport or SSE-decode failure ended the stream.
    Transport,
    /// A watchdog (first-event, inter-chunk stall or overall read) cut the stream.
    Watchdog,
    /// The backend answered with a non-success HTTP status.
    HttpStatus,
    /// No concurrency slot became free within the queue bound, so the request
    /// was never sent. Distinct from [`TerminalCause::Watchdog`]: this is the
    /// per-acquisition **queue** bound
    /// ([`crate::orchestrator::pool::QUEUE_WAIT_TIMEOUT_SECS`]), armed before any
    /// stream watchdog exists, so a queued call can never be mistaken for a cut
    /// stream.
    QueueTimeout,
}

impl TerminalCause {
    /// The terminal state this cause maps to. Caller-initiated stops are
    /// `Cancelled`; anything the provider or the transport left unfinished is
    /// `Truncated`; a failure that delivered nothing is `Error`.
    #[must_use]
    pub const fn outcome(self) -> ReplyOutcome {
        match self {
            TerminalCause::Done | TerminalCause::FinishReason => ReplyOutcome::Complete,
            TerminalCause::CallerAbort => ReplyOutcome::Cancelled,
            TerminalCause::StreamEnd | TerminalCause::Transport | TerminalCause::Watchdog => {
                ReplyOutcome::Truncated
            }
            // Nothing was ever sent to the backend, so neither of these can be a
            // truncation: they are failures on the error channel.
            TerminalCause::HttpStatus | TerminalCause::QueueTimeout => ReplyOutcome::Error,
        }
    }
}

/// The terminal signal handed to [`ReplyAccumulator::finalize`]: the state, the
/// cause that produced it, the provider's `finish_reason` when one was seen, and
/// whether the "LLM reply completed" info lines are emitted (only the full-read
/// path emits them).
#[derive(Debug, Clone)]
pub(crate) struct Terminal {
    pub(crate) outcome: ReplyOutcome,
    pub(crate) cause: TerminalCause,
    pub(crate) log_summary: bool,
    pub(crate) finish_reason: Option<String>,
}

impl Terminal {
    pub(crate) const fn from_cause(cause: TerminalCause, log_summary: bool) -> Self {
        Self {
            outcome: cause.outcome(),
            cause,
            log_summary,
            finish_reason: None,
        }
    }
}

/// A single fully-assembled assistant reply chunk sequence.
#[derive(Debug, Clone)]
pub struct StreamedReply {
    pub content: String,
    pub reasoning: String,
    pub raw: String,
    pub tool_calls: Vec<crate::types::ToolCall>,
    /// Why the stream ended. Only [`ReplyOutcome::Complete`] may be treated as
    /// an answer; a default-constructed reply is `Truncated`, never `Complete`.
    pub outcome: ReplyOutcome,
    /// Which cause produced [`StreamedReply::outcome`](ReplyOutcome).
    pub cause: TerminalCause,
    /// The provider's terminal `finish_reason` when the stream reported one
    /// (`stop`, `length`, `tool_calls`, …); `None` when it never got that far.
    /// This is what separates a genuine empty completion from a cut stream.
    pub finish_reason: Option<String>,
    /// Deltas already handed to the caller's `on_delta` hook before termination.
    /// Once this is non-zero the same request must never be re-sent.
    pub deltas: usize,
    /// Bytes of accumulated assistant payload (content + reasoning + tool-call
    /// arguments) carried by this reply.
    pub bytes: usize,
    /// Tool-call fragments the provider started but never completed. They are
    /// counted here instead of being handed over as executable tool calls.
    pub dropped_tool_calls: usize,
}

impl Default for StreamedReply {
    /// An empty reply established no terminal marker, so it is `Truncated` by
    /// construction: an undecided/empty reply can never pass a success check.
    fn default() -> Self {
        Self {
            content: String::new(),
            reasoning: String::new(),
            raw: String::new(),
            tool_calls: Vec::new(),
            outcome: ReplyOutcome::Truncated,
            cause: TerminalCause::StreamEnd,
            finish_reason: None,
            deltas: 0,
            bytes: 0,
            dropped_tool_calls: 0,
        }
    }
}

impl StreamedReply {
    /// Whether this reply may be consumed as a model answer. `false` for
    /// `Cancelled`, `Truncated` and `Error` terminals.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.outcome.is_success()
    }
}

/// Streaming chat client bound to a backend.
#[derive(Debug, Clone)]
pub struct ChatClient {
    backend_url: String,
    auth_token: String,
    model: String,
    initial_timeout_secs: u64,
    stall_timeout_secs: u64,
    /// Per-acquisition bound on the wait for a concurrency slot
    /// ([`crate::orchestrator::pool`]). Only consulted when a cap is configured
    /// for this `backend + model` pair; it is armed and discharged entirely
    /// before any stream watchdog is, so it never eats into
    /// [`INITIAL_RESPONSE_WATCHDOG_SECS`] or [`OVERALL_READ_TIMEOUT_SECS`].
    queue_wait: Duration,
    /// Bound on the wait for an **internal-reserve** slot, used only by reserve
    /// call classes ([`crate::orchestrator::pool::CallClass::SteerArbitrator`]
    /// and [`crate::orchestrator::pool::CallClass::NoticeEval`]).
    ///
    /// Defaults to [`crate::orchestrator::pool::internal_queue_wait`]
    /// ([`crate::orchestrator::pool::INTERNAL_QUEUE_WAIT_SECS`], 20 s) and is the
    /// fail-open clock of design §5 / invariant I7: when it expires the call
    /// proceeds **un-metered** with a warning instead of queueing, so a cap can
    /// never make steering unresponsive or let an internal fast call deadlock
    /// against its own reserve slot.
    reserve_wait: Duration,
    /// The shared HTTP client, or the verdict of why it could not be built.
    ///
    /// Construction never panics and never degrades silently: on a host with no
    /// TLS trust store `reqwest::Client::build()` returns `Err`, and every
    /// fallback that builds a client of its own (`Client::new()`,
    /// `unwrap_or_default()`) re-runs that same failing build and aborts the
    /// process. So the failure is **stored** here and surfaced on the error
    /// channel at request time, which keeps every `ChatClient` constructor
    /// infallible.
    http: Result<reqwest::Client, HttpClientError>,
}

/// Why marmel could not build its HTTP client at all.
///
/// Kept crate-internal: `ChatClient`'s public surface stays unchanged. The
/// reason is captured at construction time because `reqwest::Error`'s
/// `Display` for a builder failure is literally just `"builder error"` — the
/// actionable text (e.g. "No CA certificates were loaded from the system")
/// only survives in the source chain, see [`build_failure_reason`].
#[derive(Debug, Clone, Error)]
#[error(
    "no TLS trust store: could not build marmel's HTTP client ({reason}); install your \
     distribution's ca-certificates package, or point SSL_CERT_FILE (optionally \
     SSL_CERT_DIR) at an existing PEM CA bundle such as \
     /etc/ssl/certs/ca-certificates.crt — with no CA bundle loaded no backend is reachable \
     at all, https or http"
)]
pub(crate) struct HttpClientError {
    reason: String,
}

/// The HTTP client this process uses, built at most once.
///
/// Memoized so a store-less host pays for the failing probe a single time
/// rather than once per [`ChatClient`] — there are well over a hundred
/// constructor calls across the tree, and the same reason is reported to all
/// of them.
static HTTP_CLIENT: std::sync::OnceLock<Result<reqwest::Client, HttpClientError>> =
    std::sync::OnceLock::new();

/// Shared HTTP client builder: this client builds no client of its own, and it
/// never falls back to one that would panic.
fn shared_http_client() -> Result<reqwest::Client, HttpClientError> {
    HTTP_CLIENT
        .get_or_init(|| {
            crate::net::build_http_client(Some(Duration::from_secs(10)), None).map_err(|err| {
                HttpClientError {
                    reason: build_failure_reason(&err),
                }
            })
        })
        .clone()
}

/// Extract the real cause behind a reqwest client-build failure.
///
/// `err.to_string()` alone is not enough: for a builder error it says only
/// `"builder error"`. The whole source chain is therefore walked and joined,
/// with the `Debug` rendering as fallback when there is no chain to walk.
fn build_failure_reason(err: &reqwest::Error) -> String {
    let mut parts = vec![err.to_string()];
    let mut source = err.source();
    while let Some(cause) = source {
        parts.push(cause.to_string());
        source = cause.source();
    }
    if parts.len() == 1 {
        parts.push(format!("{err:?}"));
    }
    parts.join(": ")
}

/// Startup probe for the binary: `Some(actionable message)` when this process
/// could not build its HTTP client at all (no TLS trust store in practice),
/// `None` when it can talk to a backend.
///
/// Reads the same memoized verdict every [`ChatClient`] stores, so the probe both
/// answers the question and pays for the single build: the request path reuses
/// the outcome rather than probing again, and each turn then fails with the same
/// text through [`ChatError::TrustStore`]. Deliberately **not** fatal here — the
/// run keeps its normal exit code, because the failure is reported per turn.
pub fn http_client_startup_error() -> Option<String> {
    shared_http_client().err().map(|err| err.to_string())
}

/// The queue bound a client starts with: the pool module owns the number, this
/// client only consumes it (mirrors how the retry policy is consumed from
/// `crate::net::retry`).
fn default_queue_wait() -> Duration {
    pool::queue_wait_timeout()
}

/// The internal-reserve bound a client starts with, again owned by the pool
/// module: 20 s, after which a reserve-class call fails open (design §5, I7).
fn default_reserve_wait() -> Duration {
    pool::internal_queue_wait()
}

#[derive(Debug, Error)]
pub(crate) enum ChatError {
    #[error("backend returned HTTP {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("first event did not arrive within {INITIAL_RESPONSE_WATCHDOG_SECS}s")]
    InitialTimeout,
    #[error("stream stalled: no tokens received for {INTER_CHUNK_WATCHDOG_SECS}s")]
    StallTimeout,
    #[error("stream exceeded {OVERALL_READ_TIMEOUT_SECS}s read timeout")]
    ReadTimeout,
    #[error("SSE stream error: {0}")]
    Stream(String),
    #[error("transport error: {0}")]
    Transport(String),
    /// A failure that arrived **after** deltas had already been delivered to the
    /// caller. Re-sending the same request is forbidden (it would re-bill the
    /// prompt and duplicate output the caller has already rendered), so the
    /// attempt terminates here with the partial accumulation attached; see
    /// [`StreamProgress::forbid_mid_stream_retry`].
    #[error("stream interrupted after {deltas} deltas / {bytes} bytes: {cause}")]
    Interrupted {
        deltas: usize,
        bytes: usize,
        cause: Box<ChatError>,
        reply: Box<StreamedReply>,
    },
    /// No concurrency slot for this `backend + model` pair became free within the
    /// queue bound, so this call was **never sent**. Emitted by the pool gate in
    /// [`ChatClient::chat_stream`], not by an attempt: the request never reached
    /// the transport, so no delta was delivered and nothing is truncated.
    ///
    /// Names the cap and the key it belongs to, per the queue-timeout contract —
    /// a failed delegation must be attributable to the configured limit.
    #[error(
        "no backend slot for {backend} + {model}: queue wait expired after {waited_secs}s \
         (cap {cap} session(s) per backend+model)"
    )]
    QueueTimeout {
        /// Whole seconds spent waiting for a slot.
        waited_secs: u64,
        /// The configured concurrency cap that produced the queue.
        cap: usize,
        /// Normalized backend URL half of the pool key.
        backend: String,
        /// Model half of the pool key.
        model: String,
    },
    /// This process has **no HTTP client at all**: the shared `reqwest` client
    /// could not be built, which on a host with no TLS trust store means no CA
    /// bundles were found. Raised by [`ChatClient::chat_stream`] before the pool
    /// gate and before any attempt, so nothing was sent, no slot was occupied and
    /// no retry attempt was spent.
    ///
    /// Unlike [`ChatError::Transport`] this is never transient — the verdict was
    /// taken once, at construction — and `reason` carries the full actionable
    /// text (the missing CA bundles plus how to point marmel at them through
    /// `ca-certificates`, `SSL_CERT_FILE` or `SSL_CERT_DIR`).
    #[error("{reason} (backend {backend})")]
    TrustStore {
        /// Backend this client was bound to, so the failure is attributable.
        backend: String,
        /// Verdict captured at construction, remediation included.
        reason: String,
    },
}

impl crate::net::Retryable for ChatError {
    /// Retryable classes: transient HTTP statuses (503/429/502/504), watchdog
    /// timeouts, transport failures and SSE stream errors — all only while
    /// nothing has reached the caller yet. Three classes are terminal:
    /// [`ChatError::Interrupted`] (deltas were already delivered, so a new
    /// attempt of the same request would duplicate output),
    /// [`ChatError::QueueTimeout`] (the queue bound already expired — retrying
    /// here would re-enter the queue, double-spend the wait budget and let this
    /// call leapfrog the sessions already in line) and
    /// [`ChatError::TrustStore`] (no HTTP client exists in this process: the
    /// verdict was taken once at construction, so re-attempting can only fail in
    /// exactly the same way).
    fn is_retryable(&self) -> bool {
        match self {
            ChatError::Interrupted { .. }
            | ChatError::QueueTimeout { .. }
            | ChatError::TrustStore { .. } => false,
            ChatError::HttpStatus { status, .. } => matches!(status, 503 | 429 | 502 | 504),
            ChatError::InitialTimeout
            | ChatError::StallTimeout
            | ChatError::Transport(_)
            | ChatError::Stream(_)
            | ChatError::ReadTimeout => true,
        }
    }
}

impl ChatError {
    /// The terminal state this failure leaves the turn in: `Truncated` when the
    /// caller had already received deltas (the partial reply is attached to the
    /// variant), otherwise `Error` — nothing was delivered, and the error
    /// channel is what carries the failure.
    pub(crate) fn terminal_outcome(&self) -> ReplyOutcome {
        match self {
            ChatError::Interrupted { .. } => ReplyOutcome::Truncated,
            // Includes `QueueTimeout` and `TrustStore`: the request was never
            // sent, so there is no partially delivered stream to truncate.
            _ => ReplyOutcome::Error,
        }
    }

    /// The cause class this failure belongs to, for the terminal bookkeeping.
    pub(crate) fn terminal_cause(&self) -> TerminalCause {
        match self {
            ChatError::HttpStatus { .. } => TerminalCause::HttpStatus,
            ChatError::InitialTimeout | ChatError::StallTimeout | ChatError::ReadTimeout => {
                TerminalCause::Watchdog
            }
            // The queue bound is not a stream watchdog: no stream existed.
            ChatError::QueueTimeout { .. } => TerminalCause::QueueTimeout,
            // A missing trust store is a transport failure in the broad sense:
            // no client exists to speak the protocol at all, so it belongs to
            // the same cause class as `Transport` rather than a new one.
            ChatError::Stream(_)
            | ChatError::Transport(_)
            | ChatError::TrustStore { .. }
            | ChatError::Interrupted { .. } => TerminalCause::Transport,
        }
    }
}

/// Per-call stream progress: the caller's `on_delta` hook plus the counter that
/// makes the mid-stream retry ban decidable.
///
/// This is the mutable state [`crate::net::retry_with_backoff`] threads through
/// every attempt of [`ChatAttempt`]. The counter is per *call*, not per attempt,
/// so the single retry owner is told — through [`ChatError::Interrupted`], the
/// one `ChatError` class whose [`crate::net::Retryable`] classification says
/// "never retry" — that a request which has already delivered deltas is never re-sent.
#[derive(Debug)]
pub(crate) struct StreamProgress<F> {
    on_delta: F,
    /// Deltas already handed to the caller across every attempt of this call.
    deltas: usize,
}

impl<F: FnMut(&str) -> bool> StreamProgress<F> {
    pub(crate) fn new(on_delta: F) -> Self {
        Self {
            on_delta,
            deltas: 0,
        }
    }

    /// The caller's delivery/abort hook, as the shared SSE pump wants it.
    fn on_delta_mut(&mut self) -> &mut F {
        &mut self.on_delta
    }

    /// Record one delta delivered to the caller.
    fn count_delta(&mut self) {
        self.deltas += 1;
    }

    /// How many deltas this call has delivered across every attempt.
    fn deltas(&self) -> usize {
        self.deltas
    }

    /// Has this call already delivered anything? If so, a retry of the same
    /// request is forbidden.
    fn has_emitted_deltas(&self) -> bool {
        self.deltas > 0
    }

    /// Turn a failure that landed after deltas were delivered into the terminal
    /// `Truncated` state, carrying the partial accumulation. The warn names the
    /// delta count and the byte count — the re-billing this prevents.
    fn forbid_mid_stream_retry(&self, cause: ChatError, partial: StreamedReply) -> ChatError {
        tracing::warn!(
            "LLM stream interrupted after {} deltas / {} bytes: {cause}; a mid-stream retry of the same request is forbidden, terminating as {}",
            self.deltas,
            partial.bytes,
            ReplyOutcome::Truncated.as_str(),
        );
        ChatError::Interrupted {
            deltas: self.deltas,
            bytes: partial.bytes,
            cause: Box::new(cause),
            reply: Box::new(partial),
        }
    }
}

/// One LLM chat attempt driven through the shared retry loop in
/// [`crate::net::retry`]: each attempt borrows the caller's [`StreamProgress`]
/// (its `on_delta` callback plus the delivered-delta counter) as the retry
/// helper's mutable state.
struct ChatAttempt<'a> {
    client: &'a ChatClient,
    req: &'a ChatRequest,
}

impl<F> crate::net::RetryOp<StreamProgress<F>> for ChatAttempt<'_>
where
    F: FnMut(&str) -> bool,
{
    type Error = ChatError;
    type Output = StreamedReply;

    async fn attempt(
        &mut self,
        progress: &mut StreamProgress<F>,
    ) -> Result<StreamedReply, ChatError> {
        self.client.try_chat_once(self.req, progress).await
    }
}

/// What the concurrency gate holds for the duration of one `chat_stream` call.
///
/// `Drop` is the only release path (design invariant I2): the slot goes back to
/// its pool on completion, hard error, cancellation, queue timeout, watchdog cut
/// **or panic**, because the guard lives on this call's stack frame — never
/// inside a retry attempt and never in a thread parked in `block_on`.
#[derive(Debug)]
enum SlotGuard {
    /// A slot drawn from the per-key cap semaphore.
    Session(pool::SessionPermit),
    /// The per-key internal reserve slot (steer arbitrator / notice evaluation).
    Reserve(pool::ReservePermit),
    /// Nothing to hold: this pair has no configured cap, or a reserve-class call
    /// failed open. The uncapped case never touches a semaphore.
    Unmetered,
}

/// Why the concurrency gate could not hand out a guard.
#[derive(Debug)]
enum SlotAbort {
    /// The queue bound expired without a slot: terminal and non-retryable.
    Timeout(ChatError),
    /// The enclosing worker or the whole process was cancelled while waiting.
    /// Not a failure — the caller gets a `Cancelled` reply.
    Cancelled,
    /// The pool was closed under the waiter. Defensive: nothing closes a pool
    /// today, and surfacing it beats wedging the call forever.
    Closed(String),
}

/// Mirror one pool queue-position report onto the **worker registry** entry this call
/// belongs to — the durable half of the queue state (fix #3,
/// `docs/diagnose_subpane_stale_refresh.md` §3).
///
/// `position: Some(..)` is a rank report, `None` is the verdict "this call is not
/// waiting for a slot any more". Both land on the registry because the registry is
/// what the subagent pane rebuilds its rows from: the `Queued` / `Running` delegation
/// events are one-shot and can be consumed by a drain that has no row for them yet,
/// while the pool reports a rank once per acquisition and afterwards only when the
/// rank actually moves. With the rank on the entry, a later projection re-derives the
/// panel's `(Queued n/m)` label instead of losing it for the rest of the wait.
///
/// `owner: None` — an unclassified call, or a call class whose delegation the panel
/// tracks no row for — is a no-op: there is no entry to put a rank on, and inventing
/// one is exactly the phantom-row hazard the event path's identity rule exists to
/// avoid (see [`crate::orchestrator::delegation_identity`]).
fn publish_worker_queue_position(
    owner: &Option<(String, Option<String>)>,
    position: Option<pool::QueuePosition>,
) {
    let Some((agent, task)) = owner else {
        return;
    };
    crate::orchestrator::workers::set_worker_queue_position(agent, task.as_deref(), position);
}

impl SlotGuard {
    /// Report the queued → running transition this guard represents.
    ///
    /// Status traffic on queue transitions is what keeps a session that spent
    /// time in the queue from falsely tripping the idle watchdogs (design §6).
    /// An uncapped call — and a call that never had to queue — says nothing.
    fn announce_acquisition(&self) {
        match self {
            SlotGuard::Session(permit) => {
                let waited = permit.waited();
                if waited.is_zero() {
                    return;
                }
                // The cap is named alongside the wait so an operator can tie the
                // stall to the configured `[orchestration.concurrency]` value, and
                // the call context says which delegation was stuck in line.
                tracing::info!(
                    backend = %permit.backend(),
                    model = %permit.model(),
                    cap = permit.limit(),
                    waited_secs = waited.as_secs(),
                    free_permits = permit.available_permits(),
                    call = %pool::current_call_context()
                        .map(|context| context.describe())
                        .unwrap_or_else(|| "unclassified".to_string()),
                    "backend slot acquired after queueing"
                );
                crate::orchestrator::bus::emit_status(format!(
                    "backend slot acquired after {}s ({} of {} free)",
                    waited.as_secs(),
                    permit.available_permits(),
                    permit.limit()
                ));
            }
            SlotGuard::Reserve(permit) => {
                // The reserve is the slot that keeps steering responsive while the
                // cap is saturated, so holding it is worth a line.
                tracing::debug!(
                    backend = %permit.key().0,
                    model = %permit.key().1,
                    reserve_free = permit.reserve_available(),
                    "internal reserve slot held for this backend call"
                );
            }
            SlotGuard::Unmetered => {}
        }
    }
}

impl ChatClient {
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Self {
            backend_url: cfg.backend_url.clone(),
            auth_token: cfg.auth_token.clone(),
            model: cfg.model.clone(),
            initial_timeout_secs: INITIAL_RESPONSE_WATCHDOG_SECS,
            stall_timeout_secs: INTER_CHUNK_WATCHDOG_SECS,
            queue_wait: default_queue_wait(),
            reserve_wait: default_reserve_wait(),
            http: shared_http_client(),
        }
    }

    pub fn new(backend_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            backend_url: backend_url.into(),
            auth_token: String::new(),
            model: model.into(),
            initial_timeout_secs: INITIAL_RESPONSE_WATCHDOG_SECS,
            stall_timeout_secs: INTER_CHUNK_WATCHDOG_SECS,
            queue_wait: default_queue_wait(),
            reserve_wait: default_reserve_wait(),
            http: shared_http_client(),
        }
    }

    pub fn new_with_token(
        backend_url: impl Into<String>,
        model: impl Into<String>,
        auth_token: impl Into<String>,
    ) -> Self {
        Self {
            backend_url: backend_url.into(),
            auth_token: auth_token.into(),
            model: model.into(),
            initial_timeout_secs: INITIAL_RESPONSE_WATCHDOG_SECS,
            stall_timeout_secs: INTER_CHUNK_WATCHDOG_SECS,
            queue_wait: default_queue_wait(),
            reserve_wait: default_reserve_wait(),
            http: shared_http_client(),
        }
    }

    /// Override the stored HTTP client with a caller-built one.
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.http = Ok(client);
        self
    }

    pub fn with_initial_timeout_secs(mut self, secs: u64) -> Self {
        self.initial_timeout_secs = secs;
        self
    }

    pub fn with_stall_timeout_secs(mut self, secs: u64) -> Self {
        self.stall_timeout_secs = secs;
        self
    }

    /// Override the per-acquisition bound on waiting for a concurrency slot.
    ///
    /// Only meaningful when a cap is configured for this `backend + model` pair
    /// (`[orchestration.concurrency]`); an uncapped call never waits. The default
    /// is [`crate::orchestrator::pool::QUEUE_WAIT_TIMEOUT_SECS`].
    pub fn with_queue_wait_secs(mut self, secs: u64) -> Self {
        self.queue_wait = Duration::from_secs(secs);
        self
    }

    /// Override the bound on waiting for an **internal-reserve** slot, the slot
    /// steer-arbitrator and notice-evaluation calls draw (design §5).
    ///
    /// Only ever a shortening of the pool's bound: the value is clamped to
    /// [`crate::orchestrator::pool::INTERNAL_QUEUE_WAIT_SECS`] (20 s), because
    /// the point of the reserve is that an internal fast call gives up and runs
    /// un-metered rather than waiting on it — a longer wait would reintroduce
    /// the steering stall the reserve exists to prevent (invariant I7).
    pub fn with_reserve_wait_secs(mut self, secs: u64) -> Self {
        self.reserve_wait = Duration::from_secs(secs).min(pool::internal_queue_wait());
        self
    }

    /// The internal-reserve wait this client will use. Kept observable so the
    /// 20 s fail-open bound of design §5 is assertable at the client, not only
    /// in the pool.
    pub fn reserve_wait(&self) -> Duration {
        self.reserve_wait
    }

    pub fn backend_url(&self) -> &str {
        &self.backend_url
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub async fn chat(&self, req: &ChatRequest) -> Result<StreamedReply> {
        self.chat_stream(req, |_| true).await
    }

    /// Stream one chat completion through the shared retry policy.
    ///
    /// Terminal states, and the only honest success check on this path:
    /// * [`ReplyOutcome::Complete`] — `[DONE]` or a terminal `finish_reason`;
    /// * [`ReplyOutcome::Cancelled`] — the caller's `on_delta` hook returned
    ///   false (cancellation token, preemption, `cancel_all()`, sink abort), or
    ///   the call was cancelled while it was still waiting for a slot;
    /// * [`ReplyOutcome::Truncated`] — the stream ended without a terminal
    ///   marker, or failed after deltas had already been delivered;
    /// * [`ReplyOutcome::Error`] — the attempt failed before anything was
    ///   delivered, or no concurrency slot became free within the queue bound,
    ///   which is the `Err` arm of this call.
    ///
    /// A reply is only ever returned as `Ok` when a request was actually
    /// answered or cut short; the caller must check
    /// [`StreamedReply::is_success`] before treating it as an answer.
    ///
    /// Concurrency metering (design §1.3/§1.4): the slot for this `backend +
    /// model` pair is taken **before** the retry loop and held across every
    /// attempt of this call, so a retry can never leapfrog the sessions already
    /// queued for that pair. When no cap is configured, no semaphore is touched
    /// at all and this path behaves exactly as it always did.
    pub async fn chat_stream<F>(&self, req: &ChatRequest, on_delta: F) -> Result<StreamedReply>
    where
        F: FnMut(&str) -> bool,
    {
        // No HTTP client exists in this process (the shared build failed, on a
        // host with no TLS trust store in practice). Resolved **before the pool
        // gate**: a store-less run must not occupy a concurrency slot of another
        // session, and must not spend any of the retry policy's attempts — the
        // verdict never changes at runtime, so an attempt could only fail the
        // same way. Terminal, non-retryable, and never a panic.
        if let Err(err) = &self.http {
            let error = self.trust_store_error(err);
            tracing::error!(
                "LLM turn terminated as {}: {error}",
                error.terminal_outcome().as_str()
            );
            crate::debug_log::log_llm_error(&self.backend_url, &self.model, 0, &error.to_string());
            return Err(anyhow::anyhow!("{error}"));
        }
        // The slot guard is bound here, not inside a retry attempt: it stays
        // alive until this function returns, and `Drop` is the only release path
        // (success, hard error, cancellation, queue timeout, panic).
        let queued_at = std::time::Instant::now();
        let _slot = match self.acquire_slot().await {
            Ok(guard) => guard,
            Err(SlotAbort::Timeout(error)) => {
                // Terminal by classification (`ChatError::QueueTimeout` is the
                // one failure the retry owner is told about and never retries —
                // and the request was never sent in the first place).
                tracing::debug!(
                    "LLM turn terminated as {}: {error}",
                    error.terminal_outcome().as_str()
                );
                crate::debug_log::log_llm_error(
                    &self.backend_url,
                    &self.model,
                    queued_at.elapsed().as_millis(),
                    &error.to_string(),
                );
                return Err(anyhow::anyhow!("{error}"));
            }
            Err(SlotAbort::Cancelled) => {
                return Ok(self.cancelled_while_queued(queued_at.elapsed()));
            }
            Err(SlotAbort::Closed(key)) => {
                return Err(anyhow::anyhow!(
                    "concurrency pool for {key} was closed; this backend call cannot run"
                ));
            }
        };
        _slot.announce_acquisition();

        let mut progress = StreamProgress::new(on_delta);
        // Shared retry/backoff policy (`crate::net::retry`): the helper hands
        // the mutable progress state (the `on_delta` callback plus the
        // delivered-delta counter) to the operation on every attempt. The policy
        // itself decides not to retry once a failure is reported as
        // `ChatError::Interrupted`, so no second retry path lives here.
        let attempt = crate::net::retry_with_backoff(
            crate::net::MAX_ATTEMPTS,
            crate::net::BACKOFF_BASE_MS,
            "LLM backend call",
            &mut progress,
            ChatAttempt { client: self, req },
        )
        .await;

        match attempt {
            Ok(reply) => Ok(reply),
            // Deltas had already reached the caller: the attempt terminated as
            // `Truncated` with its partial accumulation instead of being re-sent.
            Err(ChatError::Interrupted { reply, .. }) => Ok(*reply),
            Err(e) => {
                tracing::debug!(
                    "LLM turn terminated as {}: {e}",
                    e.terminal_outcome().as_str()
                );
                Err(anyhow::anyhow!("{e}"))
            }
        }
    }

    /// Take the concurrency slot for one [`chat_stream`] call and return the
    /// guard that holds it, before any attempt is made.
    ///
    /// `Ok(Unmetered)` is the "no pool" answer — no cap is configured for this
    /// pair, so nothing is queued, no semaphore is touched, and behavior is
    /// byte-for-byte what it was before this feature.
    ///
    /// The wait is cancellation-aware: the pool selects on the enclosing worker's
    /// token (from [`crate::orchestrator::pool::current_worker_token`]), the
    /// process-wide token, and the queue deadline. A cancelled or timed-out wait
    /// never takes a permit, and dropping this future mid-wait leaves the slot
    /// for the callers in line.
    async fn acquire_slot(&self) -> Result<SlotGuard, SlotAbort> {
        let token = pool::current_worker_token();
        let context = pool::current_call_context();
        // Attribution for queue traffic comes from the installed call context:
        // a queued line must name the delegation it belongs to, not just the
        // backend key. `None` means an unclassified call (pre-feature behaviour)
        // and adds nothing to the text.
        let attribution = context.as_ref().map(|context| context.describe());
        // The same attribution in typed form, for the delegation event stream.
        // Only a call that belongs to a tracked delegation resolves (see
        // [`crate::orchestrator::delegation_identity`]), so a queue event can
        // never invent a panel row that no `Started`/`Completed` pair owns.
        let delegation = context
            .as_ref()
            .and_then(crate::orchestrator::delegation_identity);
        // Queue transitions go out as status traffic so a session waiting on a
        // slot does not falsely trip the idle watchdogs (design §6). No-op when
        // no UI renderer is listening.
        let queued_delegation = delegation.clone();
        // The **durable** half of the same transition (fix #3,
        // `docs/diagnose_subpane_stale_refresh.md` §3): the position is mirrored onto
        // the worker registry entry, which is the structure the subagent pane rebuilds
        // its rows from. The delegation event is one-shot — the session drains it with
        // `events.drain(..)`, and a `Queued` that reaches the drain before the row
        // projection has opened the row is gone for the whole slot wait — so a viewer
        // that missed it must be able to re-read the rank from the registry instead.
        // Keyed on the same identity the event pair is emitted for (canonical agent
        // label + plan task), never on a derived spelling, so the two halves of one
        // transition can only ever address the same entry.
        let queue_owner = delegation
            .as_ref()
            .map(|(agent, task)| (agent.as_str().to_string(), task.clone()));
        let reported_owner = queue_owner.clone();
        let reporter = move |position: pool::QueuePosition| {
            let attribution = attribution
                .as_deref()
                .map(|who| format!(" — {who}"))
                .unwrap_or_default();
            crate::orchestrator::bus::emit_status(format!(
                "queued for a backend slot ({} of {}){attribution}",
                position.position, position.of
            ));
            // The panel-facing half of the same transition: position goes over
            // as structured state, never as a free-form status label (design §7).
            if let Some((agent, task)) = &queued_delegation {
                crate::orchestrator::push_delegation_event(
                    crate::orchestrator::DelegationEvent::Queued {
                        agent: *agent,
                        task: task.clone(),
                        position: position.position,
                        of: position.of,
                    },
                );
            }
            // The registry-facing half: the same rank, kept where a later projection
            // can re-derive it.
            publish_worker_queue_position(&reported_owner, Some(position));
        };

        // Steer-arbitrator and notice-evaluation calls take the per-key reserve
        // and fail open rather than queue behind specialist work (design §5);
        // every other class — including a call with no installed context — is
        // metered against the cap semaphore.
        if pool::current_call_class().is_some_and(|class| class.uses_reserve()) {
            return match pool::acquire_reserve_with_wait(
                &self.backend_url,
                &self.model,
                token,
                self.reserve_wait,
            )
            .await
            {
                pool::ReserveOutcome::Reserved(permit) => Ok(SlotGuard::Reserve(permit)),
                pool::ReserveOutcome::Unmetered => Ok(SlotGuard::Unmetered),
                pool::ReserveOutcome::FailedOpen { waited_secs } => {
                    tracing::warn!(
                        backend = %self.backend_url,
                        model = %self.model,
                        call_class = %pool::current_call_class()
                            .map_or("unclassified", |class| class.label()),
                        waited_secs,
                        reserve_wait_secs = self.reserve_wait.as_secs(),
                        "reserve slot stayed busy; running this backend call un-metered"
                    );
                    Ok(SlotGuard::Unmetered)
                }
                pool::ReserveOutcome::Cancelled => Err(SlotAbort::Cancelled),
            };
        }

        let acquired = pool::acquire_session_with_wait(
            &self.backend_url,
            &self.model,
            token,
            self.queue_wait,
            Some(&reporter),
        )
        .await;
        // Durable half of the queued → running edge, taken back down here rather than
        // only through the `Running` event below: the moment this wait returns, this
        // call holds no rank in a FIFO any more — whether it got the permit, was
        // cancelled, timed out or found the pool closed. The event half can be dropped
        // by a panel drain that has no row for it yet (which is why the event carries
        // no `Running` for a wait that never reached this arm), so the registry alone
        // has to say "no longer waiting" for the panel to stop reading a stale rank at
        // its next projection.
        publish_worker_queue_position(&queue_owner, None);
        match acquired {
            Ok(None) => Ok(SlotGuard::Unmetered),
            Ok(Some(permit)) => {
                // The queued → running edge, paired with the `Queued` the
                // reporter emitted for this very acquisition: a call that never
                // had to queue has no queue state to take back down, and a call
                // whose wait ended in cancellation or timeout never reaches this
                // arm (its lifecycle closes with `Failed`, which clears the queue
                // state as well).
                if permit.queue_position().is_some()
                    && let Some((agent, task)) = &delegation
                {
                    crate::orchestrator::push_delegation_event(
                        crate::orchestrator::DelegationEvent::Running {
                            agent: *agent,
                            task: task.clone(),
                        },
                    );
                }
                Ok(SlotGuard::Session(permit))
            }
            Err(pool::PoolError::QueueTimeout {
                backend,
                model,
                cap,
                waited_secs,
                ..
            }) => Err(SlotAbort::Timeout(ChatError::QueueTimeout {
                waited_secs,
                cap,
                backend,
                model,
            })),
            Err(pool::PoolError::Cancelled { .. }) => Err(SlotAbort::Cancelled),
            Err(pool::PoolError::PoolClosed { key }) => Err(SlotAbort::Closed(key)),
        }
    }

    /// A cancellation requested while this call was still queued for a slot: an
    /// empty `Cancelled` reply, never an empty success and never a hard error —
    /// nothing was sent, and the caller is the one who asked for the stop.
    fn cancelled_while_queued(&self, waited: Duration) -> StreamedReply {
        tracing::warn!(
            "LLM request cancelled while waiting for a backend slot (waited {}s) — reporting {}, not an empty completion",
            waited.as_secs(),
            ReplyOutcome::Cancelled.as_str(),
        );
        crate::debug_log::log_llm_error(
            &self.backend_url,
            &self.model,
            waited.as_millis(),
            "cancelled while waiting for a backend slot",
        );
        StreamedReply {
            outcome: ReplyOutcome::Cancelled,
            cause: TerminalCause::CallerAbort,
            ..StreamedReply::default()
        }
    }

    /// The error reported when this client has **no** HTTP client: the verdict
    /// [`shared_http_client`] captured at construction, attributed to the backend
    /// this client is bound to. `reason` keeps the whole actionable text — the
    /// build failure chain plus the remediation (CA bundles, `ca-certificates`,
    /// `SSL_CERT_FILE`, `SSL_CERT_DIR`) — because `reqwest`'s own `Display` for a
    /// builder failure says nothing useful on its own.
    fn trust_store_error(&self, err: &HttpClientError) -> ChatError {
        ChatError::TrustStore {
            backend: self.backend_url.clone(),
            reason: err.to_string(),
        }
    }

    async fn try_chat_once<F>(
        &self,
        req: &ChatRequest,
        progress: &mut StreamProgress<F>,
    ) -> Result<StreamedReply, ChatError>
    where
        F: FnMut(&str) -> bool,
    {
        // Borrow the stored client. [`ChatClient::chat_stream`] already refuses a
        // client-less process with the non-retryable [`ChatError::TrustStore`]
        // **before** the pool gate and before the retry loop, so this arm only
        // exists to keep the borrow total: an attempt is never reached without a
        // client, and if one ever is, it is reported as the same terminal class
        // instead of being mistaken for a transient transport failure.
        let client = self
            .http
            .as_ref()
            .map_err(|err| self.trust_store_error(err))?;

        let url = format!(
            "{}/chat/completions",
            self.backend_url.trim_end_matches('/')
        );

        let mut req_body = req.clone();
        req_body.stream = Some(true);
        if req_body.model.is_empty() {
            req_body.model = self.model.clone();
        }
        // Pre-send normalization of the cloned payload (never the caller's history):
        // strict OpenAI-compatible chat formats accept `role: "system"` only at index
        // 0 and reject later ones with HTTP 400 ("System message must be at the
        // beginning."). Mid-history system messages come from compaction checkpoints
        // (`ContextEngine::perform_rebirth`) and from session bundles restored from
        // older builds: demote them to `user` on the wire with byte-identical content,
        // keeping the real roles in memory so the checkpoint detectors still match.
        // Runs before token counting, debug dumps and request logging.
        for (index, msg) in req_body.messages.iter_mut().enumerate() {
            if index > 0 && matches!(msg, crate::types::Message::System { .. }) {
                // `Message` has no `Default`, so swap the element out before reading it.
                let demoted = std::mem::replace(
                    msg,
                    crate::types::Message::User {
                        content: String::new(),
                    },
                );
                *msg = crate::types::Message::User {
                    content: demoted.content().unwrap_or_default().to_string(),
                };
                continue;
            }
            if let crate::types::Message::Assistant {
                content,
                tool_calls,
                ..
            } = msg
            {
                if content.is_none() {
                    *content = Some(String::new());
                }
                for tc in tool_calls {
                    tc.sanitize_arguments();
                }
            }
        }

        tracing::info!(
            "Calling LLM backend at {} (model: {}, messages: {})",
            url,
            req_body.model,
            req_body.messages.len()
        );
        let prompt_tokens = crate::manager::context::count_tokens(&req_body.messages);
        record_tokens_in(prompt_tokens);
        tracing::debug!(
            "LLM request body: {}",
            serde_json::to_string(&req_body).unwrap_or_default()
        );
        crate::debug_log::log_llm_request(&url, &req_body.model, &req_body);
        let req_start = std::time::Instant::now();

        let mut builder = client.post(&url).json(&req_body);
        if !self.auth_token.is_empty() {
            builder = builder.bearer_auth(&self.auth_token);
        }

        let first_start = std::time::Instant::now();
        let send_deadline = first_start + Duration::from_secs(self.initial_timeout_secs);
        let send_fut = builder.send();
        tokio::pin!(send_fut);
        #[allow(clippy::never_loop)]
        let resp = loop {
            // Shared SSE pump skeleton (50 ms poll + abort + watchdog deadline).
            match crate::net::pump_future(
                &mut send_fut,
                Some(send_deadline),
                progress.on_delta_mut(),
            )
            .await
            {
                crate::net::PumpNext::Item(Some(Ok(r))) => break r,
                crate::net::PumpNext::Item(Some(Err(e))) => {
                    let elapsed = req_start.elapsed().as_millis();
                    crate::debug_log::log_llm_error(&url, &req_body.model, elapsed, &e.to_string());
                    return Err(ChatError::Transport(e.to_string()));
                }
                crate::net::PumpNext::Item(None) => unreachable!("send future never yields None"),
                crate::net::PumpNext::IdleTimeout => {
                    let elapsed = req_start.elapsed().as_millis();
                    crate::debug_log::log_llm_error(
                        &url,
                        &req_body.model,
                        elapsed,
                        "initial timeout waiting for first response",
                    );
                    return Err(ChatError::InitialTimeout);
                }
                // The caller stopped the request before a single byte of the
                // answer arrived: a cancellation, never an empty success.
                crate::net::PumpNext::Aborted => {
                    return Ok(cancelled_before_stream(
                        &url,
                        &req_body.model,
                        req_start,
                        progress,
                    ));
                }
            }
        };

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            let elapsed = req_start.elapsed().as_millis();
            tracing::error!("LLM backend returned HTTP {status}: {body}");
            crate::debug_log::log_llm_error(
                &url,
                &req_body.model,
                elapsed,
                &format!("HTTP {status}: {body}"),
            );
            return Err(ChatError::HttpStatus { status, body });
        }

        let mut stream = resp.bytes_stream().eventsource();

        // Everything the SSE decoder accumulates for this read, in one owner.
        let mut state = ReadState::default();
        // How the read terminated. `StreamEnd` means "the body simply stopped"
        // which, absent any terminal marker, is a truncation.
        let mut terminal = Terminal::from_cause(TerminalCause::StreamEnd, true);

        let first =
            match crate::net::pump_next(&mut stream, Some(send_deadline), progress.on_delta_mut())
                .await
            {
                crate::net::PumpNext::Item(res) => res,
                crate::net::PumpNext::IdleTimeout => return Err(ChatError::InitialTimeout),
                crate::net::PumpNext::Aborted => {
                    return Ok(cancelled_before_stream(
                        &url,
                        &req_body.model,
                        req_start,
                        progress,
                    ));
                }
            };

        if let Some(ev) = first {
            let ev = ev.map_err(|e| ChatError::Stream(e.to_string()))?;
            match consume_event(&ev, &mut state, progress)? {
                ConsumeMark::Done => terminal = Terminal::from_cause(TerminalCause::Done, false),
                ConsumeMark::Aborted => {
                    terminal = Terminal::from_cause(TerminalCause::CallerAbort, false);
                }
                ConsumeMark::Continue => {}
            }
            if terminal.cause != TerminalCause::StreamEnd {
                if state.in_reasoning {
                    // The read is over here, so only the closing marker matters.
                    let _ = progress.on_delta_mut()("</think>");
                }
                terminal.finish_reason = state.finish_reason.take();
                return Ok(
                    // The very first SSE event already carried the terminal
                    // marker: this site emits no completion-summary lines.
                    state.into_accumulator().finalize(
                        &url,
                        &req_body.model,
                        req_start,
                        terminal,
                        progress,
                    ),
                );
            }
        } else {
            // Empty stream: the first poll yielded `None` — no `[DONE]`, no
            // `finish_reason`. A cut answer, not an empty success.
            return Ok(state.into_accumulator().finalize(
                &url,
                &req_body.model,
                req_start,
                Terminal::from_cause(TerminalCause::StreamEnd, false),
                progress,
            ));
        }

        let consume = async {
            let mut last_chunk_at = std::time::Instant::now();
            let mut last_progress_log = std::time::Instant::now();
            let mut last_logged_chars = 0usize;
            loop {
                // Inter-chunk watchdog: deadline is recomputed per chunk so the
                // stall limit applies to the silent gap since the last event.
                let stall_deadline = last_chunk_at + Duration::from_secs(self.stall_timeout_secs);
                match crate::net::pump_next(
                    &mut stream,
                    Some(stall_deadline),
                    progress.on_delta_mut(),
                )
                .await
                {
                    crate::net::PumpNext::Item(Some(ev)) => {
                        last_chunk_at = std::time::Instant::now();
                        let ev = ev.map_err(|e| ChatError::Stream(e.to_string()))?;
                        match consume_event(&ev, &mut state, progress)? {
                            ConsumeMark::Done => {
                                terminal = Terminal::from_cause(TerminalCause::Done, true);
                                break;
                            }
                            ConsumeMark::Aborted => {
                                // The caller refused further deltas: its own
                                // cancellation, never a completed answer.
                                terminal = Terminal::from_cause(TerminalCause::CallerAbort, true);
                                break;
                            }
                            ConsumeMark::Continue => {}
                        }
                        let total_chars = state.content.len() + state.reasoning.len();
                        if total_chars > 0
                            && (last_progress_log.elapsed() >= Duration::from_secs(5)
                                || total_chars.saturating_sub(last_logged_chars) >= 4000)
                        {
                            let elapsed_s = req_start.elapsed().as_secs();
                            let approx_toks = (total_chars / 4).max(1);
                            tracing::info!(
                                "LLM stream progress ({}, elapsed {}s): ~{} tokens ({} reasoning chars, {} content chars)",
                                req_body.model,
                                elapsed_s,
                                approx_toks,
                                state.reasoning.len(),
                                state.content.len(),
                            );
                            crate::debug_log::log_llm_progress(
                                &url,
                                &req_body.model,
                                req_start.elapsed().as_millis(),
                                state.reasoning.len(),
                                state.content.len(),
                                approx_toks,
                            );
                            last_progress_log = std::time::Instant::now();
                            last_logged_chars = total_chars;
                        }
                    }
                    // The body simply stopped: no `[DONE]`, no terminal
                    // `finish_reason`. A cut answer, not a completed one.
                    crate::net::PumpNext::Item(None) => break,
                    crate::net::PumpNext::IdleTimeout => {
                        terminal = Terminal::from_cause(TerminalCause::Watchdog, true);
                        return Err(ChatError::StallTimeout);
                    }
                    crate::net::PumpNext::Aborted => {
                        terminal = Terminal::from_cause(TerminalCause::CallerAbort, true);
                        break;
                    }
                }
            }
            if state.in_reasoning {
                state.in_reasoning = false;
                let _ = progress.on_delta_mut()("</think>");
            }
            Ok::<(), ChatError>(())
        };
        let read = tokio::time::timeout(Duration::from_secs(OVERALL_READ_TIMEOUT_SECS), consume)
            .await
            .map_err(|_| ChatError::ReadTimeout)
            .and_then(|r| r);

        // Classify why the read stopped before finalizing. `StreamEnd` is the
        // default "the body just stopped" state; a real failure or a provider
        // `finish_reason` overrides it so causes are never collapsed.
        if let Err(cause) = &read {
            if terminal.cause == TerminalCause::StreamEnd {
                terminal = Terminal::from_cause(cause.terminal_cause(), true);
            }
        } else if terminal.cause == TerminalCause::StreamEnd && state.finish_reason.is_some() {
            terminal = Terminal::from_cause(TerminalCause::FinishReason, true);
        }
        terminal.finish_reason = state.finish_reason.take();

        let reply =
            state
                .into_accumulator()
                .finalize(&url, &req_body.model, req_start, terminal, progress);

        match read {
            Ok(()) => Ok(reply),
            // Deltas already reached the caller: the same request must not be
            // attempted again, so the failure terminates as `Truncated` with
            // the partial accumulation attached.
            Err(cause) if progress.has_emitted_deltas() => {
                Err(progress.forbid_mid_stream_retry(cause, reply))
            }
            Err(cause) => Err(cause),
        }
    }
}

/// What one SSE read has decoded so far: the payload, the open/closed state of
/// the reasoning channel, and the provider's own terminal `finish_reason` (the
/// marker that separates a genuine — even empty — completion from a cut stream).
#[derive(Debug, Default)]
struct ReadState {
    content: String,
    reasoning: String,
    raw: String,
    tool_calls: std::collections::BTreeMap<usize, (Option<String>, String, String)>,
    in_reasoning: bool,
    finish_reason: Option<String>,
}

impl ReadState {
    /// Hand the payload over to the single finalizer. The reasoning-channel flag
    /// and the `finish_reason` are terminal bookkeeping, not reply payload, so
    /// they stay behind.
    fn into_accumulator(self) -> ReplyAccumulator {
        ReplyAccumulator {
            content: self.content,
            reasoning: self.reasoning,
            raw: self.raw,
            tool_calls: self.tool_calls,
        }
    }
}

/// The deltas accumulated for one in-flight streamed reply, ready to be finalized.
///
/// Single owner of the terminal bookkeeping: every successful exit site of
/// `ChatClient::try_chat_once` routes the accumulation through
/// [`ReplyAccumulator::finalize`], so the terminal state, the tool-call
/// filtering, the token counters and the debug-log entry exist in one place.
#[derive(Debug, Default)]
pub(crate) struct ReplyAccumulator {
    pub(crate) content: String,
    pub(crate) reasoning: String,
    pub(crate) raw: String,
    pub(crate) tool_calls: std::collections::BTreeMap<usize, (Option<String>, String, String)>,
}

impl ReplyAccumulator {
    /// Assemble the terminal [`StreamedReply`]: record why the stream ended, map
    /// the index-keyed tool-call accumulator into ordered tool calls (discarding
    /// fragments the provider never completed), count output tokens, add them to
    /// the global token counters and write the debug-log response entry.
    ///
    /// `terminal` carries the outcome, its cause and the provider's
    /// `finish_reason` when one was seen. Its `log_summary` flag keeps the one
    /// behavioural difference between the call sites explicit: only the fully-read
    /// path emits the "LLM reply completed" / per-tool-call info lines. Every
    /// interrupted terminal logs a `warn!` instead — it must never look like a
    /// completed answer.
    pub(crate) fn finalize(
        self,
        url: &str,
        model: &str,
        req_start: std::time::Instant,
        terminal: Terminal,
        progress: &StreamProgress<impl FnMut(&str) -> bool>,
    ) -> StreamedReply {
        let args_bytes: usize = self
            .tool_calls
            .values()
            .map(|(_, _, args)| args.len())
            .sum();
        let bytes = self.raw.len() + args_bytes;
        let (tool_calls, dropped_tool_calls) = map_to_tool_calls(self.tool_calls, terminal.outcome);
        if terminal.outcome.is_success() {
            if terminal.log_summary {
                tracing::info!(
                    "LLM reply completed: {} content chars, {} reasoning chars, {} tool calls",
                    self.content.len(),
                    self.reasoning.len(),
                    tool_calls.len()
                );
                for tc in &tool_calls {
                    tracing::info!(
                        "Tool call parsed: {} (id: {}) args: {}",
                        tc.function.name,
                        tc.id,
                        tc.function.arguments
                    );
                }
            }
        } else {
            tracing::warn!(
                "LLM reply {} (cause {:?}): {} content chars, {} reasoning chars, {} deltas already delivered, {} unfinished tool-call fragment(s) dropped, finish_reason={} — partial output, not an answer",
                terminal.outcome.as_str(),
                terminal.cause,
                self.content.len(),
                self.reasoning.len(),
                progress.deltas(),
                dropped_tool_calls,
                terminal.finish_reason.as_deref().unwrap_or("<none>"),
            );
        }

        let reply = StreamedReply {
            content: self.content,
            reasoning: self.reasoning,
            raw: self.raw,
            tool_calls,
            outcome: terminal.outcome,
            cause: terminal.cause,
            finish_reason: terminal.finish_reason,
            deltas: progress.deltas(),
            bytes,
            dropped_tool_calls,
        };
        let out_toks = count_reply_tokens(&reply.content, &reply.reasoning, &reply.tool_calls);
        record_tokens_out(out_toks);
        let elapsed = req_start.elapsed().as_millis();
        crate::debug_log::log_llm_response(url, model, 200, elapsed, &reply);
        reply
    }
}

/// Map the index-keyed accumulator to ordered tool calls and count the fragments
/// that had to be discarded.
///
/// An interrupted reply never yields executable tool calls: a `Cancelled` or
/// `Truncated` stream can carry half-assembled arguments, and handing those to a
/// tool executor is precisely the "cancellation looks like success" failure this
/// module must prevent. Fragments that never got a function name are dropped on
/// every path.
fn map_to_tool_calls(
    map: std::collections::BTreeMap<usize, (Option<String>, String, String)>,
    outcome: ReplyOutcome,
) -> (Vec<crate::types::ToolCall>, usize) {
    let total = map.len();
    let named: Vec<(Option<String>, String, String)> = map
        .into_values()
        .filter(|(_, name, _)| !name.trim().is_empty())
        .collect();
    let mut dropped = total - named.len();
    if !outcome.is_success() {
        dropped += named.len();
        return (Vec::new(), dropped);
    }
    let calls = named
        .into_iter()
        .map(|(id, name, arguments)| {
            let call_id = id.unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4()));
            crate::types::ToolCall::new(call_id, name, arguments)
        })
        .collect();
    (calls, dropped)
}

/// A cancellation requested before a single response byte arrived: an empty
/// `Cancelled` reply, never an empty success.
fn cancelled_before_stream<F>(
    url: &str,
    model: &str,
    req_start: std::time::Instant,
    progress: &StreamProgress<F>,
) -> StreamedReply
where
    F: FnMut(&str) -> bool,
{
    tracing::warn!(
        "LLM request cancelled before the response body started ({} deltas delivered) — reporting {}, not an empty completion",
        progress.deltas(),
        ReplyOutcome::Cancelled.as_str(),
    );
    crate::debug_log::log_llm_error(
        url,
        model,
        req_start.elapsed().as_millis(),
        "cancelled before response",
    );
    StreamedReply {
        outcome: ReplyOutcome::Cancelled,
        cause: TerminalCause::CallerAbort,
        deltas: progress.deltas(),
        ..StreamedReply::default()
    }
}

/// What one SSE event told the read loop about how the stream ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConsumeMark {
    /// The event was consumed; keep reading.
    Continue,
    /// The `[DONE]` sentinel arrived: a terminal marker.
    Done,
    /// The caller's hook refused the delta: a caller-initiated cancellation.
    Aborted,
}

fn consume_event<F>(
    ev: &eventsource_stream::Event,
    state: &mut ReadState,
    progress: &mut StreamProgress<F>,
) -> Result<ConsumeMark, ChatError>
where
    F: FnMut(&str) -> bool,
{
    if ev.data.trim() == "[DONE]" {
        if state.in_reasoning {
            state.in_reasoning = false;
            let _ = progress.on_delta_mut()("</think>");
        }
        return Ok(ConsumeMark::Done);
    }
    if let Ok(chunk) = serde_json::from_str::<ChatChunk>(&ev.data) {
        for choice in chunk.choices {
            // The provider's own terminal marker. The first non-empty one wins,
            // and it is what separates a genuine (even empty) completion from a
            // stream whose body was cut short.
            if state.finish_reason.is_none()
                && let Some(fr) = choice.finish_reason
                && !fr.is_empty()
            {
                state.finish_reason = Some(fr);
            }
            if let Some(r) = choice.delta.reasoning_content
                && !r.is_empty()
            {
                state.reasoning.push_str(&r);
                state.raw.push_str(&r);
                if !state.in_reasoning {
                    state.in_reasoning = true;
                    if !progress.on_delta_mut()("<think>") {
                        return Ok(ConsumeMark::Aborted);
                    }
                }
                progress.count_delta();
                if !progress.on_delta_mut()(&r) {
                    return Ok(ConsumeMark::Aborted);
                }
            }
            if let Some(c) = choice.delta.content
                && !c.is_empty()
            {
                if state.in_reasoning {
                    state.in_reasoning = false;
                    if !progress.on_delta_mut()("</think>") {
                        return Ok(ConsumeMark::Aborted);
                    }
                }
                state.content.push_str(&c);
                state.raw.push_str(&c);
                progress.count_delta();
                if !progress.on_delta_mut()(&c) {
                    return Ok(ConsumeMark::Aborted);
                }
            }
            if let Some(tcs) = choice.delta.tool_calls {
                for tc in tcs {
                    let entry = state
                        .tool_calls
                        .entry(tc.index)
                        .or_insert_with(|| (None, String::new(), String::new()));
                    if let Some(id) = tc.id {
                        entry.0 = Some(id);
                    }
                    if let Some(func) = tc.function {
                        if let Some(name) = func.name {
                            entry.1.push_str(&name);
                        }
                        if let Some(args) = func.arguments {
                            entry.2.push_str(&args);
                        }
                    }
                }
                // Keep-alive poll of the caller's hook: a tool-call-only stream
                // must still observe cancellation.
                progress.count_delta();
                if !progress.on_delta_mut()("") {
                    return Ok(ConsumeMark::Aborted);
                }
            }
        }
    }
    Ok(ConsumeMark::Continue)
}

/// Test-only seam for the stored HTTP-client verdict.
///
/// A missing TLS trust store cannot be simulated through the environment:
/// `openssl-probe` reads `SSL_CERT_FILE`/`SSL_CERT_DIR` **and still** probes the
/// well-known certificate directories, so a normal host always finds a store and
/// the failing branch is unreachable in CI (every runner ships `ca-certificates`).
/// Injecting the verdict is therefore the only way to exercise the
/// `bug.md` regression: it replaces exactly what [`shared_http_client`] handed to
/// a constructor, which is the whole state the request path reads. Nothing in
/// production can reach this — it exists only under `cfg(test)`.
#[cfg(test)]
impl ChatClient {
    /// Force this client to carry the "no HTTP client exists" verdict, with
    /// `reason` standing in for the captured reqwest build-failure chain.
    fn with_http_client_failure(mut self, reason: impl Into<String>) -> Self {
        self.http = Err(HttpClientError {
            reason: reason.into(),
        });
        self
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
