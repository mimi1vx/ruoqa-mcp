//! End-to-end tests for the public [`ruoqa_mcp::Telemetry`] handle: the
//! startup probe reaching a real (mocked) collector, decoded through the
//! independent `tests/common` protobuf reader, and a collector failure being
//! fatal before anything else starts. Internal pipeline mechanics (batching,
//! retry, drop accounting, the diagnostics-target guard) are unit-tested
//! inside `src/otel/pipeline.rs`, which — unlike this file — can see
//! crate-private types.

mod common;

use common::protobuf::{Field, Message};
use ruoqa_mcp::Telemetry;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// `OTEL_EXPORTER_OTLP_ENDPOINT` is process-global and cargo runs tests in
/// parallel threads within one binary, so every case lives in this one
/// `#[tokio::test]`, matching the convention already established in
/// `cli.rs`'s tests.
#[tokio::test]
#[allow(
    unsafe_code,
    reason = "edition 2024 requires unsafe for std::env::set_var"
)]
async fn startup_probe_success_and_fatal_failure() {
    let collector = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&collector)
        .await;

    // SAFETY: no other test in this binary mutates OTEL_EXPORTER_OTLP_ENDPOINT.
    // This file predates the traces signal and is about the logs probe only;
    // `OTEL_EXPORTER_OTLP_ENDPOINT` would otherwise also light up a second,
    // unasserted probe against the same collector.
    unsafe {
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", collector.uri());
        std::env::set_var("OTEL_EXPORTER_OTLP_TRACES_EXPORTER", "none");
    }

    let telemetry = Telemetry::init()
        .await
        .expect("probe against a 200 collector succeeds")
        .expect("OTEL_EXPORTER_OTLP_ENDPOINT is set, so Telemetry::init must return Some");

    let request = collector
        .received_requests()
        .await
        .expect("mock server tracks received requests")
        .into_iter()
        .next()
        .expect("exactly one probe request was posted");
    assert_eq!(
        request
            .headers
            .get("content-type")
            .map(|v| v.to_str().unwrap()),
        Some("application/x-protobuf")
    );

    let log_record = decode_first_log_record(&request.body);
    assert_eq!(
        log_record.get(2),
        Some(&Field::Varint(9)),
        "severity_number INFO"
    );
    assert_eq!(log_record.str(3), Some("INFO".to_string()));
    let body = log_record.msg(5).expect("LogRecord.body");
    assert_eq!(body.str(1), Some("ruoqa-mcp startup probe".to_string()));
    let attrs = decode_attributes(&log_record);
    assert_eq!(
        attrs.get("ruoqa.stream").map(String::as_str),
        Some("diagnostics")
    );
    assert_eq!(
        attrs.get("event").map(String::as_str),
        Some("startup_probe")
    );

    telemetry.shutdown().await;

    // A collector that only ever answers 503 makes `Telemetry::init` fail —
    // fatal, per the umbrella's step-14 decision — before anything else in
    // `run()` would stand up.
    let failing_collector = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "0"))
        .mount(&failing_collector)
        .await;
    unsafe { std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", failing_collector.uri()) };

    assert!(
        Telemetry::init().await.is_err(),
        "a 503 collector must fail the startup probe"
    );

    unsafe {
        std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        std::env::remove_var("OTEL_EXPORTER_OTLP_TRACES_EXPORTER");
    }
}

/// Navigates `ExportLogsServiceRequest -> ResourceLogs -> ScopeLogs` and
/// returns the first `log_records` entry, parsed as its own `Message`.
fn decode_first_log_record(body: &[u8]) -> Message {
    let request = Message::parse(body).expect("valid ExportLogsServiceRequest");
    let resource_logs = request.msg(1).expect("resource_logs");
    let scope_logs = resource_logs.msg(2).expect("scope_logs");
    let Some(Field::Len(record_bytes)) = scope_logs.all(2).into_iter().next() else {
        panic!("expected at least one log_records entry");
    };
    Message::parse(record_bytes).expect("valid LogRecord")
}

fn decode_attributes(log_record: &Message) -> std::collections::HashMap<String, String> {
    log_record
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
