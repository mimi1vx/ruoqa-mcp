//! Bounded export pipeline: one queue and one task per signal.
//!
//! The queue carries one pre-encoded `LogRecord` (or, in later phases, `Span`
//! / metric data point) body per message — never a batch and never a whole
//! request. The export task assembles `Resource` and `InstrumentationScope`
//! once per batch via `encode_batch`, which already knows how to splice each
//! queued record in (`proto::write_bytes(buf, 2, &record)` produces exactly
//! the LEN-framed bytes a nested `log_records` field needs, so there is no
//! separate "whole record" encoder to call here).

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue, RETRY_AFTER};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::env::QueueConfig;

/// `tracing` targets excluded from the diagnostics stream: this module and
/// the HTTP stack beneath it. Without this, a failing export would log,
/// which (once the diagnostics `Layer` is wired) would queue a record, which
/// would trigger another export — bugwarden hit exactly this loop with
/// `opentelemetry-appender-tracing`. Every `tracing` call in this module uses
/// `target: "ruoqa_mcp::otel"` so the exclusion holds by construction.
pub(crate) const EXCLUDED_TARGETS: &[&str] = &[
    "ruoqa_mcp::otel",
    "reqwest",
    "hyper",
    "hyper_util",
    "rustls",
    "h2",
    "tower",
];

/// Whether `target` is inside one of [`EXCLUDED_TARGETS`]: an exact match, or
/// a child module (`target::` prefix). `"reqwestish"` merely sharing a
/// prefix with `"reqwest"` must not match.
pub(crate) fn excluded(target: &str) -> bool {
    EXCLUDED_TARGETS
        .iter()
        .any(|excl| target == *excl || target.starts_with(&format!("{excl}::")))
}

const MAX_SEND_ATTEMPTS: u32 = 3;
const RETRY_BACKOFF: Duration = Duration::from_secs(1);

/// Closed-vocabulary reason a record never reached the collector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DropReason {
    QueueFull,
    Network,
    HttpStatus,
    Shutdown,
}

impl DropReason {
    fn as_str(self) -> &'static str {
        match self {
            DropReason::QueueFull => "queue_full",
            DropReason::Network => "network",
            DropReason::HttpStatus => "http_status",
            DropReason::Shutdown => "shutdown",
        }
    }
}

/// Per-reason drop counters. A warning fires when a counter's new total is a
/// power of two (1, 2, 4, 8, …), carrying the count and the reason **and
/// nothing else** — no URL, no error, no header.
#[derive(Default)]
struct Dropped {
    queue_full: AtomicU64,
    network: AtomicU64,
    http_status: AtomicU64,
    shutdown: AtomicU64,
}

impl Dropped {
    fn counter(&self, reason: DropReason) -> &AtomicU64 {
        match reason {
            DropReason::QueueFull => &self.queue_full,
            DropReason::Network => &self.network,
            DropReason::HttpStatus => &self.http_status,
            DropReason::Shutdown => &self.shutdown,
        }
    }

    fn record(&self, reason: DropReason, n: u64) {
        if n == 0 {
            return;
        }
        let total = self.counter(reason).fetch_add(n, Ordering::Relaxed) + n;
        if total.is_power_of_two() {
            tracing::warn!(
                target: "ruoqa_mcp::otel",
                reason = reason.as_str(),
                count = total,
                "OTLP export dropped records"
            );
        }
    }

    #[cfg(test)]
    fn get(&self, reason: DropReason) -> u64 {
        self.counter(reason).load(Ordering::Relaxed)
    }
}

/// Builds a full `ExportXServiceRequest` body from a batch of pre-encoded
/// record bodies. Signal-specific (each of D/E/F supplies its own), which is
/// what keeps this module itself signal-generic.
pub(crate) type EncodeBatch = Arc<dyn Fn(&[Vec<u8>]) -> Vec<u8> + Send + Sync>;

/// The send side of an [`Exporter`]'s queue, cloneable and independent of the
/// exporter's own lifetime. `Exporter::shutdown` consumes `self`, so it
/// cannot live behind an `Arc` shared with a `tracing` `Layer` — this is what
/// the `Layer` holds instead.
///
/// Outstanding clones do **not** keep the export task alive: shutdown is
/// driven by the `Exporter`'s `CancellationToken`, not by every sender
/// dropping, so a leaked `Producer` clone cannot wedge process exit.
#[derive(Clone)]
pub(crate) struct Producer {
    tx: mpsc::Sender<Vec<u8>>,
    dropped: Arc<Dropped>,
}

#[cfg(test)]
impl Producer {
    /// Bypasses `Exporter::start`'s task/HTTP machinery entirely, for a
    /// caller (`audit.rs`'s tests) that only needs to observe what gets
    /// enqueued.
    pub(crate) fn for_test(tx: mpsc::Sender<Vec<u8>>) -> Self {
        Self {
            tx,
            dropped: Arc::new(Dropped::default()),
        }
    }
}

impl Producer {
    /// Enqueues one pre-encoded record. **Never awaits**: callers include the
    /// tool path and the audit sink's lock holder, and a dead collector must
    /// never slow either down. A full queue drops the record and accounts it
    /// under `queue_full`.
    pub(crate) fn enqueue(&self, encoded_record: Vec<u8>) {
        match self.tx.try_send(encoded_record) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped.record(DropReason::QueueFull, 1);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.dropped.record(DropReason::Shutdown, 1);
            }
        }
    }
}

/// One bounded queue and one export task for a single OTLP signal.
pub(crate) struct Exporter {
    tx: mpsc::Sender<Vec<u8>>,
    max_queue_size: usize,
    dropped: Arc<Dropped>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl fmt::Debug for Exporter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Exporter")
            .field("max_queue_size", &self.max_queue_size)
            .finish_non_exhaustive()
    }
}

impl Exporter {
    /// Starts the export task. `client` is built once by the caller (so it
    /// is shared across signals); `endpoint`/`headers`/`timeout` come from
    /// that signal's [`SignalConfig`](super::env::SignalConfig).
    pub(crate) fn start(
        client: reqwest::Client,
        endpoint: Url,
        headers: HeaderMap,
        timeout: Duration,
        queue: QueueConfig,
        encode_batch: EncodeBatch,
    ) -> Self {
        let (tx, rx) = mpsc::channel(queue.max_queue_size);
        let dropped = Arc::new(Dropped::default());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_task(
            rx,
            cancel.clone(),
            client,
            endpoint,
            headers,
            timeout,
            queue.schedule_delay,
            queue.max_export_batch_size,
            encode_batch,
            Arc::clone(&dropped),
        ));
        Self {
            tx,
            max_queue_size: queue.max_queue_size,
            dropped,
            cancel,
            task,
        }
    }

    /// A cloneable send handle, independent of `self`'s lifetime. See
    /// [`Producer`] for why this is a separate type.
    pub(crate) fn producer(&self) -> Producer {
        Producer {
            tx: self.tx.clone(),
            dropped: Arc::clone(&self.dropped),
        }
    }

    #[cfg(test)]
    pub(crate) fn dropped(&self, reason: DropReason) -> u64 {
        self.dropped.get(reason)
    }

    /// Consumes the exporter: signals the task to stop waiting on its
    /// schedule delay, drain whatever is queued, and export it once more —
    /// bounded by `budget`. Cooperative cancellation, not `abort()`: if the
    /// task has not finished within `budget`, whatever was still queued is
    /// accounted as lost to `shutdown` and the task is left to finish
    /// detached rather than killed mid-request.
    pub(crate) async fn shutdown(self, budget: Duration) {
        let Self {
            tx,
            max_queue_size,
            dropped,
            cancel,
            task,
        } = self;
        cancel.cancel();
        if tokio::time::timeout(budget, task).await.is_err() {
            let pending = (max_queue_size - tx.capacity()) as u64;
            dropped.record(DropReason::Shutdown, pending);
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "internal task setup, not a public API"
)]
async fn run_task(
    mut rx: mpsc::Receiver<Vec<u8>>,
    cancel: CancellationToken,
    client: reqwest::Client,
    endpoint: Url,
    headers: HeaderMap,
    timeout: Duration,
    schedule_delay: Duration,
    max_export_batch_size: usize,
    encode_batch: EncodeBatch,
    dropped: Arc<Dropped>,
) {
    let mut batch: Vec<Vec<u8>> = Vec::new();
    loop {
        let sleep = tokio::time::sleep(schedule_delay);
        tokio::pin!(sleep);
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            () = &mut sleep, if !batch.is_empty() => {
                export(&client, &endpoint, &headers, timeout, &encode_batch, &mut batch, &dropped).await;
            }
            received = rx.recv() => {
                match received {
                    Some(record) => {
                        batch.push(record);
                        if batch.len() >= max_export_batch_size {
                            export(&client, &endpoint, &headers, timeout, &encode_batch, &mut batch, &dropped).await;
                        }
                    }
                    None => break,
                }
            }
        }
    }
    // Best-effort final flush: drain whatever is already queued (no more
    // waiting) and export it before the task exits.
    while let Ok(record) = rx.try_recv() {
        batch.push(record);
    }
    if !batch.is_empty() {
        export(
            &client,
            &endpoint,
            &headers,
            timeout,
            &encode_batch,
            &mut batch,
            &dropped,
        )
        .await;
    }
}

async fn export(
    client: &reqwest::Client,
    endpoint: &Url,
    headers: &HeaderMap,
    timeout: Duration,
    encode_batch: &EncodeBatch,
    batch: &mut Vec<Vec<u8>>,
    dropped: &Dropped,
) {
    let records = std::mem::take(batch);
    let count = records.len() as u64;
    let body = encode_batch(&records);

    if let Err(e) = send_with_retry(client, endpoint, headers, timeout, body).await {
        // The response body/error is deliberately not logged: an endpoint
        // may carry credentials in its userinfo.
        let reason = match e {
            SendError::Network => DropReason::Network,
            SendError::HttpStatus(_) => DropReason::HttpStatus,
        };
        dropped.record(reason, count);
    }
}

/// Why a POST never succeeded, after bounded retry. Carries just enough to
/// build a startup-probe error message — never a URL, never a header.
#[derive(Debug)]
pub(crate) enum SendError {
    Network,
    HttpStatus(u16),
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendError::Network => write!(f, "network error"),
            SendError::HttpStatus(status) => write!(f, "HTTP status {status}"),
        }
    }
}

/// POSTs `body` as `application/x-protobuf`, retrying connection errors and
/// 429/502/503/504 responses (honouring `Retry-After` when present) up to
/// [`MAX_SEND_ATTEMPTS`]. No jitter: one client, no herd of them to
/// desynchronise.
pub(crate) async fn send_with_retry(
    client: &reqwest::Client,
    endpoint: &Url,
    headers: &HeaderMap,
    timeout: Duration,
    body: Vec<u8>,
) -> Result<(), SendError> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let result = client
            .post(endpoint.clone())
            .headers(headers.clone())
            .header(
                CONTENT_TYPE,
                HeaderValue::from_static("application/x-protobuf"),
            )
            .timeout(timeout)
            .body(body.clone())
            .send()
            .await;

        match result {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    return Ok(());
                }
                let retryable = matches!(status.as_u16(), 429 | 502 | 503 | 504);
                if retryable && attempt < MAX_SEND_ATTEMPTS {
                    let delay = response
                        .headers()
                        .get(RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .map_or(RETRY_BACKOFF, Duration::from_secs);
                    tokio::time::sleep(delay).await;
                    continue;
                }
                return Err(SendError::HttpStatus(status.as_u16()));
            }
            Err(_) if attempt < MAX_SEND_ATTEMPTS => {
                tokio::time::sleep(RETRY_BACKOFF).await;
            }
            Err(_) => return Err(SendError::Network),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excluded_matches_exact_and_child_targets() {
        assert!(excluded("ruoqa_mcp::otel"));
        assert!(excluded("ruoqa_mcp::otel::pipeline"));
        assert!(excluded("reqwest"));
        assert!(excluded("reqwest::connect"));
        assert!(excluded("hyper_util::client"));
    }

    #[test]
    fn excluded_does_not_match_a_mere_prefix() {
        assert!(!excluded("reqwestish"));
        assert!(!excluded("hyper_utilish::foo"));
        assert!(!excluded("ruoqa_mcp::server"));
    }

    #[tokio::test]
    async fn enqueue_never_awaits_when_queue_is_full() {
        let queue = QueueConfig {
            max_queue_size: 1,
            max_export_batch_size: 512,
            schedule_delay: Duration::from_secs(3600),
        };
        let client = reqwest::Client::new();
        let endpoint = Url::parse("http://127.0.0.1:1/v1/logs").unwrap();
        let encode_batch: EncodeBatch = Arc::new(|records: &[Vec<u8>]| records.concat());
        let exporter = Exporter::start(
            client,
            endpoint,
            HeaderMap::new(),
            Duration::from_secs(1),
            queue,
            encode_batch,
        );

        // Fill the queue (capacity 1), then a second enqueue must drop
        // rather than block, and return promptly.
        let producer = exporter.producer();
        producer.enqueue(vec![1]);
        let start = std::time::Instant::now();
        producer.enqueue(vec![2]);
        assert!(start.elapsed() < Duration::from_millis(100));
        // Either this enqueue landed in the queue or the task already
        // drained slot 0 into an in-flight export; both are acceptable, but
        // `enqueue` must never have blocked.
        exporter.shutdown(Duration::from_millis(50)).await;
    }

    /// A collector that accepts the connection but never answers must still
    /// leave `enqueue` prompt, and the records it cannot make room for are
    /// accounted under `queue_full` — the real point of the drop-accounting
    /// design: nothing here may await the network.
    #[tokio::test]
    async fn hung_collector_drops_queue_full_and_stays_prompt() {
        let collector = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_delay(Duration::from_secs(10)))
            .mount(&collector)
            .await;

        let queue = QueueConfig {
            max_queue_size: 2,
            max_export_batch_size: 1,
            schedule_delay: Duration::from_secs(3600),
        };
        let client = reqwest::Client::new();
        let endpoint = Url::parse(&format!("{}/v1/logs", collector.uri())).unwrap();
        let encode_batch: EncodeBatch = Arc::new(|records: &[Vec<u8>]| records.concat());
        let exporter = Exporter::start(
            client,
            endpoint,
            HeaderMap::new(),
            Duration::from_millis(100),
            queue,
            encode_batch,
        );

        let producer = exporter.producer();
        let start = std::time::Instant::now();
        for i in 0..20u8 {
            producer.enqueue(vec![i]);
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(200),
            "enqueue blocked on a hung collector: {elapsed:?}"
        );

        // The first record's export is now stuck timing out and retrying;
        // the rest of the queue fills up and drops behind it.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(exporter.dropped(DropReason::QueueFull) > 0);

        exporter.shutdown(Duration::from_millis(50)).await;
    }

    /// C4's deferred test: a failing export logs (this file's own
    /// `Dropped::record`), which — without the target exclusion — would
    /// queue a record via the diagnostics `Layer`, triggering another
    /// export. Drives real traffic through a real `Layer` against a really
    /// failing exporter and asserts the drop count is bounded by the
    /// traffic generated, not compounded by the failures warning about
    /// themselves.
    ///
    /// A collector that always answers 500 (not a closed port): 500 is not
    /// in `send_with_retry`'s retryable set, so each export fails on its
    /// first attempt instead of waiting out `RETRY_BACKOFF` three times —
    /// same failure class (`DropReason::HttpStatus`), a deterministic test.
    #[tokio::test]
    async fn diagnostics_layer_does_not_feed_its_own_export_failures_back_in() {
        use tracing_subscriber::layer::SubscriberExt;

        const TRAFFIC: u64 = 20;

        let collector = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&collector)
            .await;

        let queue = QueueConfig {
            max_queue_size: 64,
            max_export_batch_size: 4,
            schedule_delay: Duration::from_millis(10),
        };
        let client = reqwest::Client::new();
        let endpoint = Url::parse(&format!("{}/v1/logs", collector.uri())).unwrap();
        let encode_batch: EncodeBatch = Arc::new(|records: &[Vec<u8>]| records.concat());
        let exporter = Exporter::start(
            client,
            endpoint,
            HeaderMap::new(),
            Duration::from_millis(200),
            queue,
            encode_batch,
        );

        let layer = super::super::logs::DiagnosticsLayer::new(exporter.producer());
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        // Simulated real traffic on a non-excluded target — 20 events, none
        // of them the pipeline's own `target: "ruoqa_mcp::otel"` warnings.
        for i in 0..TRAFFIC {
            tracing::debug!(target: "ruoqa_mcp::server", i, "simulated tool call");
        }

        // Long enough for several export/fail cycles at a 10ms schedule
        // delay and a 200ms per-request timeout.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let dropped = exporter.dropped(DropReason::HttpStatus);
        assert!(
            dropped > 0,
            "the mock 500 should have failed at least one export"
        );
        assert!(
            dropped <= TRAFFIC,
            "drop count ({dropped}) exceeded the traffic generated ({TRAFFIC}): the pipeline's \
             own drop warnings are feeding back into the export queue"
        );

        exporter.shutdown(Duration::from_millis(50)).await;
    }
}
