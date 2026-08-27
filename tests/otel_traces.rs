//! End-to-end tests for the OTLP traces signal: the tool-call `SERVER` span,
//! its `openqa.request` `CLIENT` child (through both `request_json` and
//! `tools::artifact::execute`), inbound `traceparent` adoption, the
//! sampler, and the per-signal `OTEL_TRACES_EXPORTER` switch — decoded
//! through the independent `tests/common` protobuf reader against a mocked
//! collector, driving real tool calls through a real MCP session (matching
//! `tests/tools.rs`'s harness) against a mocked openQA server.
//!
//! `OTEL_*` and `RUST_LOG` are process environment and cargo runs tests in
//! parallel threads within one binary, so every scenario below lives in one
//! `#[tokio::test]`, matching `tests/otel_logs.rs`'s established convention.

mod common;

use std::sync::Arc;

use common::protobuf::{Field, Message};
use rmcp::model::{CallToolRequestParams, RequestParamsMeta};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use ruoqa_mcp::audit::Auditor;
use ruoqa_mcp::{OpenQaServer, ServerRegistry, Telemetry, TraceProducer};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_SERVER: &str = "test";

async fn run_server(
    openqa: &MockServer,
    traces: Option<TraceProducer>,
) -> RunningService<RoleClient, ()> {
    run_server_with_audit(openqa, traces, None).await
}

async fn run_server_with_audit(
    openqa: &MockServer,
    traces: Option<TraceProducer>,
    audit: Option<Arc<Auditor>>,
) -> RunningService<RoleClient, ()> {
    let client = ruoqa::ClientBuilder::new()
        .server(openqa.uri())
        .config_paths(vec![])
        .build()
        .expect("build client");
    let mut clients = std::collections::HashMap::new();
    clients.insert(TEST_SERVER.to_string(), client);
    let server = OpenQaServer::new(ServerRegistry::from_map(clients), false)
        .with_traces(traces)
        .with_audit(audit);

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
    traceparent: Option<&str>,
) -> Result<rmcp::model::CallToolResult, rmcp::ServiceError> {
    let mut obj = args.as_object().cloned().unwrap_or_default();
    obj.entry("server".to_string())
        .or_insert_with(|| json!(TEST_SERVER));
    let mut params = CallToolRequestParams::new(name.to_string()).with_arguments(obj);
    if let Some(tp) = traceparent {
        params.set_traceparent(tp);
    }
    client.peer().call_tool(params).await
}

/// Every `spans` entry across a single `ExportTraceServiceRequest`, parsed
/// as its own [`Message`]. Mirrors `tests/otel_logs.rs`'s
/// `decode_log_records`.
fn decode_spans(body: &[u8]) -> Vec<Message> {
    let request = Message::parse(body).expect("valid ExportTraceServiceRequest");
    let resource_spans = request.msg(1).expect("resource_spans");
    let scope_spans = resource_spans.msg(2).expect("scope_spans");
    scope_spans
        .all(2)
        .into_iter()
        .filter_map(|field| match field {
            Field::Len(bytes) => Message::parse(bytes).ok(),
            _ => None,
        })
        .collect()
}

fn span_name(span: &Message) -> Option<String> {
    span.str(5)
}

fn span_trace_id(span: &Message) -> Option<Vec<u8>> {
    match span.get(1) {
        Some(Field::Len(bytes)) => Some(bytes.clone()),
        _ => None,
    }
}

fn span_id(span: &Message) -> Option<Vec<u8>> {
    match span.get(2) {
        Some(Field::Len(bytes)) => Some(bytes.clone()),
        _ => None,
    }
}

fn span_parent_id(span: &Message) -> Option<Vec<u8>> {
    match span.get(4) {
        Some(Field::Len(bytes)) => Some(bytes.clone()),
        _ => None,
    }
}

fn span_str_attr(span: &Message, key: &str) -> Option<String> {
    span.all(9).into_iter().find_map(|field| {
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

#[tokio::test]
#[allow(
    unsafe_code,
    reason = "edition 2024 requires unsafe for std::env::set_var"
)]
async fn otlp_traces_signal_end_to_end() {
    two_span_trace_through_request_json().await;
    two_span_trace_through_artifact_execute().await;
    client_traceparent_is_adopted_onto_the_exported_span().await;
    malformed_traceparent_falls_back_to_a_root_span().await;
    parentbased_always_on_respects_an_unsampled_parent().await;
    unsampled_call_still_writes_an_audit_record().await;
    traceparent_matches_byte_for_byte_on_span_and_audit_record().await;
    traces_exporter_none_leaves_logs_working().await;
}

/// `get_job` reaches the wire only through `request_json`: the exported
/// trace must carry `mcp.tool/get_job` (SERVER) parenting `openqa.request`
/// (CLIENT), same `trace_id`, correct `parent_span_id`.
#[allow(
    unsafe_code,
    reason = "edition 2024 requires unsafe for std::env::set_var"
)]
async fn two_span_trace_through_request_json() {
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

    // SAFETY: this whole file lives in one `#[tokio::test]`.
    unsafe { std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", collector.uri()) };

    let telemetry = Telemetry::init()
        .await
        .expect("probe succeeds")
        .expect("OTEL_EXPORTER_OTLP_ENDPOINT is set");
    let traces = telemetry.trace_producer();
    let client = run_server(&openqa, traces).await;

    call(&client, "get_job", json!({"job_id": 1}), None)
        .await
        .expect("call_tool");

    telemetry.shutdown().await;

    let requests = requests_to(&collector, "/v1/traces").await;
    assert_eq!(
        requests.len(),
        2,
        "expected the startup probe plus one flushed batch"
    );
    let spans = decode_spans(&requests[1].body);
    let tool_span = spans
        .iter()
        .find(|s| span_name(s).as_deref() == Some("mcp.tool/get_job"))
        .expect("a tool span was exported");
    let upstream_span = spans
        .iter()
        .find(|s| span_name(s).as_deref() == Some("openqa.request"))
        .expect("an upstream span was exported");
    assert_eq!(span_trace_id(tool_span), span_trace_id(upstream_span));
    assert_eq!(span_parent_id(upstream_span), span_id(tool_span));
    assert_eq!(span_str_attr(tool_span, "outcome").as_deref(), Some("ok"));
    assert_eq!(
        span_str_attr(upstream_span, "url.path").as_deref(),
        Some("/api/v1/jobs/1")
    );

    unsafe { std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT") };
}

/// `get_job_log` reaches the wire only through `tools::artifact::execute`,
/// which cannot reach `request_json`; the same two-span shape must still
/// come out.
#[allow(
    unsafe_code,
    reason = "edition 2024 requires unsafe for std::env::set_var"
)]
async fn two_span_trace_through_artifact_execute() {
    let openqa = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("a log line\n"))
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
    let traces = telemetry.trace_producer();
    let client = run_server(&openqa, traces).await;

    call(
        &client,
        "get_job_log",
        json!({"job_id": 1, "filename": "autoinst-log.txt"}),
        None,
    )
    .await
    .expect("call_tool");

    telemetry.shutdown().await;

    let requests = requests_to(&collector, "/v1/traces").await;
    assert_eq!(requests.len(), 2);
    let spans = decode_spans(&requests[1].body);
    let tool_span = spans
        .iter()
        .find(|s| span_name(s).as_deref() == Some("mcp.tool/get_job_log"))
        .expect("a tool span was exported");
    let upstream_span = spans
        .iter()
        .find(|s| span_name(s).as_deref() == Some("openqa.request"))
        .expect("an upstream span was exported");
    assert_eq!(span_trace_id(tool_span), span_trace_id(upstream_span));
    assert_eq!(span_parent_id(upstream_span), span_id(tool_span));

    unsafe { std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT") };
}

/// A client-supplied `traceparent`'s trace id is adopted onto the exported
/// span, with `parent_span_id` set to the traceparent's own span id.
#[allow(
    unsafe_code,
    reason = "edition 2024 requires unsafe for std::env::set_var"
)]
async fn client_traceparent_is_adopted_onto_the_exported_span() {
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
    let client = run_server(&openqa, telemetry.trace_producer()).await;

    let traceparent = "00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba902b7-01";
    call(&client, "get_job", json!({"job_id": 1}), Some(traceparent))
        .await
        .expect("call_tool");

    telemetry.shutdown().await;

    let requests = requests_to(&collector, "/v1/traces").await;
    let spans = decode_spans(&requests[1].body);
    let tool_span = spans
        .iter()
        .find(|s| span_name(s).as_deref() == Some("mcp.tool/get_job"))
        .expect("a tool span was exported");
    let expected_trace_id: Vec<u8> = hex_decode("0af7651916cd43dd8448eb211c80319c");
    let expected_parent_id: Vec<u8> = hex_decode("00f067aa0ba902b7");
    assert_eq!(span_trace_id(tool_span), Some(expected_trace_id));
    assert_eq!(span_parent_id(tool_span), Some(expected_parent_id));

    unsafe { std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT") };
}

/// A malformed `traceparent` never fails the call and never carries through
/// to the exported span: the call still gets a root span (a fresh trace id,
/// no `parent_span_id`).
#[allow(
    unsafe_code,
    reason = "edition 2024 requires unsafe for std::env::set_var"
)]
async fn malformed_traceparent_falls_back_to_a_root_span() {
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
    let client = run_server(&openqa, telemetry.trace_producer()).await;

    let result = call(
        &client,
        "get_job",
        json!({"job_id": 1}),
        Some("not-a-traceparent"),
    )
    .await
    .expect("a malformed traceparent must not fail the call");
    assert_ne!(result.is_error, Some(true));

    telemetry.shutdown().await;

    let requests = requests_to(&collector, "/v1/traces").await;
    let spans = decode_spans(&requests[1].body);
    let tool_span = spans
        .iter()
        .find(|s| span_name(s).as_deref() == Some("mcp.tool/get_job"))
        .expect("a root span was still exported");
    assert!(span_parent_id(tool_span).is_none(), "must be a root span");

    unsafe { std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT") };
}

/// `OTEL_TRACES_SAMPLER=parentbased_always_on` with an unsampled parent
/// (`traceparent` flags `00`) exports no span, but the call still runs and
/// answers normally.
#[allow(
    unsafe_code,
    reason = "edition 2024 requires unsafe for std::env::set_var"
)]
async fn parentbased_always_on_respects_an_unsampled_parent() {
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
        std::env::set_var("OTEL_TRACES_SAMPLER", "parentbased_always_on");
    }

    let telemetry = Telemetry::init()
        .await
        .expect("probe succeeds")
        .expect("OTEL_EXPORTER_OTLP_ENDPOINT is set");
    let client = run_server(&openqa, telemetry.trace_producer()).await;

    let unsampled = "00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba902b7-00";
    let result = call(&client, "get_job", json!({"job_id": 1}), Some(unsampled))
        .await
        .expect("the call must still run and answer normally");
    assert_ne!(result.is_error, Some(true));

    telemetry.shutdown().await;

    let requests = requests_to(&collector, "/v1/traces").await;
    // Only the startup probe: no span for the unsampled call.
    assert_eq!(requests.len(), 1);

    unsafe {
        std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        std::env::remove_var("OTEL_TRACES_SAMPLER");
    }
}

/// An unsampled call still runs and still writes an audit record — only the
/// span export is skipped, and that record carries no `trace`/`span`.
#[allow(
    unsafe_code,
    reason = "edition 2024 requires unsafe for std::env::set_var"
)]
async fn unsampled_call_still_writes_an_audit_record() {
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
        std::env::set_var("OTEL_TRACES_SAMPLER", "parentbased_always_on");
    }

    let telemetry = Telemetry::init()
        .await
        .expect("probe succeeds")
        .expect("OTEL_EXPORTER_OTLP_ENDPOINT is set");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("audit.jsonl");
    let cfg =
        ruoqa_mcp::audit::AuditConfig::parse(&format!("path = {:?}\n", path.to_str().unwrap()))
            .expect("parse audit config");
    let auditor = Arc::new(Auditor::open(&cfg).expect("open audit sink"));
    let client = run_server_with_audit(&openqa, telemetry.trace_producer(), Some(auditor)).await;

    let unsampled = "00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba902b7-00";
    call(&client, "get_job", json!({"job_id": 1}), Some(unsampled))
        .await
        .expect("the call must still run");

    telemetry.shutdown().await;

    let text = std::fs::read_to_string(&path).expect("audit file written");
    let record = tool_call_record(&text);
    assert!(
        record.get("trace").is_none(),
        "unsampled call carries no trace id"
    );

    unsafe {
        std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        std::env::remove_var("OTEL_TRACES_SAMPLER");
    }
}

/// A client-supplied `traceparent`'s trace id is byte-identical across the
/// exported span and the audit record's own `trace`/`span` fields — the
/// audit bridge passes through the same `SpanCtx` `call_tool` built, rather
/// than re-deriving it.
#[allow(
    unsafe_code,
    reason = "edition 2024 requires unsafe for std::env::set_var"
)]
async fn traceparent_matches_byte_for_byte_on_span_and_audit_record() {
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
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("audit.jsonl");
    let cfg =
        ruoqa_mcp::audit::AuditConfig::parse(&format!("path = {:?}\n", path.to_str().unwrap()))
            .expect("parse audit config");
    let auditor = Arc::new(Auditor::open(&cfg).expect("open audit sink"));
    let client = run_server_with_audit(&openqa, telemetry.trace_producer(), Some(auditor)).await;

    let traceparent = "00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba902b7-01";
    call(&client, "get_job", json!({"job_id": 1}), Some(traceparent))
        .await
        .expect("call_tool");

    telemetry.shutdown().await;

    let text = std::fs::read_to_string(&path).expect("audit file written");
    let record = tool_call_record(&text);
    let audit_trace = record["trace"]
        .as_str()
        .expect("trace field present")
        .to_string();
    let audit_span = record["span"]
        .as_str()
        .expect("span field present")
        .to_string();
    assert_eq!(audit_trace, "0af7651916cd43dd8448eb211c80319c");

    let requests = requests_to(&collector, "/v1/traces").await;
    let spans = decode_spans(&requests[1].body);
    let tool_span = spans
        .iter()
        .find(|s| span_name(s).as_deref() == Some("mcp.tool/get_job"))
        .expect("a tool span was exported");
    assert_eq!(span_trace_id(tool_span), Some(hex_decode(&audit_trace)));
    assert_eq!(span_id(tool_span), Some(hex_decode(&audit_span)));

    unsafe { std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT") };
}

/// `OTEL_TRACES_EXPORTER=none` with a base endpoint set
/// produces zero requests to `/v1/traces` (including the startup probe),
/// while the logs signal keeps working.
#[allow(
    unsafe_code,
    reason = "edition 2024 requires unsafe for std::env::set_var"
)]
async fn traces_exporter_none_leaves_logs_working() {
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
        .expect("the logs signal is still configured");
    assert!(telemetry.trace_producer().is_none(), "traces must be off");
    telemetry.shutdown().await;

    let trace_requests = requests_to(&collector, "/v1/traces").await;
    assert!(trace_requests.is_empty(), "no request to /v1/traces at all");
    let log_requests = requests_to(&collector, "/v1/logs").await;
    assert_eq!(log_requests.len(), 1, "the logs startup probe still ran");

    unsafe {
        std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        std::env::remove_var("OTEL_TRACES_EXPORTER");
    }
}

/// The `event: "tool_call"` line in a JSONL audit file — skips the
/// `session_open` line `initialize` writes first.
fn tool_call_record(text: &str) -> Value {
    text.lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|record| record["event"] == "tool_call")
        .expect("a tool_call record was written")
}

fn hex_decode(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}
