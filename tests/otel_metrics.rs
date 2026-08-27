//! End-to-end tests for the OTLP metrics signal: `ruoqa.tool.calls` and
//! `ruoqa.tool.duration` reaching a mocked collector, cumulative behaviour
//! across intervals, the idle case, `OTEL_METRICS_EXPORTER=none`, and
//! shutdown ordering — decoded through the independent `tests/common`
//! protobuf reader, driving real tool calls through a real MCP session
//! (matching `tests/otel_traces.rs`'s harness) against a mocked openQA
//! server.
//!
//! `OTEL_*` is process environment and cargo runs tests in parallel threads
//! within one binary, so every scenario below lives in one
//! `#[tokio::test]`, matching `tests/otel_logs.rs`/`tests/otel_traces.rs`'s
//! established convention.

mod common;

use std::time::Duration;

use common::protobuf::{Field, Message};
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use ruoqa_mcp::{OpenQaServer, ServerRegistry, Telemetry};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_SERVER: &str = "test";

async fn run_server(openqa: &MockServer, telemetry: &Telemetry) -> RunningService<RoleClient, ()> {
    let client = ruoqa::ClientBuilder::new()
        .server(openqa.uri())
        .config_paths(vec![])
        .build()
        .expect("build client");
    let mut clients = std::collections::HashMap::new();
    clients.insert(TEST_SERVER.to_string(), client);
    let server = OpenQaServer::new(ServerRegistry::from_map(clients), false)
        .with_metrics(telemetry.metric_recorder());

    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_transport).await {
            let _ = running.waiting().await;
        }
    });
    ().serve(client_transport).await.expect("client handshake")
}

async fn call(
    client: &RunningService<RoleClient, ()>,
    name: &str,
    args: Value,
) -> Result<rmcp::model::CallToolResult, rmcp::ServiceError> {
    let mut obj = args.as_object().cloned().unwrap_or_default();
    obj.entry("server".to_string())
        .or_insert_with(|| json!(TEST_SERVER));
    let params = CallToolRequestParams::new(name.to_string()).with_arguments(obj);
    client.peer().call_tool(params).await
}

/// Requests a mocked collector received on `path`, in arrival order.
async fn requests_to(collector: &MockServer, path: &str) -> Vec<wiremock::Request> {
    collector
        .received_requests()
        .await
        .expect("mock server tracks received requests")
        .into_iter()
        .filter(|r| r.url.path() == path)
        .collect()
}

/// Every top-level `metrics` entry across a single
/// `ExportMetricsServiceRequest`, parsed as its own [`Message`].
fn decode_metrics(body: &[u8]) -> Vec<Message> {
    let request = Message::parse(body).expect("valid ExportMetricsServiceRequest");
    let resource_metrics = request.msg(1).expect("resource_metrics");
    let scope_metrics = resource_metrics.msg(2).expect("scope_metrics");
    scope_metrics
        .all(2)
        .into_iter()
        .filter_map(|field| match field {
            Field::Len(bytes) => Message::parse(bytes).ok(),
            _ => None,
        })
        .collect()
}

fn metric_by_name<'a>(metrics: &'a [Message], name: &str) -> Option<&'a Message> {
    metrics.iter().find(|m| m.str(1).as_deref() == Some(name))
}

/// `Sum.data_points` (field 7 -> field 1), each parsed as its own `Message`.
fn sum_points(metric: &Message) -> Vec<Message> {
    let Some(sum) = metric.msg(7) else {
        return Vec::new();
    };
    sum.all(1)
        .into_iter()
        .filter_map(|field| match field {
            Field::Len(bytes) => Message::parse(bytes).ok(),
            _ => None,
        })
        .collect()
}

/// `Histogram.data_points` (field 9 -> field 1), each parsed as its own
/// `Message`.
fn histogram_points(metric: &Message) -> Vec<Message> {
    let Some(hist) = metric.msg(9) else {
        return Vec::new();
    };
    hist.all(1)
        .into_iter()
        .filter_map(|field| match field {
            Field::Len(bytes) => Message::parse(bytes).ok(),
            _ => None,
        })
        .collect()
}

fn kv_str_attr(point: &Message, attrs_field: u32, key: &str) -> Option<String> {
    point.all(attrs_field).into_iter().find_map(|field| {
        let Field::Len(kv_bytes) = field else {
            return None;
        };
        let kv = Message::parse(kv_bytes).ok()?;
        if kv.str(1).as_deref() != Some(key) {
            return None;
        }
        kv.msg(2)?.str(1)
    })
}

/// `NumberDataPoint.attributes` is field 7.
fn number_attr(point: &Message, key: &str) -> Option<String> {
    kv_str_attr(point, 7, key)
}

/// `HistogramDataPoint.attributes` is field 9 — a different number from
/// `NumberDataPoint`'s, per `opentelemetry-proto`.
fn histogram_attr(point: &Message, key: &str) -> Option<String> {
    kv_str_attr(point, 9, key)
}

#[tokio::test]
#[allow(
    unsafe_code,
    reason = "edition 2024 requires unsafe for std::env::set_var"
)]
async fn otlp_metrics_signal_end_to_end() {
    tool_call_produces_matching_calls_and_duration_points().await;
    failing_call_adds_a_second_calls_series_with_error_kind().await;
    cumulative_series_rises_and_start_time_is_stable().await;
    idle_interval_produces_no_metrics_request().await;
    metrics_exporter_none_leaves_logs_working().await;
    three_signals_on_one_base_endpoint_arrive_on_their_own_paths().await;
    a_call_recorded_immediately_before_shutdown_still_arrives().await;
}

/// One successful `get_job` call, immediately followed by `shutdown()` (no
/// periodic tick has a chance to fire — the default export interval is a
/// minute), produces one `ruoqa.tool.calls` point (`tool`, `server`,
/// `outcome`) and one `ruoqa.tool.duration` point (`tool`, `server`) whose
/// bucket counts sum to its `count`. This is also F7's "recorded immediately
/// before shutdown still arrives" case: `MetricsPipeline::shutdown`'s final
/// `collect` is the only thing that can have produced this export.
#[allow(
    unsafe_code,
    reason = "edition 2024 requires unsafe for std::env::set_var"
)]
async fn tool_call_produces_matching_calls_and_duration_points() {
    let openqa = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1})))
        .mount(&openqa)
        .await;
    let collector = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&collector)
        .await;

    unsafe {
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", collector.uri());
        std::env::set_var("OTEL_TRACES_EXPORTER", "none");
    }

    let telemetry = Telemetry::init()
        .await
        .expect("probe succeeds")
        .expect("OTEL_EXPORTER_OTLP_ENDPOINT is set");
    let client = run_server(&openqa, &telemetry).await;

    call(&client, "get_job", json!({"job_id": 1}))
        .await
        .expect("call_tool");

    telemetry.shutdown().await;

    // Startup probe, plus shutdown's final flush.
    let requests = requests_to(&collector, "/v1/metrics").await;
    assert_eq!(requests.len(), 2);
    let metrics = decode_metrics(&requests[1].body);
    let calls = metric_by_name(&metrics, "ruoqa.tool.calls").expect("calls metric exported");
    let sum = calls.msg(7).expect("Sum submessage");
    assert_eq!(
        sum.get(2),
        Some(&Field::Varint(2)),
        "AGGREGATION_TEMPORALITY_CUMULATIVE"
    );
    assert_eq!(sum.get(3), Some(&Field::Varint(1)), "is_monotonic");
    let points = sum_points(calls);
    let point = points
        .iter()
        .find(|p| number_attr(p, "tool").as_deref() == Some("get_job"))
        .expect("a get_job calls point");
    // `server` is the client's resolved canonical `host:port` id, not the
    // `"test"` selector the call used — same as `resolve_id`'s contract for
    // the audit stream and the tool span.
    let server = number_attr(point, "server").expect("server attribute present");
    assert_eq!(number_attr(point, "outcome").as_deref(), Some("ok"));
    assert_eq!(point.u64(6), Some(1), "as_int");

    let duration =
        metric_by_name(&metrics, "ruoqa.tool.duration").expect("duration metric exported");
    let hist_points = histogram_points(duration);
    let hist_point = hist_points
        .iter()
        .find(|p| histogram_attr(p, "tool").as_deref() == Some("get_job"))
        .expect("a get_job duration point");
    assert_eq!(
        histogram_attr(hist_point, "server").as_deref(),
        Some(server.as_str()),
        "the calls point and the duration point must agree on server"
    );
    let count = hist_point.u64(4).expect("count");
    let buckets = hist_point.packed_u64(6).expect("bucket_counts");
    assert_eq!(buckets.iter().sum::<u64>(), count);
    assert_eq!(count, 1);

    unsafe {
        std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        std::env::remove_var("OTEL_TRACES_EXPORTER");
    }
}

/// A failing `get_job` call (openQA answers 404) produces a second
/// `ruoqa.tool.calls` series carrying `error.kind`, distinct from any
/// successful-call series for the same tool.
#[allow(
    unsafe_code,
    reason = "edition 2024 requires unsafe for std::env::set_var"
)]
async fn failing_call_adds_a_second_calls_series_with_error_kind() {
    let openqa = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&openqa)
        .await;
    let collector = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&collector)
        .await;

    unsafe {
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", collector.uri());
        std::env::set_var("OTEL_TRACES_EXPORTER", "none");
    }

    let telemetry = Telemetry::init()
        .await
        .expect("probe succeeds")
        .expect("OTEL_EXPORTER_OTLP_ENDPOINT is set");
    let client = run_server(&openqa, &telemetry).await;

    call(&client, "get_job", json!({"job_id": 1}))
        .await
        .expect("a tool error is not a protocol error");

    telemetry.shutdown().await;

    let requests = requests_to(&collector, "/v1/metrics").await;
    assert_eq!(requests.len(), 2);
    let metrics = decode_metrics(&requests[1].body);
    let calls = metric_by_name(&metrics, "ruoqa.tool.calls").expect("calls metric exported");
    let points = sum_points(calls);
    let point = points
        .iter()
        .find(|p| {
            number_attr(p, "tool").as_deref() == Some("get_job")
                && number_attr(p, "outcome").as_deref() == Some("tool_error")
        })
        .expect("a tool_error calls point");
    assert_eq!(
        number_attr(point, "error.kind").as_deref(),
        Some("not_found")
    );

    unsafe {
        std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        std::env::remove_var("OTEL_TRACES_EXPORTER");
    }
}

/// Two successive periodic exports: the `ruoqa.tool.calls` Sum's value rises
/// with a second call in between, while `start_time_unix_nano` stays
/// byte-identical — the one test that distinguishes cumulative from delta.
#[allow(
    unsafe_code,
    reason = "edition 2024 requires unsafe for std::env::set_var"
)]
async fn cumulative_series_rises_and_start_time_is_stable() {
    let openqa = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1})))
        .mount(&openqa)
        .await;
    let collector = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&collector)
        .await;

    unsafe {
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", collector.uri());
        std::env::set_var("OTEL_TRACES_EXPORTER", "none");
        std::env::set_var("OTEL_METRIC_EXPORT_INTERVAL", "50");
        // The registry re-exports every series, unchanged or not, on every
        // tick, so the queue always has exactly 2 items (one Sum, one
        // Histogram) to flush per collection — forcing an immediate export
        // per tick instead of waiting on the default 5s batch delay, so
        // each periodic collection lands as its own HTTP request.
        std::env::set_var("OTEL_BLRP_MAX_EXPORT_BATCH_SIZE", "2");
    }

    let telemetry = Telemetry::init()
        .await
        .expect("probe succeeds")
        .expect("OTEL_EXPORTER_OTLP_ENDPOINT is set");
    let client = run_server(&openqa, &telemetry).await;

    call(&client, "get_job", json!({"job_id": 1}))
        .await
        .expect("call_tool");
    let first = wait_for_calls_value(&collector, 1).await;

    call(&client, "get_job", json!({"job_id": 1}))
        .await
        .expect("call_tool");
    let second = wait_for_calls_value(&collector, 2).await;

    telemetry.shutdown().await;

    assert_eq!(
        first.u64(2),
        second.u64(2),
        "start_time_unix_nano must stay identical across exports"
    );
    assert!(
        second.u64(6).unwrap() > first.u64(6).unwrap(),
        "the Sum must have risen"
    );

    unsafe {
        std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        std::env::remove_var("OTEL_TRACES_EXPORTER");
        std::env::remove_var("OTEL_METRIC_EXPORT_INTERVAL");
        std::env::remove_var("OTEL_BLRP_MAX_EXPORT_BATCH_SIZE");
    }
}

fn get_job_calls_point(body: &[u8]) -> Message {
    let metrics = decode_metrics(body);
    let calls = metric_by_name(&metrics, "ruoqa.tool.calls").expect("calls metric exported");
    sum_points(calls)
        .into_iter()
        .find(|p| number_attr(p, "tool").as_deref() == Some("get_job"))
        .expect("a get_job calls point")
}

/// Polls `/v1/metrics` requests (most recent first) until one carries a
/// `get_job` calls point whose value is exactly `expected`, or panics after
/// 2s. Robust to the periodic reader re-exporting an unchanged cumulative
/// value on every tick: it looks for the value, not a request index.
async fn wait_for_calls_value(collector: &MockServer, expected: u64) -> Message {
    for _ in 0..100 {
        let requests = requests_to(collector, "/v1/metrics").await;
        for request in requests.iter().rev() {
            let metrics = decode_metrics(&request.body);
            let Some(calls) = metric_by_name(&metrics, "ruoqa.tool.calls") else {
                continue;
            };
            if let Some(point) = sum_points(calls)
                .into_iter()
                .find(|p| number_attr(p, "tool").as_deref() == Some("get_job"))
                && point.u64(6) == Some(expected)
            {
                return point;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for a get_job calls point valued {expected}");
}

/// An interval with no tool calls in between produces no request to
/// `/v1/metrics` beyond the startup probe.
#[allow(
    unsafe_code,
    reason = "edition 2024 requires unsafe for std::env::set_var"
)]
async fn idle_interval_produces_no_metrics_request() {
    let collector = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&collector)
        .await;

    unsafe {
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", collector.uri());
        std::env::set_var("OTEL_TRACES_EXPORTER", "none");
        std::env::set_var("OTEL_METRIC_EXPORT_INTERVAL", "80");
    }

    let telemetry = Telemetry::init()
        .await
        .expect("probe succeeds")
        .expect("OTEL_EXPORTER_OTLP_ENDPOINT is set");

    // Long enough for at least two intervals to have fired with nothing
    // recorded.
    tokio::time::sleep(Duration::from_millis(250)).await;
    telemetry.shutdown().await;

    let requests = requests_to(&collector, "/v1/metrics").await;
    assert_eq!(
        requests.len(),
        1,
        "only the startup probe, no periodic export for an idle registry"
    );

    unsafe {
        std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        std::env::remove_var("OTEL_TRACES_EXPORTER");
        std::env::remove_var("OTEL_METRIC_EXPORT_INTERVAL");
    }
}

/// `OTEL_METRICS_EXPORTER=none` with a base endpoint set produces zero
/// requests to `/v1/metrics` (including the startup probe), while the logs
/// signal keeps working.
#[allow(
    unsafe_code,
    reason = "edition 2024 requires unsafe for std::env::set_var"
)]
async fn metrics_exporter_none_leaves_logs_working() {
    let collector = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&collector)
        .await;

    unsafe {
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", collector.uri());
        std::env::set_var("OTEL_TRACES_EXPORTER", "none");
        std::env::set_var("OTEL_METRICS_EXPORTER", "none");
    }

    let telemetry = Telemetry::init()
        .await
        .expect("probe succeeds")
        .expect("the logs signal is still configured");
    assert!(telemetry.metric_recorder().is_none(), "metrics must be off");
    telemetry.shutdown().await;

    let metrics_requests = requests_to(&collector, "/v1/metrics").await;
    assert!(
        metrics_requests.is_empty(),
        "no request to /v1/metrics at all"
    );
    let log_requests = requests_to(&collector, "/v1/logs").await;
    assert_eq!(log_requests.len(), 1, "the logs startup probe still ran");

    unsafe {
        std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        std::env::remove_var("OTEL_TRACES_EXPORTER");
        std::env::remove_var("OTEL_METRICS_EXPORTER");
    }
}

/// A single `OTEL_EXPORTER_OTLP_ENDPOINT` lights up all three signals, each
/// arriving on its own path with its own startup probe.
#[allow(
    unsafe_code,
    reason = "edition 2024 requires unsafe for std::env::set_var"
)]
async fn three_signals_on_one_base_endpoint_arrive_on_their_own_paths() {
    let openqa = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1})))
        .mount(&openqa)
        .await;
    let collector = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&collector)
        .await;

    unsafe { std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", collector.uri()) };

    let telemetry = Telemetry::init()
        .await
        .expect("probe succeeds")
        .expect("OTEL_EXPORTER_OTLP_ENDPOINT is set");
    let client = run_server(&openqa, &telemetry).await;

    call(&client, "get_job", json!({"job_id": 1}))
        .await
        .expect("call_tool");

    telemetry.shutdown().await;

    assert!(!requests_to(&collector, "/v1/logs").await.is_empty());
    assert!(!requests_to(&collector, "/v1/traces").await.is_empty());
    assert!(!requests_to(&collector, "/v1/metrics").await.is_empty());

    unsafe { std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT") };
}

/// A `record_call` made immediately before `shutdown()` — no sleep in
/// between — still reaches the collector: `MetricsPipeline::shutdown`'s
/// final `collect` runs before the underlying exporter is flushed.
#[allow(
    unsafe_code,
    reason = "edition 2024 requires unsafe for std::env::set_var"
)]
async fn a_call_recorded_immediately_before_shutdown_still_arrives() {
    let openqa = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1})))
        .mount(&openqa)
        .await;
    let collector = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&collector)
        .await;

    unsafe {
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", collector.uri());
        std::env::set_var("OTEL_TRACES_EXPORTER", "none");
        // Long enough that no periodic tick fires before shutdown.
        std::env::set_var("OTEL_METRIC_EXPORT_INTERVAL", "60000");
    }

    let telemetry = Telemetry::init()
        .await
        .expect("probe succeeds")
        .expect("OTEL_EXPORTER_OTLP_ENDPOINT is set");
    let client = run_server(&openqa, &telemetry).await;

    call(&client, "get_job", json!({"job_id": 1}))
        .await
        .expect("call_tool");
    telemetry.shutdown().await;

    let requests = requests_to(&collector, "/v1/metrics").await;
    assert_eq!(
        requests.len(),
        2,
        "the startup probe, plus one export from shutdown's final collect"
    );
    let point = get_job_calls_point(&requests[1].body);
    assert_eq!(point.u64(6), Some(1));

    unsafe {
        std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        std::env::remove_var("OTEL_TRACES_EXPORTER");
        std::env::remove_var("OTEL_METRIC_EXPORT_INTERVAL");
    }
}
