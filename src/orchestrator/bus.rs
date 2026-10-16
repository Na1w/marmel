//! Global session event bus, status dispatching, and process-wide cancellation.

static STATUS_SENDER: std::sync::RwLock<Option<tokio::sync::mpsc::UnboundedSender<String>>> =
    std::sync::RwLock::new(None);

static EVENT_SENDER: std::sync::RwLock<
    Option<tokio::sync::mpsc::UnboundedSender<crate::ui::Event>>,
> = std::sync::RwLock::new(None);

/// Shared, thread-safe steering conversation history for the active session.
pub type SharedSteeringHistory = std::sync::Arc<std::sync::RwLock<Vec<(String, String)>>>;

static STEERING_HISTORY: std::sync::RwLock<Option<SharedSteeringHistory>> =
    std::sync::RwLock::new(None);

static GLOBAL_CANCELLATION_TOKEN: std::sync::LazyLock<
    std::sync::RwLock<tokio_util::sync::CancellationToken>,
> = std::sync::LazyLock::new(|| std::sync::RwLock::new(tokio_util::sync::CancellationToken::new()));

/// Get the current session-wide cancellation token.
///
/// Clone-and-drop: the read guard is held only for the `clone()` and is
/// dropped before this function returns; no nested locks are ever taken while
/// a guard is held.
pub fn global_cancellation_token() -> tokio_util::sync::CancellationToken {
    GLOBAL_CANCELLATION_TOKEN
        .read()
        .map(|guard| guard.clone())
        .unwrap_or_default()
}

tokio::task_local! {
    /// Task-local cancellation token for the currently executing specialist worker or validator.
    pub static CURRENT_WORKER_TOKEN: tokio_util::sync::CancellationToken;
}

/// Cancel all active subagents, workers, LLM streams, and operations across the entire process.
pub fn cancel_all() {
    if let Ok(guard) = GLOBAL_CANCELLATION_TOKEN.read() {
        guard.cancel();
    }
    crate::orchestrator::cancel_all_active_workers();
}

/// Returns true if a global cancellation / abort signal has been requested.
pub fn is_globally_cancelled() -> bool {
    GLOBAL_CANCELLATION_TOKEN
        .read()
        .map(|guard| guard.is_cancelled())
        .unwrap_or(false)
}

/// Returns true if either a global cancellation or the current worker's cancellation has been triggered.
pub fn is_current_or_global_cancelled() -> bool {
    if is_globally_cancelled() {
        return true;
    }
    CURRENT_WORKER_TOKEN
        .try_with(|t| t.is_cancelled())
        .unwrap_or(false)
}

/// Reset the global cancellation token for a fresh turn / prompt cycle.
pub fn reset_cancellation() {
    if let Ok(mut guard) = GLOBAL_CANCELLATION_TOKEN.write() {
        *guard = tokio_util::sync::CancellationToken::new();
    }
}

/// Register an unbounded channel to receive real-time status updates across all agents and specialists.
///
/// Intentionally a swap (not a once-only `OnceLock`): the single call site
/// (`run_session` in `src/ui/session.rs`) is reachable multiple times per
/// process lifetime, and the write guard is held only for the swap itself.
pub fn set_status_sender(tx: tokio::sync::mpsc::UnboundedSender<String>) {
    if let Ok(mut lock) = STATUS_SENDER.write() {
        *lock = Some(tx);
    }
}

/// Register an unbounded channel to receive real-time UI events across all agents and specialists.
///
/// Same swap pattern as [`set_status_sender`].
pub fn set_event_sender(tx: tokio::sync::mpsc::UnboundedSender<crate::ui::Event>) {
    if let Ok(mut lock) = EVENT_SENDER.write() {
        *lock = Some(tx);
    }
}

/// Emit a status update to the active UI renderer.
///
/// Contention-free pattern: clone the sender out under the read lock, drop the
/// guard, and only then perform the channel send with NO lock held. The send
/// must never happen while the guard is alive, so emitters never block behind
/// (or hold up) each other or behind the setter's write lock.
pub fn emit_status(msg: impl Into<String>) {
    let tx = STATUS_SENDER.read().ok().and_then(|guard| guard.clone());
    if let Some(tx) = tx {
        let _ = tx.send(msg.into());
    }
}

/// Emit a UI event directly to the active UI renderer.
///
/// Same contention-free pattern as [`emit_status`].
pub fn emit_event(ev: crate::ui::Event) {
    let tx = EVENT_SENDER.read().ok().and_then(|guard| guard.clone());
    if let Some(tx) = tx {
        let _ = tx.send(ev);
    }
}

/// Register the shared steering conversation history for the active session.
///
/// Follows the same swap pattern as `set_status_sender` and `set_event_sender`:
/// reachable multiple times across session restarts.
pub fn set_steering_history(hist: SharedSteeringHistory) {
    if let Ok(mut lock) = STEERING_HISTORY.write() {
        *lock = Some(hist);
    }
}

/// Retrieve the active session's shared steering conversation history, if registered.
pub fn get_steering_history() -> Option<SharedSteeringHistory> {
    STEERING_HISTORY.read().ok().and_then(|guard| guard.clone())
}

/// Reset or clear the active steering conversation history registration.
#[cfg(test)]
pub fn clear_steering_history() {
    if let Ok(mut lock) = STEERING_HISTORY.write() {
        *lock = None;
    }
}

/// Record or update a steering exchange in the active session's steering history.
///
/// An entry is addressed by an **identity key**, never by a substring of one —
/// the same exact-identity discipline as worker routing. Correlation: the most
/// recent pending entry whose recorded arbitrator text names the `notice_id` as
/// a whole identifier token (`entry_names_notice_id`); else the entry whose
/// recorded inquiry equals the query (`same_steering_inquiry`: trim + ASCII
/// case-fold, then whole-string equality); else the exchange is appended.
pub fn record_steering_exchange(
    notice_id: Option<&str>,
    user_inquiry: &str,
    arbitrator_response: &str,
) {
    let Some(hist_lock) = get_steering_history() else {
        return;
    };
    let Ok(mut hist) = hist_lock.write() else {
        return;
    };

    let mut matched = false;

    // 1. Exact notice-id key: the entry has to name this id as a whole token, so
    //    `notice-1` never addresses the entry recorded for `notice-10`.
    if let Some(nid) = notice_id.map(str::trim).filter(|id| !id.is_empty()) {
        for (_user_q, resp_text) in hist.iter_mut().rev() {
            if super::steer::entry_names_notice_id(resp_text, nid)
                && super::steer::steering_entry_is_pending(resp_text)
            {
                *resp_text = arbitrator_response.to_string();
                matched = true;
                break;
            }
        }
    }

    // 2. Exact inquiry key: whole-string equality after trim + case-fold. A
    //    prefix, a suffix or any other partial overlap is a *different* steer.
    if !matched {
        for (user_q, resp_text) in hist.iter_mut().rev() {
            if super::steer::same_steering_inquiry(user_inquiry, user_q)
                && super::steer::steering_entry_is_pending(resp_text)
            {
                *resp_text = arbitrator_response.to_string();
                matched = true;
                break;
            }
        }
    }

    // 3. No recorded entry is addressed by this exchange: keep it as its own
    //    entry instead of overwriting an unrelated pending one.
    if !matched {
        hist.push((user_inquiry.to_string(), arbitrator_response.to_string()));
    }
}
