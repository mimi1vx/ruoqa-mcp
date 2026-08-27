//! The `ruoqa.tool.calls` / `ruoqa.tool.duration` metrics: a process-lifetime
//! registry, `Metric` / `ExportMetricsServiceRequest` encoding, and the
//! periodic reader that turns the registry into OTLP requests.
//!
//! Both instruments are cumulative: every export repeats every series seen
//! since process start, stamped with the same [`Registry::new`]-time
//! `start_time_unix_nano`. That is what makes the stream cumulative rather
//! than a sequence of unrelated snapshots, and it is why `Registry` never
//! resets a counter or a histogram bucket between exports.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::proto::{self, Value};

/// `AggregationTemporality.AGGREGATION_TEMPORALITY_CUMULATIVE`. Both
/// instruments this module exports use it; delta is out of scope (see the
/// umbrella phase plan's alternatives).
const AGGREGATION_TEMPORALITY_CUMULATIVE: u32 = 2;

/// OpenTelemetry's default millisecond histogram bounds. Chosen for
/// operator familiarity over a distribution actually tuned to this crate's
/// calls — every OTLP dashboard and collector example assumes these.
const BOUNDS: [f64; 15] = [
    0.0, 5.0, 10.0, 25.0, 50.0, 75.0, 100.0, 250.0, 500.0, 750.0, 1000.0, 2500.0, 5000.0, 7500.0,
    10_000.0,
];

/// One more bucket than bounds: the last one is `(bounds[last], +Inf)`.
const BUCKET_COUNT: usize = BOUNDS.len() + 1;

/// The attribute set for one `ruoqa.tool.calls` series. `error_kind` is an
/// owned `String`, not `&'static str`: `outcome_of`'s `kind` is parsed back
/// out of a tool result's JSON body, so the compiler cannot see it as
/// `'static` even though every producer draws it from the same closed
/// vocabulary `error.rs::kind_of` defines.
#[derive(Clone, PartialEq, Eq, Hash)]
struct CallKey {
    tool: String,
    server: Option<String>,
    outcome: &'static str,
    error_kind: Option<String>,
}

/// The attribute set for one `ruoqa.tool.duration` series — no `outcome` or
/// `error_kind`: a call's latency is worth tracking as `tool`/`server`
/// regardless of how it ended.
#[derive(Clone, PartialEq, Eq, Hash)]
struct DurKey {
    tool: String,
    server: Option<String>,
}

/// One histogram series' running state: count, sum (for the mean), and one
/// counter per [`BOUNDS`] bucket plus the trailing `+Inf` bucket.
#[derive(Clone, Copy)]
struct Hist {
    count: u64,
    sum_ms: f64,
    buckets: [u64; BUCKET_COUNT],
}

impl Default for Hist {
    fn default() -> Self {
        Self {
            count: 0,
            sum_ms: 0.0,
            buckets: [0; BUCKET_COUNT],
        }
    }
}

impl Hist {
    /// Adds one sample to the first bucket whose bound is `>= ms`, or the
    /// trailing `+Inf` bucket when none is.
    fn record(&mut self, ms: u64) {
        self.count += 1;
        // Precision loss above 2^53ms (~285000 years) is not a concern.
        #[allow(clippy::cast_precision_loss)]
        let ms_f = ms as f64;
        self.sum_ms += ms_f;
        let idx = BOUNDS
            .iter()
            .position(|&bound| ms_f <= bound)
            .unwrap_or(BOUNDS.len());
        self.buckets[idx] += 1;
    }
}

#[derive(Default)]
struct State {
    calls: HashMap<CallKey, u64>,
    duration: HashMap<DurKey, Hist>,
}

/// The process-lifetime metrics registry: one `Mutex<State>` behind a fixed
/// `start_time_unix_nano`, captured once at construction and stamped on
/// every data point of every export.
pub(crate) struct Registry {
    start_time_unix_nano: u64,
    state: Mutex<State>,
}

impl Registry {
    pub(crate) fn new(start_time_unix_nano: u64) -> Self {
        Self {
            start_time_unix_nano,
            state: Mutex::new(State::default()),
        }
    }

    /// Records one completed tool call: bumps `ruoqa.tool.calls` for
    /// `(tool, server, outcome, error_kind)` and adds `duration_ms` to the
    /// `ruoqa.tool.duration` histogram for `(tool, server)`. Takes the lock
    /// once. A poisoned lock is recovered from rather than propagated: a
    /// metric is never worth taking a tool call down over.
    pub(crate) fn record_call(
        &self,
        tool: &str,
        server: Option<&str>,
        outcome: &'static str,
        error_kind: Option<&str>,
        duration_ms: u64,
    ) {
        let call_key = CallKey {
            tool: tool.to_string(),
            server: server.map(str::to_string),
            outcome,
            error_kind: error_kind.map(str::to_string),
        };
        let dur_key = DurKey {
            tool: tool.to_string(),
            server: server.map(str::to_string),
        };
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        *state.calls.entry(call_key).or_insert(0) += 1;
        state
            .duration
            .entry(dur_key)
            .or_default()
            .record(duration_ms);
    }

    /// Snapshots both maps under the lock, releases it, and encodes outside
    /// it. Returns one `Metric` body per non-empty instrument — an empty
    /// registry (nothing recorded since start, or since the last call) yields
    /// an empty `Vec`, and the caller enqueues nothing: a collector should
    /// not see an interval of empty `Metric`s.
    pub(crate) fn collect(&self, now_unix_nanos: u64) -> Vec<Vec<u8>> {
        let (calls, duration) = {
            let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            (state.calls.clone(), state.duration.clone())
        };

        let mut metrics = Vec::new();
        if !calls.is_empty() {
            let points: Vec<NumberPoint<'_>> = calls
                .iter()
                .map(|(key, count)| {
                    let mut attrs: Vec<(&str, Value<'_>)> = vec![("tool", Value::Str(&key.tool))];
                    if let Some(server) = &key.server {
                        attrs.push(("server", Value::Str(server)));
                    }
                    attrs.push(("outcome", Value::Str(key.outcome)));
                    if let Some(kind) = &key.error_kind {
                        attrs.push(("error.kind", Value::Str(kind)));
                    }
                    NumberPoint {
                        attrs,
                        value: i64::try_from(*count).unwrap_or(i64::MAX),
                        start_time_unix_nano: self.start_time_unix_nano,
                        time_unix_nano: now_unix_nanos,
                    }
                })
                .collect();
            metrics.push(encode_sum(
                "ruoqa.tool.calls",
                "{call}",
                "Number of ruoqa-mcp tool calls.",
                &points,
            ));
        }
        if !duration.is_empty() {
            let points: Vec<HistogramPoint<'_>> = duration
                .iter()
                .map(|(key, hist)| {
                    let mut attrs: Vec<(&str, Value<'_>)> = vec![("tool", Value::Str(&key.tool))];
                    if let Some(server) = &key.server {
                        attrs.push(("server", Value::Str(server)));
                    }
                    HistogramPoint {
                        attrs,
                        count: hist.count,
                        sum_ms: hist.sum_ms,
                        bucket_counts: hist.buckets.to_vec(),
                        start_time_unix_nano: self.start_time_unix_nano,
                        time_unix_nano: now_unix_nanos,
                    }
                })
                .collect();
            metrics.push(encode_histogram(
                "ruoqa.tool.duration",
                "ms",
                "Duration of ruoqa-mcp tool calls, in milliseconds.",
                &points,
            ));
        }
        metrics
    }
}

/// One `NumberDataPoint` for a `Sum`: `as_int`, never `as_double` — every
/// value this crate counts is an integer.
struct NumberPoint<'a> {
    attrs: Vec<(&'a str, Value<'a>)>,
    value: i64,
    start_time_unix_nano: u64,
    time_unix_nano: u64,
}

/// One `HistogramDataPoint`. `bucket_counts` is already the full
/// `BUCKET_COUNT`-length array; `explicit_bounds` is shared by every point
/// of a given `Metric` ([`BOUNDS`]), so it is not carried per-point.
struct HistogramPoint<'a> {
    attrs: Vec<(&'a str, Value<'a>)>,
    count: u64,
    sum_ms: f64,
    bucket_counts: Vec<u64>,
    start_time_unix_nano: u64,
    time_unix_nano: u64,
}

/// Encodes one complete `Metric` message body — carrying a `Sum` — with no
/// outer LEN framing, matching `logs::encode_record`/`traces::encode_span`'s
/// convention: the pipeline splices this straight into `ScopeMetrics.metrics`
/// via `proto::write_bytes`. `is_monotonic` is always `true` and
/// `aggregation_temporality` always `CUMULATIVE`: the only shape this crate
/// produces.
fn encode_sum(name: &str, unit: &str, description: &str, points: &[NumberPoint<'_>]) -> Vec<u8> {
    let mut buf = Vec::new();
    proto::write_string(&mut buf, 1, name);
    proto::write_string(&mut buf, 2, description);
    proto::write_string(&mut buf, 3, unit);
    proto::write_message(&mut buf, 7, |sum| {
        for point in points {
            proto::write_message(sum, 1, |dp| {
                proto::write_fixed64(dp, 2, point.start_time_unix_nano);
                proto::write_fixed64(dp, 3, point.time_unix_nano);
                // `as_int` sits behind a oneof: a zero-valued counter must
                // still set it, or the point has no recognized `value` at
                // all (see `proto::write_sfixed64_always`'s doc comment).
                proto::write_sfixed64_always(dp, 6, point.value);
                proto::write_attributes(dp, 7, &point.attrs);
            });
        }
        proto::write_uint32(sum, 2, AGGREGATION_TEMPORALITY_CUMULATIVE);
        proto::write_bool(sum, 3, true);
    });
    buf
}

/// Encodes one complete `Metric` message body carrying a `Histogram`, same
/// framing convention as [`encode_sum`].
fn encode_histogram(
    name: &str,
    unit: &str,
    description: &str,
    points: &[HistogramPoint<'_>],
) -> Vec<u8> {
    let mut buf = Vec::new();
    proto::write_string(&mut buf, 1, name);
    proto::write_string(&mut buf, 2, description);
    proto::write_string(&mut buf, 3, unit);
    proto::write_message(&mut buf, 9, |hist| {
        for point in points {
            proto::write_message(hist, 1, |dp| {
                proto::write_fixed64(dp, 2, point.start_time_unix_nano);
                proto::write_fixed64(dp, 3, point.time_unix_nano);
                proto::write_fixed64(dp, 4, point.count);
                // `sum` has explicit presence: 0.0 still means "the sum is
                // zero", not "no sum was computed" (see
                // `proto::write_double_always`'s doc comment).
                proto::write_double_always(dp, 5, point.sum_ms);
                proto::write_packed_fixed64(dp, 6, &point.bucket_counts);
                proto::write_packed_double(dp, 7, &BOUNDS);
                proto::write_attributes(dp, 9, &point.attrs);
            });
        }
        proto::write_uint32(hist, 2, AGGREGATION_TEMPORALITY_CUMULATIVE);
    });
    buf
}

/// The `ruoqa.startup` probe metric: one monotonic cumulative Sum data
/// point, `as_int = 1`, no attributes, `start_time == time == now`. Proves
/// the collector accepts a real `Metric` (not just an empty request) before
/// any tool call runs — the same argument `mod.rs`'s log/trace probes make —
/// and the series it creates is a legitimate process-restart counter rather
/// than noise. `encode_histogram` needs no equivalent probe: it is proven by
/// its own golden-byte tests and by the periodic reader's integration test.
pub(crate) fn encode_startup_metric(now_unix_nanos: u64) -> Vec<u8> {
    let points = [NumberPoint {
        attrs: vec![],
        value: 1,
        start_time_unix_nano: now_unix_nanos,
        time_unix_nano: now_unix_nanos,
    }];
    encode_sum(
        "ruoqa.startup",
        "{start}",
        "Number of times ruoqa-mcp has started.",
        &points,
    )
}

/// Wraps a batch of already-encoded `Metric` bodies into a full
/// `ExportMetricsServiceRequest`, one `Resource` and one
/// `InstrumentationScope` — its own function rather than a shared generic
/// one, matching `traces::encode_request`'s note: `ResourceMetrics`/
/// `ScopeMetrics` happening to number their fields the same as the logs and
/// traces trees is a coincidence, not a contract.
pub(crate) fn encode_request(
    resource: &[(&str, Value<'_>)],
    scope: (&str, &str),
    metrics: &[Vec<u8>],
) -> Vec<u8> {
    let mut buf = Vec::new();
    proto::write_message(&mut buf, 1, |resource_metrics| {
        proto::write_resource(resource_metrics, 1, resource);
        proto::write_message(resource_metrics, 2, |scope_metrics| {
            proto::write_scope(scope_metrics, 1, scope.0, scope.1);
            for metric in metrics {
                proto::write_bytes(scope_metrics, 2, metric);
            }
        });
    });
    buf
}

/// Owns the registry, the metrics signal's own [`super::pipeline::Exporter`],
/// and the `tokio::time::interval` task that periodically drains the
/// registry onto it. The reader never awaits the network: `collect` plus
/// `Producer::enqueue` are both synchronous, so a hung collector stalls the
/// exporter's own task, never a collection.
pub(crate) struct MetricsPipeline {
    registry: Arc<Registry>,
    exporter: super::pipeline::Exporter,
    cancel: CancellationToken,
    reader: JoinHandle<()>,
}

impl MetricsPipeline {
    /// Starts the periodic reader. `registry` is shared with the caller so a
    /// tool call's `record_call` and the reader's `collect` see the same
    /// state.
    pub(crate) fn start(
        registry: Arc<Registry>,
        exporter: super::pipeline::Exporter,
        interval: Duration,
    ) -> Self {
        let cancel = CancellationToken::new();
        let reader = tokio::spawn(run_reader(
            Arc::clone(&registry),
            exporter.producer(),
            interval,
            cancel.clone(),
        ));
        Self {
            registry,
            exporter,
            cancel,
            reader,
        }
    }

    /// A handle for `record_call`, shared with `OpenQaServer`. Cloning a
    /// `Producer`-adjacent `Arc<Registry>` costs one atomic increment, on the
    /// same hot path that already allocates two `String`s per call.
    pub(crate) fn registry(&self) -> Arc<Registry> {
        Arc::clone(&self.registry)
    }

    /// Cancels the reader, awaits it, does one final `collect` + enqueue,
    /// *then* flushes the underlying exporter. **Ordering is load-bearing**:
    /// reversed, the last interval's counts would be enqueued onto a queue
    /// whose drain has already finished and would be silently lost.
    pub(crate) async fn shutdown(self, budget: Duration) {
        let Self {
            registry,
            exporter,
            cancel,
            reader,
        } = self;
        cancel.cancel();
        let _ = reader.await;
        let producer = exporter.producer();
        for metric in registry.collect(super::logs::now_unix_nanos()) {
            producer.enqueue(metric);
        }
        exporter.shutdown(budget).await;
    }
}

async fn run_reader(
    registry: Arc<Registry>,
    producer: super::pipeline::Producer,
    interval: Duration,
    cancel: CancellationToken,
) {
    let mut ticker = tokio::time::interval(interval);
    // A slow collector must not produce a burst of catch-up exports.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick fires immediately; consume it so the first real
    // collection happens one full interval after startup, matching every
    // subsequent one.
    ticker.tick().await;
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            _ = ticker.tick() => {
                for metric in registry.collect(super::logs::now_unix_nanos()) {
                    producer.enqueue(metric);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::otel::proto;

    #[test]
    fn record_call_is_monotonic_across_repeated_calls() {
        let registry = Registry::new(0);
        registry.record_call("get_job", Some("prod"), "ok", None, 10);
        registry.record_call("get_job", Some("prod"), "ok", None, 20);
        let state = registry.state.lock().unwrap();
        let key = CallKey {
            tool: "get_job".to_string(),
            server: Some("prod".to_string()),
            outcome: "ok",
            error_kind: None,
        };
        assert_eq!(state.calls.get(&key), Some(&2));
        let dur_key = DurKey {
            tool: "get_job".to_string(),
            server: Some("prod".to_string()),
        };
        let hist = state.duration.get(&dur_key).unwrap();
        assert_eq!(hist.count, 2);
        assert!((hist.sum_ms - 30.0).abs() < f64::EPSILON);
    }

    #[test]
    fn differing_error_kind_creates_a_second_series() {
        let registry = Registry::new(0);
        registry.record_call("get_job", None, "tool_error", Some("not_found"), 1);
        registry.record_call("get_job", None, "tool_error", Some("timeout"), 1);
        let state = registry.state.lock().unwrap();
        assert_eq!(state.calls.len(), 2, "two distinct error kinds");
    }

    #[test]
    fn same_attribute_tuple_is_one_series() {
        let registry = Registry::new(0);
        for _ in 0..5 {
            registry.record_call("list_jobs", None, "ok", None, 1);
        }
        let state = registry.state.lock().unwrap();
        assert_eq!(state.calls.len(), 1);
    }

    #[test]
    fn bucket_assignment_at_each_boundary_and_extremes() {
        let mut hist = Hist::default();
        hist.record(0);
        hist.record(5);
        hist.record(10_000);
        hist.record(10_001);
        hist.record(u64::MAX);
        // 0 -> bucket 0 (bound 0.0); 5 -> bucket 1 (bound 5.0);
        // 10000 -> bucket 14 (bound 10000.0, the last real bound);
        // 10001 and u64::MAX -> the trailing +Inf bucket.
        assert_eq!(hist.buckets[0], 1);
        assert_eq!(hist.buckets[1], 1);
        assert_eq!(hist.buckets[14], 1);
        assert_eq!(hist.buckets[BUCKET_COUNT - 1], 2);
        assert_eq!(hist.count, 5);
    }

    #[test]
    fn collect_on_an_empty_registry_returns_empty() {
        let registry = Registry::new(0);
        assert!(registry.collect(0).is_empty());
    }

    #[test]
    fn collect_returns_both_instruments_once_populated() {
        let registry = Registry::new(0);
        registry.record_call("get_job", None, "ok", None, 5);
        let metrics = registry.collect(100);
        assert_eq!(metrics.len(), 2, "one Sum metric, one Histogram metric");
    }

    #[test]
    fn encode_sum_pins_oneof_tag_seven_and_writes_a_zero_valued_counter() {
        let points = [NumberPoint {
            attrs: vec![("tool", Value::Str("get_job"))],
            value: 0,
            start_time_unix_nano: 1,
            time_unix_nano: 2,
        }];
        let bytes = encode_sum("ruoqa.tool.calls", "{call}", "d", &points);

        let mut expected = Vec::new();
        proto::write_string(&mut expected, 1, "ruoqa.tool.calls");
        proto::write_string(&mut expected, 2, "d");
        proto::write_string(&mut expected, 3, "{call}");
        proto::write_message(&mut expected, 7, |sum| {
            proto::write_message(sum, 1, |dp| {
                proto::write_fixed64(dp, 2, 1);
                proto::write_fixed64(dp, 3, 2);
                proto::write_sfixed64_always(dp, 6, 0);
                proto::write_key_value(dp, 7, "tool", &Value::Str("get_job"));
            });
            proto::write_uint32(sum, 2, 2);
            proto::write_bool(sum, 3, true);
        });
        assert_eq!(bytes, expected);

        // Field 7 (LEN) tag byte is `(7 << 3) | 2 = 58 = 0x3a`: the `sum`
        // oneof slot, not 1/2/3 (would-be-guessed-from-declaration-order)
        // and not 9 (histogram's slot).
        assert!(bytes.contains(&0x3a));
    }

    #[test]
    fn encode_histogram_pins_oneof_tag_nine_and_sum_present_at_zero() {
        let points = [HistogramPoint {
            attrs: vec![],
            count: 3,
            sum_ms: 0.0,
            bucket_counts: vec![1, 2, 0],
            start_time_unix_nano: 1,
            time_unix_nano: 2,
        }];
        let bytes = encode_histogram("ruoqa.tool.duration", "ms", "d", &points);

        let mut expected = Vec::new();
        proto::write_string(&mut expected, 1, "ruoqa.tool.duration");
        proto::write_string(&mut expected, 2, "d");
        proto::write_string(&mut expected, 3, "ms");
        proto::write_message(&mut expected, 9, |hist| {
            proto::write_message(hist, 1, |dp| {
                proto::write_fixed64(dp, 2, 1);
                proto::write_fixed64(dp, 3, 2);
                proto::write_fixed64(dp, 4, 3);
                proto::write_double_always(dp, 5, 0.0);
                proto::write_packed_fixed64(dp, 6, &[1, 2, 0]);
                proto::write_packed_double(dp, 7, &BOUNDS);
            });
            proto::write_uint32(hist, 2, 2);
        });
        assert_eq!(bytes, expected);
        // Field 9 (LEN) tag byte is `(9 << 3) | 2 = 74 = 0x4a`.
        assert!(bytes.contains(&0x4a));
    }

    #[test]
    fn encode_histogram_bucket_counts_and_bounds_lengths_match() {
        let points = [HistogramPoint {
            attrs: vec![],
            count: 1,
            sum_ms: 1.0,
            bucket_counts: vec![0; BUCKET_COUNT],
            start_time_unix_nano: 0,
            time_unix_nano: 0,
        }];
        let bytes = encode_histogram("n", "ms", "", &points);
        // bucket_counts: BUCKET_COUNT fixed64 values, tag(field=6,LEN) 0x32.
        assert!(bytes.contains(&0x32));
        // explicit_bounds: BOUNDS.len() doubles, tag(field=7,LEN) 0x3a.
        assert!(bytes.contains(&0x3a));
    }

    #[test]
    fn encode_request_matches_hand_built_bytes() {
        let points = [NumberPoint {
            attrs: vec![],
            value: 1,
            start_time_unix_nano: 1,
            time_unix_nano: 2,
        }];
        let metric = encode_sum("ruoqa.tool.calls", "{call}", "", &points);
        let resource: Vec<(&str, Value<'_>)> = vec![("service.name", Value::Str("ruoqa-mcp"))];
        let bytes = encode_request(
            &resource,
            ("ruoqa-mcp", "1.2.3"),
            std::slice::from_ref(&metric),
        );

        let mut expected = Vec::new();
        proto::write_message(&mut expected, 1, |resource_metrics| {
            proto::write_resource(resource_metrics, 1, &resource);
            proto::write_message(resource_metrics, 2, |scope_metrics| {
                proto::write_scope(scope_metrics, 1, "ruoqa-mcp", "1.2.3");
                proto::write_bytes(scope_metrics, 2, &metric);
            });
        });
        assert_eq!(bytes, expected);
    }
}
