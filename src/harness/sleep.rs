//! Sleep / wait tool handlers.

use super::common::{ToolError, ToolResult};

pub(crate) fn handle_sleep(args: &serde_json::Value) -> Result<ToolResult, ToolError> {
    let secs = args
        .get("seconds")
        .or_else(|| args.get("duration"))
        .or_else(|| args.get("duration_seconds"))
        .and_then(|v| {
            v.as_u64()
                .or_else(|| {
                    v.as_i64()
                        .and_then(|i| if i > 0 { Some(i as u64) } else { None })
                })
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .unwrap_or(5);
    let max_sleep = 300; // Cap at 5 minutes
    let actual_secs = secs.clamp(1, max_sleep);
    let reason = args
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let reason_clause = if reason.is_empty() {
        String::new()
    } else {
        format!(" ({reason})")
    };

    let cancel = crate::orchestrator::bus::global_cancellation_token();
    if cancel.is_cancelled() {
        return Ok(ToolResult::err("Sleep cancelled before starting."));
    }

    let completed = if let Ok(handle) = tokio::runtime::Handle::try_current() {
        match handle.runtime_flavor() {
            tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(|| {
                handle.block_on(async {
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(actual_secs)) => true,
                        _ = cancel.cancelled() => false,
                    }
                })
            }),
            _ => {
                let start = std::time::Instant::now();
                let dur = std::time::Duration::from_secs(actual_secs);
                let mut done = false;
                while start.elapsed() < dur {
                    if cancel.is_cancelled() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                if start.elapsed() >= dur && !cancel.is_cancelled() {
                    done = true;
                }
                done
            }
        }
    } else {
        let start = std::time::Instant::now();
        let dur = std::time::Duration::from_secs(actual_secs);
        let mut done = false;
        while start.elapsed() < dur {
            if cancel.is_cancelled() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if start.elapsed() >= dur && !cancel.is_cancelled() {
            done = true;
        }
        done
    };

    if completed {
        Ok(ToolResult::ok(format!(
            "Slept for {actual_secs} seconds{reason_clause}."
        )))
    } else {
        Ok(ToolResult::err("Sleep interrupted by cancellation signal."))
    }
}

pub async fn handle_sleep_async(arguments: &serde_json::Value) -> Result<ToolResult, ToolError> {
    let secs = arguments
        .get("seconds")
        .or_else(|| arguments.get("duration"))
        .or_else(|| arguments.get("duration_seconds"))
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .unwrap_or(5);

    let actual_secs = secs.clamp(1, 300);
    let reason = arguments
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");

    let reason_clause = if reason.is_empty() {
        String::new()
    } else {
        format!(" ({reason})")
    };

    let cancel = crate::orchestrator::bus::global_cancellation_token();
    if cancel.is_cancelled() || crate::orchestrator::is_current_or_global_cancelled() {
        return Ok(ToolResult::err("Sleep cancelled before starting."));
    }

    let worker_token = crate::orchestrator::CURRENT_WORKER_TOKEN
        .try_with(|t| t.clone())
        .ok();

    let completed = tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_secs(actual_secs)) => true,
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
        Ok(ToolResult::ok(format!(
            "Slept for {actual_secs} seconds{reason_clause}."
        )))
    } else {
        Ok(ToolResult::err("Sleep interrupted by cancellation signal."))
    }
}
