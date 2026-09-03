//! Global session event bus, status dispatching, and process-wide cancellation.

static STATUS_SENDER: std::sync::RwLock<Option<tokio::sync::mpsc::UnboundedSender<String>>> =
    std::sync::RwLock::new(None);

static EVENT_SENDER: std::sync::RwLock<
    Option<tokio::sync::mpsc::UnboundedSender<crate::ui::Event>>,
> = std::sync::RwLock::new(None);

static GLOBAL_CANCELLATION_TOKEN: std::sync::LazyLock<
    std::sync::RwLock<tokio_util::sync::CancellationToken>,
> = std::sync::LazyLock::new(|| std::sync::RwLock::new(tokio_util::sync::CancellationToken::new()));

/// Get the current session-wide cancellation token.
pub fn global_cancellation_token() -> tokio_util::sync::CancellationToken {
    GLOBAL_CANCELLATION_TOKEN
        .read()
        .map(|guard| guard.clone())
        .unwrap_or_default()
}

/// Cancel all active subagents, workers, LLM streams, and operations across the entire process.
pub fn cancel_all() {
    if let Ok(guard) = GLOBAL_CANCELLATION_TOKEN.read() {
        guard.cancel();
    }
}

/// Returns true if a global cancellation / abort signal has been requested.
pub fn is_globally_cancelled() -> bool {
    GLOBAL_CANCELLATION_TOKEN
        .read()
        .map(|guard| guard.is_cancelled())
        .unwrap_or(false)
}

/// Reset the global cancellation token for a fresh turn / prompt cycle.
pub fn reset_cancellation() {
    if let Ok(mut guard) = GLOBAL_CANCELLATION_TOKEN.write() {
        *guard = tokio_util::sync::CancellationToken::new();
    }
}

/// Register an unbounded channel to receive real-time status updates across all agents and specialists.
pub fn set_status_sender(tx: tokio::sync::mpsc::UnboundedSender<String>) {
    if let Ok(mut lock) = STATUS_SENDER.write() {
        *lock = Some(tx);
    }
}

/// Register an unbounded channel to receive real-time UI events across all agents and specialists.
pub fn set_event_sender(tx: tokio::sync::mpsc::UnboundedSender<crate::ui::Event>) {
    if let Ok(mut lock) = EVENT_SENDER.write() {
        *lock = Some(tx);
    }
}

/// Emit a status update to the active UI renderer.
pub fn emit_status(msg: impl Into<String>) {
    if let Ok(lock) = STATUS_SENDER.read()
        && let Some(tx) = lock.as_ref()
    {
        let _ = tx.send(msg.into());
    }
}

/// Emit a UI event directly to the active UI renderer.
pub fn emit_event(ev: crate::ui::Event) {
    if let Ok(lock) = EVENT_SENDER.read()
        && let Some(tx) = lock.as_ref()
    {
        let _ = tx.send(ev);
    }
}
