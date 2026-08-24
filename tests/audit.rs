//! Integration tests for the JSONL audit stream: reuses `tests/routing.rs`'s
//! mock-backed-client-over-`tokio::io::duplex` harness, adding `.with_audit`
//! pointed at a `tempfile::TempDir`.

use std::sync::Arc;

use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use ruoqa_mcp::audit::{AuditConfig, Auditor, Transport};
use ruoqa_mcp::{OpenQaServer, ServerRegistry};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_SERVER: &str = "test";

/// An `OpenQaServer` backed by `mock`, auditing to a file under `dir`.
async fn run_audited_server(
    mock: &MockServer,
    dir: &std::path::Path,
) -> RunningService<RoleClient, ()> {
    let client = ruoqa::ClientBuilder::new()
        .server(mock.uri())
        .config_paths(vec![])
        .build()
        .expect("build client");
    let mut clients = std::collections::HashMap::new();
    clients.insert(TEST_SERVER.to_string(), client);

    let audit_path = dir.join("audit.jsonl");
    let cfg_text = format!("path = {:?}\n", audit_path.to_str().unwrap());
    let cfg = AuditConfig::parse(&cfg_text).expect("parse audit config");
    let auditor = Arc::new(Auditor::open(&cfg).expect("open audit sink"));

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

async fn call(
    client: &RunningService<RoleClient, ()>,
    name: &str,
    args: Value,
) -> Result<CallToolResult, rmcp::service::ServiceError> {
    let mut obj = args.as_object().cloned().unwrap_or_default();
    obj.entry("server".to_string())
        .or_insert_with(|| json!(TEST_SERVER));
    let params = CallToolRequestParams::new(name.to_string()).with_arguments(obj);
    client.peer().call_tool(params).await
}

fn audit_lines(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("read audit file")
        .lines()
        .map(|line| serde_json::from_str(line).expect("audit line is valid JSON"))
        .collect()
}

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
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs/404"))
        .respond_with(ResponseTemplate::new(404).set_body_string("Job 404 does not exist"))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/tests/7/file/autoinst-log.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(4 * 1024 * 1024)))
        .mount(&mock)
        .await;
    mock
}

#[tokio::test]
async fn session_open_is_first_and_tool_calls_are_gapless_in_order() {
    let mock = mock_openqa().await;
    let dir = tempfile::tempdir().unwrap();
    let client = run_audited_server(&mock, dir.path()).await;

    call(&client, "get_job", json!({"job_id": 7}))
        .await
        .unwrap();
    call(&client, "get_job", json!({"job_id": 7}))
        .await
        .unwrap();
    call(
        &client,
        "add_job_comment",
        json!({"job_id": 7, "text": "hi"}),
    )
    .await
    .unwrap();

    let lines = audit_lines(&dir.path().join("audit.jsonl"));
    assert_eq!(
        lines[0]["event"], "session_open",
        "session_open must be first"
    );

    let seqs: Vec<u64> = lines.iter().map(|l| l["seq"].as_u64().unwrap()).collect();
    assert_eq!(seqs, vec![1, 2, 3, 4], "seq must be gapless from 1");
    for line in &lines {
        assert_eq!(line["v"], 1);
    }

    let tool_calls: Vec<&Value> = lines.iter().filter(|l| l["event"] == "tool_call").collect();
    assert_eq!(tool_calls.len(), 3, "one line per call, in call order");
    assert_eq!(tool_calls[0]["tool"], "get_job");
    assert_eq!(tool_calls[1]["tool"], "get_job");
    assert_eq!(tool_calls[2]["tool"], "add_job_comment");
}

#[tokio::test]
async fn add_job_comment_carries_text_verbatim_and_a_write_ok_outcome() {
    let mock = mock_openqa().await;
    let dir = tempfile::tempdir().unwrap();
    let client = run_audited_server(&mock, dir.path()).await;

    call(
        &client,
        "add_job_comment",
        json!({"job_id": 7, "text": "label:force_result:passed:bsc#1234567"}),
    )
    .await
    .unwrap();

    // `server` in the arguments is the selector ("test"); the audit record
    // carries the *resolved* id (the mock's own host:port), per A8.
    let mock_url = url::Url::parse(&mock.uri()).unwrap();
    let resolved_id = format!(
        "{}:{}",
        mock_url.host_str().unwrap(),
        mock_url.port().unwrap()
    );

    let lines = audit_lines(&dir.path().join("audit.jsonl"));
    let record = lines
        .iter()
        .find(|l| l["event"] == "tool_call")
        .expect("one tool_call line");
    assert_eq!(
        record["args"]["text"],
        "label:force_result:passed:bsc#1234567"
    );
    assert_eq!(record["server"], resolved_id);
    assert_eq!(record["scope"], "write");
    assert_eq!(record["outcome"], "ok");
    assert!(record["duration_ms"].is_u64());
}

#[tokio::test]
async fn a_404_tool_error_is_recorded_with_kind_and_status() {
    let mock = mock_openqa().await;
    let dir = tempfile::tempdir().unwrap();
    let client = run_audited_server(&mock, dir.path()).await;

    let result = call(&client, "get_job", json!({"job_id": 404}))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));

    let lines = audit_lines(&dir.path().join("audit.jsonl"));
    let record = lines
        .iter()
        .find(|l| l["event"] == "tool_call")
        .expect("one tool_call line");
    assert_eq!(
        record["outcome"],
        json!({"tool_error": {"kind": "not_found", "status": 404}})
    );
}

#[tokio::test]
async fn a_multi_mib_result_produces_a_small_record() {
    let mock = mock_openqa().await;
    let dir = tempfile::tempdir().unwrap();
    let client = run_audited_server(&mock, dir.path()).await;

    call(
        &client,
        "get_job_log",
        json!({"job_id": 7, "filename": "autoinst-log.txt"}),
    )
    .await
    .unwrap();

    let text = std::fs::read_to_string(dir.path().join("audit.jsonl")).unwrap();
    let last_line = text.lines().next_back().expect("at least one line");
    assert!(
        last_line.len() < 500,
        "record should be a few hundred bytes, got {} bytes",
        last_line.len()
    );
    let record: Value = serde_json::from_str(last_line).unwrap();
    assert_eq!(record["tool"], "get_job_log");
}

#[tokio::test]
async fn an_unknown_server_selector_is_a_protocol_error_with_no_top_level_server() {
    let mock = mock_openqa().await;
    let dir = tempfile::tempdir().unwrap();
    let client = run_audited_server(&mock, dir.path()).await;

    let params = CallToolRequestParams::new("get_job".to_string()).with_arguments(
        json!({"job_id": 7, "server": "nope"})
            .as_object()
            .unwrap()
            .clone(),
    );
    let error = client
        .peer()
        .call_tool(params)
        .await
        .expect_err("unknown server must be a protocol-level error");
    let rmcp::service::ServiceError::McpError(mcp_error) = error else {
        panic!("expected an McpError, got {error:?}");
    };
    assert_eq!(mcp_error.code.0, -32602);

    let lines = audit_lines(&dir.path().join("audit.jsonl"));
    let record = lines
        .iter()
        .find(|l| l["event"] == "tool_call")
        .expect("one tool_call line");
    assert_eq!(
        record["outcome"],
        json!({"protocol_error": {"code": -32602}})
    );
    assert!(
        record.get("server").is_none(),
        "unresolved server must be absent"
    );
    assert_eq!(record["args"]["server"], "nope");
}

#[tokio::test]
async fn no_audit_config_creates_no_file() {
    let mock = mock_openqa().await;
    let dir = tempfile::tempdir().unwrap();
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

    call(&client, "get_job", json!({"job_id": 7}))
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        0,
        "no audit file must appear anywhere when auditing is unconfigured"
    );
}
