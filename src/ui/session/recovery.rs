//! Deep-Freeze checkpoint recovery: spawn `recover_frozen` and drain status/events while it runs.

use super::helpers::is_abort_command;
use crate::orchestrator::OrchestratorManager;
use crate::ui::{Event, Renderer};
use std::sync::Arc;
use std::time::Duration;

/// Drive the Deep-Freeze recovery to completion, returning the recovered
/// `(task_info, deliverable)` if the journal was frozen and recovery succeeded.
pub(crate) async fn recover_frozen_deliverable(
    manager: Option<Arc<OrchestratorManager>>,
    renderer: &mut dyn Renderer,
    status_rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
) -> Option<(String, String)> {
    let mut recovered_deliverable: Option<(String, String)> = None;
    if let Some(mgr) = manager.as_ref()
        && mgr.journal.is_frozen()
    {
        renderer.on_event(&Event::Status(
            "Deep-Freeze checkpoint detected: recovering interrupted task...".to_string(),
        ));
        let _ = renderer.flush();

        let recover_mgr = mgr.clone();
        let mut recover_handle = tokio::spawn(async move { recover_mgr.recover_frozen().await });

        let deliverable_opt = loop {
            let mut had_events = false;
            while let Ok(msg) = status_rx.try_recv() {
                renderer.on_event(&Event::Status(msg));
                had_events = true;
            }
            while let Ok(ev) = event_rx.try_recv() {
                renderer.on_event(&ev);
                had_events = true;
            }
            if had_events {
                let _ = renderer.flush();
            }
            if let Some(input) = renderer.poll_input()
                && is_abort_command(&input)
            {
                crate::orchestrator::cancel_all();
                renderer.on_event(&Event::Status("Recovery aborted by user".to_string()));
                let _ = renderer.flush();
                break None;
            }
            if renderer.aborted() {
                crate::orchestrator::cancel_all();
                break None;
            }
            match tokio::time::timeout(Duration::from_millis(50), &mut recover_handle).await {
                Ok(Ok(Ok(Some(d)))) => break Some(d),
                Ok(Ok(Ok(None))) => break None,
                Ok(Ok(Err(e))) => {
                    tracing::warn!("Recovery returned error: {e}");
                    renderer.on_event(&Event::Status(format!("Recovery failed: {e}")));
                    let _ = renderer.flush();
                    break None;
                }
                Ok(Err(join_err)) => {
                    tracing::warn!("Recovery worker panicked: {join_err}");
                    renderer.on_event(&Event::Status(format!(
                        "Recovery worker panicked: {join_err}"
                    )));
                    let _ = renderer.flush();
                    break None;
                }
                Err(_) => {}
            }
        };

        if let Some(deliverable) = deliverable_opt {
            let task_info = deliverable.task_id.as_deref().unwrap_or("recovered");
            recovered_deliverable = Some((task_info.to_string(), deliverable.content));
        }
    }
    recovered_deliverable
}
