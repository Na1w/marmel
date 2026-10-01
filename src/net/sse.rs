//! Shared SSE pump skeleton: poll a stream (or a one-shot future) every
//! 50 ms, checking an optional abort hook and an optional idle (watchdog)
//! deadline before each poll.
//!
//! Both the LLM chat client (three-tier watchdogs: initial-response,
//! inter-chunk stall, overall read timeout) and the MCP HTTP/SSE transport
//! (flat per-event request timeout) previously hand-rolled this exact
//! "poll with a clock" idiom; this is the single shared implementation.
//! Callers keep their own deadline *policy* (which instant to treat as the
//! watchdog) and only the polling/abort/idle mechanics live here.

use futures_util::StreamExt;
use std::future::Future;
use std::time::{Duration, Instant};

/// Interval between stream polls. Keeps the pump responsive to the abort hook
/// and deadline checks. (Same 50 ms value the LLM client has always used.)
pub const SSE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Outcome of one pump cycle.
pub enum PumpNext<T> {
    /// The stream produced an item (`Some`) or ended (`None`).
    Item(Option<T>),
    /// The idle watchdog deadline elapsed before an item arrived.
    IdleTimeout,
    /// The abort hook signalled stop.
    Aborted,
}

/// Poll `stream` once, every [`SSE_POLL_INTERVAL`], checking `abort` and the
/// optional `deadline` before each poll (abort is checked first, matching the
/// original LLM client ordering).
///
/// `abort` is the same delta-probe callback the LLM client has always used
/// (`on_delta("")`): a falsy return means the caller wants to stop.
pub async fn pump_next<S>(
    stream: &mut S,
    deadline: Option<Instant>,
    abort: &mut impl FnMut(&str) -> bool,
) -> PumpNext<S::Item>
where
    S: StreamExt + Unpin,
{
    loop {
        if !abort("") {
            return PumpNext::Aborted;
        }
        if let Some(d) = deadline
            && Instant::now() >= d
        {
            return PumpNext::IdleTimeout;
        }
        let mut next = stream.next();
        if let Ok(res) = tokio::time::timeout(SSE_POLL_INTERVAL, &mut next).await {
            return PumpNext::Item(res);
        }
    }
}

/// Poll a one-shot future (e.g. `reqwest`'s `send()`) every
/// [`SSE_POLL_INTERVAL`], checking `abort` and the optional `deadline` before
/// each poll. Returns `PumpNext::Item(Some(output))` when the future resolves.
pub async fn pump_future<F>(
    fut: &mut F,
    deadline: Option<Instant>,
    abort: &mut impl FnMut(&str) -> bool,
) -> PumpNext<F::Output>
where
    F: Future + Unpin,
{
    loop {
        if !abort("") {
            return PumpNext::Aborted;
        }
        if let Some(d) = deadline
            && Instant::now() >= d
        {
            return PumpNext::IdleTimeout;
        }
        if let Ok(res) = tokio::time::timeout(SSE_POLL_INTERVAL, &mut *fut).await {
            return PumpNext::Item(Some(res));
        }
    }
}
