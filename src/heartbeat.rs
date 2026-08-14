//! Progress pings for slow openQA calls (port of `_with_heartbeat`).
//!
//! `ProgressSink` decouples the ticking logic from `rmcp::Peer`, whose
//! constructor is crate-private and can't be instantiated in unit tests;
//! implementing it for `Peer<RoleServer>` wires the real thing in `server.rs`.

use std::future::Future;
use std::time::Duration;

use rmcp::RoleServer;
use rmcp::model::{ProgressNotificationParam, ProgressToken};
use rmcp::service::Peer;

use crate::config::{InvalidDuration, parse_duration_secs};

const DEFAULT_INTERVAL: Duration = Duration::from_secs(15);

/// Interval between heartbeat pings; `Ok(None)` means disabled. Read on each
/// call (not cached) so tests can tweak it via env.
///
/// # Errors
///
/// Returns [`InvalidDuration`] if `OPENQA_MCP_HEARTBEAT_INTERVAL` is set to
/// an unparseable, non-finite, or out-of-range value.
pub fn interval() -> std::result::Result<Option<Duration>, InvalidDuration> {
    parse_duration_secs(
        "OPENQA_MCP_HEARTBEAT_INTERVAL",
        std::env::var("OPENQA_MCP_HEARTBEAT_INTERVAL")
            .ok()
            .as_deref(),
        DEFAULT_INTERVAL,
    )
}

pub trait ProgressSink: Send + Sync {
    /// Send one progress ping; failures are swallowed by the implementation,
    /// mirroring Python's `except Exception: pass`.
    fn notify(&self, token: ProgressToken, progress: f64) -> impl Future<Output = ()> + Send;
}

impl ProgressSink for Peer<RoleServer> {
    async fn notify(&self, token: ProgressToken, progress: f64) {
        let params = ProgressNotificationParam::new(token, progress).with_message("working…");
        let _ = self.notify_progress(params).await;
    }
}

/// Run `fut` while emitting periodic progress pings via `sink`, so an MCP
/// client waiting on a slow openQA call sees liveness instead of timing out.
/// No-op (just awaits `fut`) without a progress token or when the interval
/// is `<= 0`.
pub async fn with_heartbeat<F: Future, S: ProgressSink>(
    sink: &S,
    token: Option<ProgressToken>,
    fut: F,
) -> F::Output {
    let Some(token) = token else {
        return fut.await;
    };
    // `Err` is unreachable in production: startup validates the same
    // variable via `interval()` and aborts before any call reaches here.
    // Treat both "disabled" and "invalid" as "just await `fut`".
    let Ok(Some(interval)) = interval() else {
        return fut.await;
    };

    tokio::select! {
        output = fut => output,
        () = tick_forever(sink, token, interval) => unreachable!("tick_forever never returns"),
    }
}

async fn tick_forever<S: ProgressSink>(sink: &S, token: ProgressToken, interval: Duration) {
    let mut progress = 0.0;
    loop {
        tokio::time::sleep(interval).await;
        progress += 1.0;
        sink.notify(token.clone(), progress).await;
    }
}

#[cfg(test)]
#[allow(unsafe_code)] // edition 2024 requires unsafe for std::env::set_var
mod tests {
    use std::sync::Mutex;

    use rmcp::model::NumberOrString;
    use tokio::time::sleep;

    use super::*;

    #[derive(Default)]
    struct RecordingSink {
        pings: Mutex<Vec<f64>>,
    }

    impl ProgressSink for RecordingSink {
        async fn notify(&self, _token: ProgressToken, progress: f64) {
            self.pings.lock().unwrap().push(progress);
        }
    }

    fn token() -> ProgressToken {
        ProgressToken(NumberOrString::Number(1))
    }

    // Both env-dependent cases live in one #[tokio::test] fn: OPENQA_MCP_HEARTBEAT_INTERVAL
    // is process-global, and cargo runs tests in parallel threads within one binary.
    #[tokio::test(start_paused = true)]
    async fn heartbeat_interval_enables_and_disables_ticking() {
        // SAFETY: no other test in this binary mutates OPENQA_MCP_HEARTBEAT_INTERVAL.
        unsafe { std::env::set_var("OPENQA_MCP_HEARTBEAT_INTERVAL", "0.01") };
        let sink = RecordingSink::default();
        with_heartbeat(&sink, Some(token()), sleep(Duration::from_millis(50))).await;
        assert!(sink.pings.lock().unwrap().len() >= 3);

        unsafe { std::env::set_var("OPENQA_MCP_HEARTBEAT_INTERVAL", "0") };
        let sink = RecordingSink::default();
        let output = with_heartbeat(&sink, Some(token()), async { 42 }).await;
        assert_eq!(output, 42);
        assert!(sink.pings.lock().unwrap().is_empty());

        // Non-finite/unparseable/out-of-range values must not panic: they
        // fall back to "no heartbeat", the same as `0`.
        for raw in ["nan", "inf", "1e30", "abc"] {
            unsafe { std::env::set_var("OPENQA_MCP_HEARTBEAT_INTERVAL", raw) };
            let sink = RecordingSink::default();
            let output = with_heartbeat(&sink, Some(token()), async { 7 }).await;
            assert_eq!(output, 7);
            assert!(sink.pings.lock().unwrap().is_empty());
        }

        unsafe { std::env::remove_var("OPENQA_MCP_HEARTBEAT_INTERVAL") };
    }

    #[test]
    fn interval_rejects_invalid_values() {
        for raw in ["nan", "inf", "-inf", "1e30", "abc"] {
            let err =
                parse_duration_secs("OPENQA_MCP_HEARTBEAT_INTERVAL", Some(raw), DEFAULT_INTERVAL)
                    .unwrap_err();
            assert!(err.to_string().contains("OPENQA_MCP_HEARTBEAT_INTERVAL"));
        }
    }

    #[tokio::test]
    async fn no_token_emits_no_pings() {
        let sink = RecordingSink::default();

        let output = with_heartbeat(&sink, None, async { "done" }).await;

        assert_eq!(output, "done");
        assert!(sink.pings.lock().unwrap().is_empty());
    }
}
