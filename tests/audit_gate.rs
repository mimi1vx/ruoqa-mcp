//! Integration tests for the audit fail-closed gate: a `call_tool` is
//! refused per `fail_mode` while persistence is failing, and it recovers
//! once a write succeeds again — whether persistence is a file sink or the
//! audit stream's own OTLP delivery health.
//!
//! Deterministic failure injection with no test-only hook: `rotate_max_bytes`
//! is set small enough that any write past the first needs `rotate()`, and a
//! read-only *directory* makes `rotate()`'s `std::fs::rename` fail with a
//! real `EACCES` — the already-open fd keeps writing fine, only the rename
//! is denied. Every scenario using this trick is `#[cfg(unix)]` and skips
//! under root, which ignores directory mode bits.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use ruoqa_mcp::audit::{AuditConfig, Auditor, Transport};
use ruoqa_mcp::{OpenQaServer, ServerRegistry, Telemetry};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_SERVER: &str = "test";

/// A mounted openQA endpoint serving the calls exercised below.
async fn mock_openqa() -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"job": {"id": 7}})))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/jobs/7/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1})))
        .mount(&mock)
        .await;
    mock
}

async fn run_server(mock: &MockServer, auditor: Arc<Auditor>) -> RunningService<RoleClient, ()> {
    let client = ruoqa::ClientBuilder::new()
        .server(mock.uri())
        .config_paths(vec![])
        .build()
        .expect("build client");
    let mut clients = std::collections::HashMap::new();
    clients.insert(TEST_SERVER.to_string(), client);

    let server = OpenQaServer::new(ServerRegistry::from_map(clients), false)
        .with_audit(Some(auditor))
        .with_transport(Transport::Stdio);

    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_transport).await {
            let _ = running.waiting().await;
        }
    });

    ().serve(client_transport).await.expect("client handshake")
}

async fn call(client: &RunningService<RoleClient, ()>, name: &str, args: Value) -> CallToolResult {
    let mut obj = args.as_object().cloned().unwrap_or_default();
    obj.entry("server".to_string())
        .or_insert_with(|| json!(TEST_SERVER));
    let params = CallToolRequestParams::new(name.to_string()).with_arguments(obj);
    client
        .peer()
        .call_tool(params)
        .await
        .expect("call_tool transport ok")
}

/// The `error.kind` of a refused (or otherwise failed) tool call's payload.
fn error_kind(result: &CallToolResult) -> Option<String> {
    if result.is_error != Some(true) {
        return None;
    }
    let ContentBlock::Text(text) = &result.content[0] else {
        return None;
    };
    let payload: Value = serde_json::from_str(&text.text).ok()?;
    payload["error"]["kind"].as_str().map(str::to_string)
}

fn audit_lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("read audit file")
        .lines()
        .map(|line| serde_json::from_str(line).expect("audit line is valid JSON"))
        .collect()
}

/// Build an [`Auditor`] at `path` with the given `fail_mode`, resolved for
/// stdio. `rotate_max_bytes` is small enough that, once the file already
/// holds the `initialize` handshake's `session_open` line, the *next* write
/// of any size needs `rotate()` — the lever a locked directory pulls.
fn open_auditor(path: &Path, fail_mode: &str) -> Arc<Auditor> {
    let cfg_text = format!(
        "path = {:?}\nfail_mode = \"{fail_mode}\"\nrotate_max_bytes = 4096\nrotate_keep = 3\n",
        path.to_str().unwrap()
    );
    let cfg = AuditConfig::parse(&cfg_text).expect("parse audit config");
    let fail_mode = cfg.fail_mode_for(Transport::Stdio);
    Arc::new(
        Auditor::open(&cfg)
            .expect("open audit sink")
            .with_fail_mode(fail_mode),
    )
}

/// `true` (after printing why) when running as root, which ignores
/// directory mode bits and would silently turn the rotation trick into a
/// no-op rather than a real permission failure.
#[cfg(unix)]
#[allow(unsafe_code, reason = "geteuid reads process state, no preconditions")]
fn skip_if_root() -> bool {
    // SAFETY: reads process state, no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipping: root ignores directory mode bits");
        true
    } else {
        false
    }
}

/// Locks `dir`, primes exactly one persistence failure (an oversized write
/// that forces a denied `rotate()`), then makes one `tool` call and checks
/// whether it was refused with `kind = "audit_unavailable"`. Unlocks `dir`
/// before returning either way, for `TempDir`'s own cleanup.
#[cfg(unix)]
async fn assert_gate_outcome(fail_mode: &str, tool: &str, args: Value, should_refuse: bool) {
    use std::os::unix::fs::PermissionsExt;

    let mock = mock_openqa().await;
    let dir = tempfile::tempdir().unwrap();
    let audit_path = dir.path().join("audit.jsonl");
    let auditor = open_auditor(&audit_path, fail_mode);
    let client = run_server(&mock, auditor).await;

    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    let big_text = "x".repeat(5000);
    call(
        &client,
        "add_job_comment",
        json!({"job_id": 7, "text": big_text}),
    )
    .await;

    let result = call(&client, tool, args).await;

    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

    if should_refuse {
        assert_eq!(
            result.is_error,
            Some(true),
            "{fail_mode}/{tool} must be refused"
        );
        assert_eq!(
            error_kind(&result).as_deref(),
            Some("audit_unavailable"),
            "{fail_mode}/{tool}"
        );
    } else {
        assert_ne!(
            result.is_error,
            Some(true),
            "{fail_mode}/{tool} must not be refused"
        );
    }
}

/// The full (`fail_mode` × scope) matrix from the gate's boundaries: `open`
/// never refuses, `closed_writes` refuses only the write tool, `closed_all`
/// refuses both — all while persistence is actively failing.
#[cfg(unix)]
#[tokio::test]
async fn gate_refuses_per_fail_mode_and_scope() {
    if skip_if_root() {
        return;
    }
    assert_gate_outcome("open", "get_job", json!({"job_id": 7}), false).await;
    assert_gate_outcome(
        "open",
        "add_job_comment",
        json!({"job_id": 7, "text": "hi"}),
        false,
    )
    .await;

    assert_gate_outcome("closed_writes", "get_job", json!({"job_id": 7}), false).await;
    assert_gate_outcome(
        "closed_writes",
        "add_job_comment",
        json!({"job_id": 7, "text": "hi"}),
        true,
    )
    .await;

    assert_gate_outcome("closed_all", "get_job", json!({"job_id": 7}), true).await;
    assert_gate_outcome(
        "closed_all",
        "add_job_comment",
        json!({"job_id": 7, "text": "hi"}),
        true,
    )
    .await;
}

/// Recovery: once a write succeeds again, exactly one `audit_gap` line is
/// appended, `count` matches the failed appends, `refused` matches the
/// refused calls during the outage, and no further call is refused or
/// produces a second gap line.
#[cfg(unix)]
#[tokio::test]
async fn recovery_emits_one_gap_record_then_stops_refusing() {
    use std::os::unix::fs::PermissionsExt;

    if skip_if_root() {
        return;
    }

    let mock = mock_openqa().await;
    let dir = tempfile::tempdir().unwrap();
    let audit_path = dir.path().join("audit.jsonl");
    let auditor = open_auditor(&audit_path, "closed_all");
    let client = run_server(&mock, Arc::clone(&auditor)).await;

    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    let big_text = "x".repeat(5000);
    // One priming failure: not yet refused (failing was false), but its own
    // append fails, setting `failing`.
    call(
        &client,
        "add_job_comment",
        json!({"job_id": 7, "text": big_text}),
    )
    .await;

    // Refused per the pre-existing `failing` state; its own (small) audit
    // record needs no rotation, so this same call's write is what recovers.
    let recovering = call(&client, "get_job", json!({"job_id": 7})).await;
    assert_eq!(recovering.is_error, Some(true));
    assert_eq!(
        error_kind(&recovering).as_deref(),
        Some("audit_unavailable")
    );

    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();

    let lines = audit_lines(&audit_path);
    let gap_lines: Vec<&Value> = lines.iter().filter(|l| l["event"] == "audit_gap").collect();
    assert_eq!(gap_lines.len(), 1, "exactly one audit_gap line: {lines:?}");
    let gap = gap_lines[0];
    assert_eq!(gap["count"], 1, "one failed append: the priming write");
    assert_eq!(
        gap["refused"], 1,
        "one refused call: the recovering one itself"
    );
    assert!(gap["since"].is_string());

    // Now healthy: a further call is not refused, and no second gap line
    // appears.
    let after = call(&client, "get_job", json!({"job_id": 7})).await;
    assert_ne!(after.is_error, Some(true));
    let lines_after = audit_lines(&audit_path);
    let gap_lines_after = lines_after
        .iter()
        .filter(|l| l["event"] == "audit_gap")
        .count();
    assert_eq!(gap_lines_after, 1, "no second gap line while healthy");
}

/// With no audit config at all, there is nothing to gate: a call is never
/// refused, and this holds independent of any fail mode (there is no
/// `Auditor` to hold one).
#[tokio::test]
async fn no_audit_config_never_refuses() {
    let mock = mock_openqa().await;
    let client = ruoqa::ClientBuilder::new()
        .server(mock.uri())
        .config_paths(vec![])
        .build()
        .expect("build client");
    let mut clients = std::collections::HashMap::new();
    clients.insert(TEST_SERVER.to_string(), client);
    let server = OpenQaServer::new(ServerRegistry::from_map(clients), false);

    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_transport).await {
            let _ = running.waiting().await;
        }
    });
    let client = ().serve(client_transport).await.expect("client handshake");

    let result = call(&client, "get_job", json!({"job_id": 7})).await;
    assert_ne!(result.is_error, Some(true));
}

/// `path = "none"` with an OTLP collector: delivery health, not a file
/// sink, is the persistence signal the gate reads. A collector that starts
/// failing engages the gate; once it recovers, the next successful export
/// clears it. One `#[tokio::test]` for the whole scenario: `OTEL_*` is
/// process environment, shared by every test in this binary.
#[tokio::test]
#[allow(
    unsafe_code,
    reason = "edition 2024 requires unsafe for std::env::set_var"
)]
async fn path_none_collector_failure_engages_gate_then_recovers() {
    let collector = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&collector)
        .await;

    // SAFETY: this whole file has no other test touching these variables.
    unsafe {
        std::env::remove_var("RUST_LOG");
        std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", collector.uri());
        std::env::set_var("OTEL_TRACES_EXPORTER", "none");
        std::env::set_var("OTEL_METRICS_EXPORTER", "none");
        std::env::set_var("OTEL_BLRP_SCHEDULE_DELAY", "10");
        std::env::set_var("OTEL_BLRP_MAX_EXPORT_BATCH_SIZE", "1");
    }

    let telemetry = Telemetry::init_with_audit_stream(true)
        .await
        .expect("probe succeeds")
        .expect("OTEL_EXPORTER_OTLP_ENDPOINT is set");
    let audit_producer = telemetry
        .audit_producer()
        .expect("the audit stream is configured");

    let cfg = AuditConfig::parse("path = \"none\"\nfail_mode = \"closed_all\"\n").unwrap();
    let fail_mode = cfg.fail_mode_for(Transport::Stdio);
    let auditor = Arc::new(
        Auditor::open(&cfg)
            .unwrap()
            .with_otlp(audit_producer)
            .with_fail_mode(fail_mode),
    );

    let mock = mock_openqa().await;
    let client = run_server(&mock, Arc::clone(&auditor)).await;

    let baseline = call(&client, "get_job", json!({"job_id": 7})).await;
    assert_ne!(baseline.is_error, Some(true));

    // The collector starts rejecting every export.
    collector.reset().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&collector)
        .await;

    // Two calls: the first's write observes stale (healthy) delivery
    // health and enqueues a record whose export then fails in the
    // background; the second's write is the one that reads the now-false
    // health and sets `failing`.
    call(&client, "get_job", json!({"job_id": 7})).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    call(&client, "get_job", json!({"job_id": 7})).await;

    let refused = call(&client, "get_job", json!({"job_id": 7})).await;
    assert_eq!(refused.is_error, Some(true));
    assert_eq!(error_kind(&refused).as_deref(), Some("audit_unavailable"));

    // The collector recovers.
    collector.reset().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&collector)
        .await;

    call(&client, "get_job", json!({"job_id": 7})).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Refused per the still-stale `failing` state, but this call's own
    // write now observes a healthy export and clears it.
    let recovering = call(&client, "get_job", json!({"job_id": 7})).await;
    assert_eq!(recovering.is_error, Some(true));

    let healthy = call(&client, "get_job", json!({"job_id": 7})).await;
    assert_ne!(healthy.is_error, Some(true));

    telemetry.shutdown().await;

    unsafe {
        std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        std::env::remove_var("OTEL_TRACES_EXPORTER");
        std::env::remove_var("OTEL_METRICS_EXPORTER");
        std::env::remove_var("OTEL_BLRP_SCHEDULE_DELAY");
        std::env::remove_var("OTEL_BLRP_MAX_EXPORT_BATCH_SIZE");
    }
}
