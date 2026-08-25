//! End-to-end tests for the OTLP logs signal: the diagnostics `tracing`
//! `Layer`, its two independent filters (`RUST_LOG` and the target
//! exclusion), and the audit-stream bridge, all decoded through the
//! independent `tests/common` protobuf reader against a mocked collector.
//!
//! `OTEL_EXPORTER_OTLP_ENDPOINT` and `RUST_LOG` are process environment and
//! `tracing`'s global subscriber can only be installed once per process, so
//! — matching `tests/otel_pipeline.rs`'s own convention — every scenario
//! below lives in one `#[tokio::test]`, and each diagnostics-layer scope
//! uses `tracing::subscriber::with_default` (scoped, per-block) rather than
//! `init()`, so one scenario's layer cannot silently leak into the next.

mod common;

use common::protobuf::{Field, Message};
use ruoqa_mcp::Telemetry;
use ruoqa_mcp::audit::{AuditConfig, Auditor, Outcome, RecordScope, Transport};
use serde_json::json;
use tracing_subscriber::layer::SubscriberExt;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Every `log_records` entry across a single `ExportLogsServiceRequest`,
/// parsed as its own [`Message`]. Mirrors `tests/otel_pipeline.rs`'s
/// `decode_first_log_record`, generalized to more than one record per batch.
fn decode_log_records(body: &[u8]) -> Vec<Message> {
    let request = Message::parse(body).expect("valid ExportLogsServiceRequest");
    let resource_logs = request.msg(1).expect("resource_logs");
    let scope_logs = resource_logs.msg(2).expect("scope_logs");
    scope_logs
        .all(2)
        .into_iter()
        .filter_map(|field| match field {
            Field::Len(bytes) => Message::parse(bytes).ok(),
            _ => None,
        })
        .collect()
}

fn record_body(record: &Message) -> Option<String> {
    record.msg(5)?.str(1)
}

/// String-valued attributes only: `seq` (an `Int`) is not observable through
/// this map, which is fine — no test here needs it.
fn record_attrs(record: &Message) -> std::collections::HashMap<String, String> {
    record
        .all(6)
        .into_iter()
        .filter_map(|field| {
            let Field::Len(kv_bytes) = field else {
                return None;
            };
            let kv = Message::parse(kv_bytes).ok()?;
            let key = kv.str(1)?;
            let value = kv.msg(2)?.str(1)?;
            Some((key, value))
        })
        .collect()
}

fn record_stream(record: &Message) -> Option<String> {
    record_attrs(record).get("ruoqa.stream").cloned()
}

#[tokio::test]
#[allow(
    unsafe_code,
    reason = "edition 2024 requires unsafe for std::env::set_var"
)]
async fn otlp_logs_signal_end_to_end() {
    default_filter_passes_info_blocks_debug_and_excluded_targets().await;
    rust_log_debug_widens_the_diagnostics_filter().await;
    audit_bridge_shares_the_endpoint_and_is_separable_by_stream().await;
}

/// At the default `RUST_LOG` (unset): an INFO event reaches the collector, a
/// DEBUG event does not, and an event on an excluded target (`reqwest`)
/// never arrives even at INFO — two independent filters, both enforced.
#[allow(
    unsafe_code,
    reason = "edition 2024 requires unsafe for std::env::set_var"
)]
async fn default_filter_passes_info_blocks_debug_and_excluded_targets() {
    let collector = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&collector)
        .await;

    // SAFETY: this whole file lives in one `#[tokio::test]`, so no other
    // test thread in this binary reads these variables concurrently.
    unsafe {
        std::env::remove_var("RUST_LOG");
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", collector.uri());
    }

    let telemetry = Telemetry::init()
        .await
        .expect("probe succeeds")
        .expect("OTEL_EXPORTER_OTLP_ENDPOINT is set");

    let subscriber = tracing_subscriber::registry().with(telemetry.diagnostics_layer());
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "ruoqa_mcp::server", "an info event");
        tracing::debug!(target: "ruoqa_mcp::server", "a debug event");
        tracing::info!(target: "reqwest", "an excluded event");
    });

    // No sleep between emitting and shutting down: this also proves records
    // enqueued immediately before `shutdown()` still arrive.
    telemetry.shutdown().await;

    let requests = collector
        .received_requests()
        .await
        .expect("mock server tracks received requests");
    assert_eq!(
        requests.len(),
        2,
        "expected the startup probe plus one flushed batch"
    );
    let bodies: Vec<Option<String>> = decode_log_records(&requests[1].body)
        .iter()
        .map(record_body)
        .collect();
    assert_eq!(
        bodies,
        vec![Some("an info event".to_string())],
        "the debug event and the excluded-target event must not have exported"
    );

    unsafe { std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT") };
}

/// `RUST_LOG=debug` widens the diagnostics filter's default: both an INFO
/// and a DEBUG event now reach the collector.
#[allow(
    unsafe_code,
    reason = "edition 2024 requires unsafe for std::env::set_var"
)]
async fn rust_log_debug_widens_the_diagnostics_filter() {
    let collector = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&collector)
        .await;

    unsafe {
        std::env::set_var("RUST_LOG", "debug");
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", collector.uri());
    }

    let telemetry = Telemetry::init()
        .await
        .expect("probe succeeds")
        .expect("OTEL_EXPORTER_OTLP_ENDPOINT is set");

    let subscriber = tracing_subscriber::registry().with(telemetry.diagnostics_layer());
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "ruoqa_mcp::server", "info under debug");
        tracing::debug!(target: "ruoqa_mcp::server", "debug under debug");
    });
    telemetry.shutdown().await;

    let requests = collector
        .received_requests()
        .await
        .expect("mock server tracks received requests");
    assert_eq!(requests.len(), 2);
    let mut bodies: Vec<String> = decode_log_records(&requests[1].body)
        .iter()
        .filter_map(record_body)
        .collect();
    bodies.sort();
    assert_eq!(
        bodies,
        vec![
            "debug under debug".to_string(),
            "info under debug".to_string()
        ]
    );

    unsafe {
        std::env::remove_var("RUST_LOG");
        std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
    }
}

/// The audit stream and the diagnostics stream share one OTLP endpoint and
/// are separable by `ruoqa.stream`; the exported audit body is byte-equal to
/// the JSONL file line.
#[allow(
    unsafe_code,
    reason = "edition 2024 requires unsafe for std::env::set_var"
)]
async fn audit_bridge_shares_the_endpoint_and_is_separable_by_stream() {
    let collector = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&collector)
        .await;

    unsafe {
        std::env::remove_var("RUST_LOG");
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", collector.uri());
    }

    let telemetry = Telemetry::init()
        .await
        .expect("probe succeeds")
        .expect("OTEL_EXPORTER_OTLP_ENDPOINT is set");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("audit.jsonl");
    let cfg = AuditConfig::parse(&format!("path = {:?}\n", path.to_str().unwrap()))
        .expect("parse audit config");
    let producer = telemetry
        .log_producer()
        .expect("the logs signal is configured");
    let auditor = Auditor::open(&cfg)
        .expect("open audit sink")
        .with_otlp(producer);

    let subscriber = tracing_subscriber::registry().with(telemetry.diagnostics_layer());
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "ruoqa_mcp::server", "diagnostics alongside audit");
    });

    auditor.tool_call(
        "s1",
        Transport::Http,
        RecordScope::Write,
        "add_job_comment",
        Some("osd".to_string()),
        Some(json!({"text": "lgtm"})),
        Outcome::Ok,
        7,
    );

    // No sleep between the calls above and this flush: also proves records
    // enqueued immediately before `shutdown()` still arrive.
    telemetry.shutdown().await;

    let file_line = std::fs::read_to_string(&path).expect("read audit file");
    let file_line = file_line.trim_end();

    let requests = collector
        .received_requests()
        .await
        .expect("mock server tracks received requests");
    assert_eq!(
        requests.len(),
        2,
        "expected the startup probe plus one flushed batch"
    );
    let records = decode_log_records(&requests[1].body);

    let audit_record = records
        .iter()
        .find(|r| record_stream(r).as_deref() == Some("audit"))
        .expect("an audit-tagged record arrived");
    assert_eq!(
        record_body(audit_record).as_deref(),
        Some(file_line),
        "the exported audit body must be byte-equal to the file line"
    );
    let audit_attrs = record_attrs(audit_record);
    assert_eq!(
        audit_attrs.get("event").map(String::as_str),
        Some("tool_call")
    );
    assert_eq!(
        audit_attrs.get("transport").map(String::as_str),
        Some("http")
    );
    assert_eq!(
        audit_attrs.get("tool").map(String::as_str),
        Some("add_job_comment")
    );
    assert_eq!(audit_attrs.get("server").map(String::as_str), Some("osd"));
    assert_eq!(audit_attrs.get("outcome").map(String::as_str), Some("ok"));

    let diagnostics_record = records
        .iter()
        .find(|r| record_stream(r).as_deref() == Some("diagnostics"))
        .expect("a diagnostics-tagged record arrived on the same endpoint");
    assert_eq!(
        record_body(diagnostics_record).as_deref(),
        Some("diagnostics alongside audit")
    );

    unsafe { std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT") };
}
