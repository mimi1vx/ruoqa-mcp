//! wiremock-backed port of `tests/test_tools.py`. Drives `OpenQaServer` over
//! an in-memory MCP session (a `tokio::io::duplex` pair) so tool calls go
//! through the real router/handler, not internal shortcuts.

use std::time::Duration;

use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use ruoqa_mcp::OpenQaServer;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

async fn server_with_mock(mock: &MockServer, readonly: bool) -> RunningService<RoleClient, ()> {
    server_with_client(mock, readonly, None, None).await
}

async fn server_with_client(
    mock: &MockServer,
    readonly: bool,
    api_key: Option<&str>,
    api_secret: Option<&str>,
) -> RunningService<RoleClient, ()> {
    run_server(mock, readonly, api_key, api_secret, None).await
}

async fn server_with_call_timeout(
    mock: &MockServer,
    timeout: Duration,
) -> RunningService<RoleClient, ()> {
    run_server(mock, false, None, None, Some(timeout)).await
}

async fn run_server(
    mock: &MockServer,
    readonly: bool,
    api_key: Option<&str>,
    api_secret: Option<&str>,
    call_timeout: Option<Duration>,
) -> RunningService<RoleClient, ()> {
    run_server_at(&mock.uri(), readonly, api_key, api_secret, call_timeout).await
}

/// Same as `run_server`, but against an arbitrary `server` URL rather than a
/// live `MockServer` — for cases (e.g. connection refused) where there is no
/// listener at all.
async fn run_server_at(
    server: &str,
    readonly: bool,
    api_key: Option<&str>,
    api_secret: Option<&str>,
    call_timeout: Option<Duration>,
) -> RunningService<RoleClient, ()> {
    let mut builder = ruoqa::ClientBuilder::new()
        .server(server)
        .config_paths(vec![]);
    if let (Some(key), Some(secret)) = (api_key, api_secret) {
        builder = builder
            .api_key(ruoqa::ApiKey::new(key))
            .api_secret(ruoqa::ApiSecret::new(secret));
    }
    let client = builder.build().expect("build client");
    let mut server = OpenQaServer::new(client, readonly);
    if let Some(timeout) = call_timeout {
        server = server.with_call_timeout(Some(timeout));
    }

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
    let mut params = CallToolRequestParams::new(name.to_string());
    if let Some(obj) = args.as_object() {
        params = params.with_arguments(obj.clone());
    }
    client.peer().call_tool(params).await
}

fn text(result: &rmcp::model::CallToolResult) -> Value {
    let rmcp::model::ContentBlock::Text(text) = &result.content[0] else {
        panic!("expected text content block");
    };
    serde_json::from_str(&text.text).expect("tool result is valid JSON")
}

#[tokio::test]
async fn list_jobs_drops_none_and_expands_ids() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"jobs": [{"id": 42}]})))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, false).await;

    let result = call(
        &client,
        "list_jobs",
        json!({"state": "done", "arch": "x86_64", "ids": [1, 2]}),
    )
    .await
    .expect("call_tool");

    let requests = mock.received_requests().await.expect("recorded requests");
    let request = requests.last().expect("one request");
    let query = request.url.query().unwrap_or("");
    // None filters (result, distri, ...) must not appear; ids expand to repeated keys.
    assert_eq!(query, "state=done&arch=x86_64&ids=1&ids=2");
    assert_eq!(text(&result), json!({"jobs": [{"id": 42}]}));
}

#[tokio::test]
async fn list_jobs_summary_returns_compact_shape() {
    let mock = MockServer::start().await;
    let jobs = json!([
        {"id": 1, "test": "boot", "result": "passed", "state": "done", "settings": {"ARCH": "x86_64"}},
        {"id": 5, "test": "wip", "result": "none", "state": "running", "settings": {"ARCH": "x86_64"}},
    ]);
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"jobs": jobs})))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, false).await;

    let result = call(&client, "list_jobs", json!({"summary": true}))
        .await
        .expect("call_tool");
    let summary = text(&result);

    assert_eq!(summary["total"], 2);
    assert_eq!(summary["by_result"], json!({"passed": 1, "none": 1}));
    assert_eq!(summary["jobs"]["running"][0]["id"], 5);
    assert!(summary["jobs"].get("none").is_none());
}

#[tokio::test]
async fn list_jobs_overview_summary_accepts_bare_array() {
    let mock = MockServer::start().await;
    let jobs = json!([{"id": 1, "test": "boot", "result": "passed", "state": "done"}]);
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs/overview"))
        .respond_with(ResponseTemplate::new(200).set_body_json(jobs))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, false).await;

    let result = call(&client, "list_jobs_overview", json!({"summary": true}))
        .await
        .expect("call_tool");
    let summary = text(&result);

    assert_eq!(summary["total"], 1);
    assert_eq!(summary["by_result"], json!({"passed": 1}));
}

async fn assert_summary_shape_rejected(tool: &str, upstream_path: &str, body: Value) {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(upstream_path))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, false).await;

    let err = call(&client, tool, json!({"summary": true}))
        .await
        .expect_err("malformed jobs shape must be rejected");

    let rmcp::ServiceError::McpError(mcp_err) = err else {
        panic!("expected McpError, got {err:?}");
    };
    assert_eq!(mcp_err.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
}

#[tokio::test]
async fn list_jobs_summary_rejects_missing_jobs_key() {
    assert_summary_shape_rejected("list_jobs", "/api/v1/jobs", json!({})).await;
}

#[tokio::test]
async fn list_jobs_summary_rejects_non_array_jobs() {
    assert_summary_shape_rejected("list_jobs", "/api/v1/jobs", json!({"jobs": "oops"})).await;
}

#[tokio::test]
async fn list_jobs_summary_rejects_unrelated_object() {
    assert_summary_shape_rejected("list_jobs", "/api/v1/jobs", json!({"error": "nope"})).await;
}

#[tokio::test]
async fn list_jobs_summary_rejects_top_level_array() {
    assert_summary_shape_rejected("list_jobs", "/api/v1/jobs", json!([{"id": 1}])).await;
}

#[tokio::test]
async fn list_jobs_summary_false_passes_malformed_body_through_unchanged() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"unexpected": true})))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, false).await;

    let result = call(&client, "list_jobs", json!({"summary": false}))
        .await
        .expect("summary=false must not validate shape");

    assert_eq!(text(&result), json!({"unexpected": true}));
}

#[tokio::test]
async fn list_jobs_overview_summary_rejects_missing_jobs_key() {
    assert_summary_shape_rejected("list_jobs_overview", "/api/v1/jobs/overview", json!({})).await;
}

#[tokio::test]
async fn list_jobs_overview_summary_rejects_non_array_jobs() {
    assert_summary_shape_rejected(
        "list_jobs_overview",
        "/api/v1/jobs/overview",
        json!({"jobs": "oops"}),
    )
    .await;
}

#[tokio::test]
async fn list_jobs_overview_summary_rejects_unrelated_object() {
    assert_summary_shape_rejected(
        "list_jobs_overview",
        "/api/v1/jobs/overview",
        json!({"error": "nope"}),
    )
    .await;
}

#[tokio::test]
async fn add_job_comment_is_form_encoded_not_json() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/jobs/7/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 100})))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, false).await;

    let result = call(
        &client,
        "add_job_comment",
        json!({"job_id": 7, "text": "looks flaky"}),
    )
    .await
    .expect("call_tool");

    let requests = mock.received_requests().await.expect("recorded requests");
    let request = requests.last().expect("one request");
    assert_eq!(
        request
            .headers
            .get("content-type")
            .map(|v| v.to_str().unwrap()),
        Some("application/x-www-form-urlencoded")
    );
    assert_eq!(String::from_utf8_lossy(&request.body), "text=looks+flaky");
    assert_eq!(text(&result), json!({"id": 100}));
}

#[tokio::test]
async fn restart_jobs_sends_one_request_with_repeated_jobs_key() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/jobs/restart"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": []})))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, false).await;

    call(&client, "restart_jobs", json!({"job_ids": [1, 2]}))
        .await
        .expect("call_tool");

    let requests = mock.received_requests().await.expect("recorded requests");
    assert_eq!(requests.len(), 1);
    let body = String::from_utf8_lossy(&requests.last().unwrap().body);
    assert_eq!(body, "jobs=1&jobs=2");
}

#[tokio::test]
async fn restart_jobs_partial_outcome_is_a_success() {
    let mock = MockServer::start().await;
    let upstream_body = json!({
        "result": [{"1": 11}],
        "errors": ["Job 2 misses the following mandatory assets: ..."],
        "enforceable": 1
    });
    Mock::given(method("POST"))
        .and(path("/api/v1/jobs/restart"))
        .respond_with(ResponseTemplate::new(200).set_body_json(upstream_body.clone()))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, false).await;

    let result = call(&client, "restart_jobs", json!({"job_ids": [1, 2]}))
        .await
        .expect("a partial upstream failure is still a successful tool call");

    assert_eq!(text(&result), upstream_body);
}

#[tokio::test]
async fn delete_job_normalizes_204_to_empty_object() {
    let mock = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/api/v1/jobs/7"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, false).await;

    let result = call(&client, "delete_job", json!({"job_id": 7}))
        .await
        .expect("call_tool");

    assert_eq!(text(&result), json!({}));
}

#[tokio::test]
async fn unauthenticated_write_403_becomes_error_without_secret() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/jobs/7/cancel"))
        .respond_with(ResponseTemplate::new(403).set_body_string("Forbidden"))
        .mount(&mock)
        .await;
    let client = server_with_client(&mock, false, Some("KEY"), Some("TOPSECRET")).await;

    let result = call(&client, "cancel_job", json!({"job_id": 7}))
        .await
        .expect("a 403 is a tool-level error, not a protocol error");

    assert_eq!(result.is_error, Some(true));
    let payload = text(&result);
    assert_eq!(payload["error"]["kind"], "forbidden");
    assert_eq!(payload["error"]["status"], 403);
    assert_eq!(payload["error"]["body"], "Forbidden");
    let raw = payload.to_string();
    assert!(
        !raw.contains("TOPSECRET"),
        "payload must not leak the API secret: {raw}"
    );
}

/// Table-driven: every `Request` status maps to its documented `kind`.
#[tokio::test]
async fn request_statuses_map_to_documented_kinds() {
    let cases = [
        (401u16, "unauthorized"),
        (403, "forbidden"),
        (404, "not_found"),
        (429, "rate_limited"),
        (500, "server_error"),
    ];
    for (status, kind) in cases {
        let mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/jobs/1"))
            .respond_with(ResponseTemplate::new(status).set_body_string("upstream said so"))
            .mount(&mock)
            .await;
        let client = server_with_mock(&mock, true).await;

        let result = call(&client, "get_job", json!({"job_id": 1}))
            .await
            .unwrap_or_else(|e| panic!("status {status} should be a tool-level error, got {e}"));

        assert_eq!(result.is_error, Some(true), "status {status}");
        let payload = text(&result);
        assert_eq!(payload["error"]["kind"], kind, "status {status}");
        assert_eq!(payload["error"]["status"], status, "status {status}");
        assert_eq!(
            payload["error"]["body"], "upstream said so",
            "status {status}"
        );
    }
}

#[tokio::test]
async fn connection_refused_becomes_a_connection_kind_error() {
    // Bind then immediately drop a listener: the port stays free of any
    // other process but nothing is listening, so a connect attempt is
    // refused deterministically without touching the network.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    drop(listener);
    let client = run_server_at(&format!("http://{addr}"), true, None, None, None).await;

    let result = call(&client, "get_job", json!({"job_id": 1}))
        .await
        .expect("a connection failure is a tool-level error, not a protocol error");

    assert_eq!(result.is_error, Some(true));
    assert_eq!(text(&result)["error"]["kind"], "connection");
}

#[tokio::test]
async fn oversized_body_becomes_a_response_too_large_kind_error() {
    let mock = MockServer::start().await;
    // Comfortably over ruoqa's default 32 MiB max_response_bytes.
    let huge = "x".repeat(33 * 1024 * 1024);
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs/1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(huge))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, true).await;

    let result = call(&client, "get_job", json!({"job_id": 1}))
        .await
        .expect("an oversized body is a tool-level error, not a protocol error");

    assert_eq!(result.is_error, Some(true));
    assert_eq!(text(&result)["error"]["kind"], "response_too_large");
}

#[tokio::test]
async fn garbage_body_becomes_an_invalid_response_kind_error() {
    let mock = MockServer::start().await;
    // A JSON content type with an unparsable body: BodyKind::Text would
    // swallow this silently, so the content type must say JSON to reach
    // ruoqa's Error::Parse.
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs/1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("not json { at all", "application/json"),
        )
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, true).await;

    let result = call(&client, "get_job", json!({"job_id": 1}))
        .await
        .expect("an unparsable body is a tool-level error, not a protocol error");

    assert_eq!(result.is_error, Some(true));
    assert_eq!(text(&result)["error"]["kind"], "invalid_response");
}

#[tokio::test]
async fn cancel_jobs_empty_filter_is_rejected() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/jobs/cancel"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": []})))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, false).await;

    let err = call(&client, "cancel_jobs", json!({}))
        .await
        .expect_err("empty filter must be rejected");

    let rmcp::ServiceError::McpError(mcp_err) = err else {
        panic!("expected McpError, got {err:?}");
    };
    assert_eq!(mcp_err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    assert!(
        mock.received_requests()
            .await
            .expect("recorded requests")
            .is_empty()
    );
}

#[tokio::test]
async fn cancel_jobs_blank_only_filter_is_rejected() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/jobs/cancel"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": []})))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, false).await;

    let err = call(&client, "cancel_jobs", json!({"state": "   "}))
        .await
        .expect_err("blank-only filter must be rejected");

    let rmcp::ServiceError::McpError(mcp_err) = err else {
        panic!("expected McpError, got {err:?}");
    };
    assert_eq!(mcp_err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    assert!(
        mock.received_requests()
            .await
            .expect("recorded requests")
            .is_empty()
    );
}

#[tokio::test]
async fn cancel_jobs_each_filter_alone_produces_expected_query() {
    let cases: [(&str, Value, &str); 10] = [
        ("state", json!("running"), "state=running"),
        ("result", json!("failed"), "result=failed"),
        ("distri", json!("opensuse"), "distri=opensuse"),
        ("version", json!("15.5"), "version=15.5"),
        ("build", json!("42"), "build=42"),
        ("test", json!("boot"), "test=boot"),
        ("arch", json!("x86_64"), "arch=x86_64"),
        ("machine", json!("64bit"), "machine=64bit"),
        ("groupid", json!(7), "groupid=7"),
        ("group", json!("staging"), "group=staging"),
    ];

    for (key, value, expected_query) in cases {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/jobs/cancel"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": []})))
            .mount(&mock)
            .await;
        let client = server_with_mock(&mock, false).await;

        let mut args = serde_json::Map::new();
        args.insert(key.to_string(), value);
        call(&client, "cancel_jobs", Value::Object(args))
            .await
            .unwrap_or_else(|e| panic!("cancel_jobs with {key} failed: {e}"));

        let requests = mock.received_requests().await.expect("recorded requests");
        let request = requests.last().expect("one request");
        assert_eq!(
            request.url.query().unwrap_or(""),
            expected_query,
            "filter: {key}"
        );
    }
}

#[tokio::test]
async fn cancel_jobs_drops_blank_filter_alongside_a_real_one() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/jobs/cancel"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": []})))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, false).await;

    call(
        &client,
        "cancel_jobs",
        json!({"state": "running", "arch": "  "}),
    )
    .await
    .expect("call_tool");

    let requests = mock.received_requests().await.expect("recorded requests");
    let request = requests.last().expect("one request");
    assert_eq!(request.url.query().unwrap_or(""), "state=running");
}

#[tokio::test]
async fn cancel_scheduled_product_valid_name_produces_expected_path() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": 1})))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, false).await;

    call(
        &client,
        "cancel_scheduled_product",
        json!({"name": "SLE-15-SP4-Online-x86_64-Build1.1-Media1.iso"}),
    )
    .await
    .expect("call_tool");

    let requests = mock.received_requests().await.expect("recorded requests");
    let request = requests.last().expect("one request");
    assert_eq!(
        request.url.path(),
        "/api/v1/isos/SLE-15-SP4-Online-x86_64-Build1.1-Media1.iso/cancel"
    );
}

#[tokio::test]
async fn cancel_scheduled_product_cannot_escape_the_isos_segment() {
    let cases = ["../jobs/7", "/", "%2f", "x?foo=bar", "x#frag", "ünïcode"];

    for name in cases {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": 1})))
            .mount(&mock)
            .await;
        let client = server_with_mock(&mock, false).await;

        call(&client, "cancel_scheduled_product", json!({"name": name}))
            .await
            .unwrap_or_else(|e| panic!("cancel_scheduled_product with {name:?} failed: {e}"));

        let requests = mock.received_requests().await.expect("recorded requests");
        let request = requests.last().expect("one request");
        let path = request.url.path();
        assert!(
            path.starts_with("/api/v1/isos/") && path.ends_with("/cancel"),
            "name {name:?}: path {path} escaped the isos/.../cancel shape"
        );
        let segment = &path["/api/v1/isos/".len()..path.len() - "/cancel".len()];
        assert!(
            !segment.contains('/'),
            "name {name:?}: segment {segment:?} contains an extra path separator"
        );
        assert_eq!(
            request.url.query(),
            None,
            "name {name:?}: request must carry no query string"
        );
        assert_ne!(
            path, "/api/v1/jobs/7/cancel",
            "name {name:?}: leaked into the jobs cancel endpoint"
        );
    }
}

#[tokio::test]
async fn cancel_scheduled_product_rejects_empty_and_dot_names() {
    for name in ["", ".", ".."] {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": 1})))
            .mount(&mock)
            .await;
        let client = server_with_mock(&mock, false).await;

        let err = call(&client, "cancel_scheduled_product", json!({"name": name}))
            .await
            .expect_err(&format!("name {name:?} must be rejected"));

        let rmcp::ServiceError::McpError(mcp_err) = err else {
            panic!("expected McpError, got {err:?}");
        };
        assert_eq!(mcp_err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(
            mock.received_requests()
                .await
                .expect("recorded requests")
                .is_empty(),
            "name {name:?} must send no HTTP request"
        );
    }
}

#[tokio::test]
async fn list_jobs_ids_over_limit_is_rejected_with_no_request() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"jobs": []})))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, false).await;

    let ids: Vec<i64> = (1..=501).collect();
    let err = call(&client, "list_jobs", json!({"ids": ids}))
        .await
        .expect_err("over-limit ids must be rejected");

    let rmcp::ServiceError::McpError(mcp_err) = err else {
        panic!("expected McpError, got {err:?}");
    };
    assert_eq!(mcp_err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    assert!(
        mock.received_requests()
            .await
            .expect("recorded requests")
            .is_empty()
    );
}

#[tokio::test]
async fn list_jobs_ids_at_limit_reaches_mock() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"jobs": []})))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, false).await;

    let ids: Vec<i64> = (1..=500).collect();
    call(&client, "list_jobs", json!({"ids": ids}))
        .await
        .expect("at-limit ids call should succeed");

    let requests = mock.received_requests().await.expect("recorded requests");
    assert_eq!(requests.len(), 1);
}

#[tokio::test]
async fn restart_jobs_over_limit_is_rejected_with_no_request() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": true})))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, false).await;

    let job_ids: Vec<i64> = (1..=501).collect();
    let err = call(&client, "restart_jobs", json!({"job_ids": job_ids}))
        .await
        .expect_err("over-limit job_ids must be rejected");

    let rmcp::ServiceError::McpError(mcp_err) = err else {
        panic!("expected McpError, got {err:?}");
    };
    assert_eq!(mcp_err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    assert!(
        !mcp_err.message.contains("restart_jobs_bulk"),
        "restart_jobs_bulk no longer exists, message should not name it: {}",
        mcp_err.message
    );
    assert!(
        mock.received_requests()
            .await
            .expect("recorded requests")
            .is_empty()
    );
}

#[tokio::test]
async fn restart_jobs_empty_is_rejected() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": true})))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, false).await;

    let err = call(&client, "restart_jobs", json!({"job_ids": []}))
        .await
        .expect_err("empty job_ids must be rejected");

    let rmcp::ServiceError::McpError(mcp_err) = err else {
        panic!("expected McpError, got {err:?}");
    };
    assert_eq!(mcp_err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    assert!(
        mock.received_requests()
            .await
            .expect("recorded requests")
            .is_empty()
    );
}

#[tokio::test]
async fn restart_jobs_at_limit_reaches_mock() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/jobs/restart"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": []})))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, false).await;

    let job_ids: Vec<i64> = (1..=500).collect();
    call(&client, "restart_jobs", json!({"job_ids": job_ids}))
        .await
        .expect("at-limit call should succeed");

    let requests = mock.received_requests().await.expect("recorded requests");
    assert_eq!(requests.len(), 1);
    let body = String::from_utf8_lossy(&requests.last().unwrap().body);
    assert_eq!(body.matches("jobs=").count(), 500);
}

fn extra_map(n: usize) -> Value {
    let map: serde_json::Map<String, Value> = (0..n)
        .map(|i| (format!("KEY{i}"), json!(format!("value{i}"))))
        .collect();
    Value::Object(map)
}

#[tokio::test]
async fn trigger_isos_extra_over_limit_is_rejected_with_no_request() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/isos"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1})))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, false).await;

    let err = call(
        &client,
        "trigger_isos",
        json!({
            "distri": "opensuse",
            "version": "15.5",
            "flavor": "DVD",
            "arch": "x86_64",
            "extra": extra_map(101),
        }),
    )
    .await
    .expect_err("over-limit extra must be rejected");

    let rmcp::ServiceError::McpError(mcp_err) = err else {
        panic!("expected McpError, got {err:?}");
    };
    assert_eq!(mcp_err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    assert!(
        mock.received_requests()
            .await
            .expect("recorded requests")
            .is_empty()
    );
}

#[tokio::test]
async fn trigger_isos_extra_at_limit_reaches_mock() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/isos"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1})))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, false).await;

    call(
        &client,
        "trigger_isos",
        json!({
            "distri": "opensuse",
            "version": "15.5",
            "flavor": "DVD",
            "arch": "x86_64",
            "extra": extra_map(100),
        }),
    )
    .await
    .expect("at-limit call should succeed");

    let requests = mock.received_requests().await.expect("recorded requests");
    assert_eq!(requests.len(), 1);
}

async fn assert_extra_rejected(extra: Value) {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/isos"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1})))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, false).await;

    let err = call(
        &client,
        "trigger_isos",
        json!({
            "distri": "opensuse",
            "version": "15.5",
            "flavor": "DVD",
            "arch": "x86_64",
            "extra": extra,
        }),
    )
    .await
    .expect_err("colliding extra key must be rejected");

    let rmcp::ServiceError::McpError(mcp_err) = err else {
        panic!("expected McpError, got {err:?}");
    };
    assert_eq!(mcp_err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    assert!(
        mock.received_requests()
            .await
            .expect("recorded requests")
            .is_empty()
    );
}

#[tokio::test]
async fn trigger_isos_extra_lowercase_reserved_key_is_rejected() {
    assert_extra_rejected(json!({"distri": "tumbleweed"})).await;
}

#[tokio::test]
async fn trigger_isos_extra_mixed_case_reserved_key_is_rejected() {
    assert_extra_rejected(json!({"Arch": "aarch64"})).await;
}

#[tokio::test]
async fn trigger_isos_extra_internal_case_collision_is_rejected() {
    assert_extra_rejected(json!({"foo": "1", "FOO": "2"})).await;
}

#[tokio::test]
async fn trigger_isos_extra_async_and_settings_still_reach_the_mock() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/isos"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1})))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, false).await;

    call(
        &client,
        "trigger_isos",
        json!({
            "distri": "opensuse",
            "version": "15.5",
            "flavor": "DVD",
            "arch": "x86_64",
            "extra": {"async": "1", "MY_SETTING": "x"},
        }),
    )
    .await
    .expect("async and ordinary settings are not reserved");

    let requests = mock.received_requests().await.expect("recorded requests");
    let body = String::from_utf8_lossy(&requests.last().unwrap().body);
    assert!(
        body.contains("async=1"),
        "body should contain async=1: {body}"
    );
    assert!(
        body.contains("MY_SETTING=x"),
        "body should contain MY_SETTING=x: {body}"
    );
}

#[tokio::test]
async fn call_exceeding_the_deadline_fails_instead_of_hanging() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"jobs": []}))
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&mock)
        .await;
    let client = server_with_call_timeout(&mock, Duration::from_millis(50)).await;

    let result = call(&client, "list_jobs", json!({}))
        .await
        .expect("the deadline is a tool-level error, not a protocol error");

    assert_eq!(result.is_error, Some(true));
    let payload = text(&result);
    assert_eq!(payload["error"]["kind"], "timeout");
    let message = payload["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("OPENQA_MCP_CALL_TIMEOUT"),
        "message should name the deadline variable: {message}"
    );
}

#[tokio::test]
async fn readonly_server_does_not_expose_mutating_tools() {
    let mock = MockServer::start().await;
    let client = server_with_mock(&mock, true).await;

    let err = call(&client, "delete_job", json!({"job_id": 1}))
        .await
        .expect_err("readonly server has no write tools");

    assert!(matches!(err, rmcp::ServiceError::McpError(_)));
}

// --- Job-artifact tools -----------------------------------------------

/// Parses a `Range: bytes=<start>-<end>` header value the way this crate's
/// `insert_range` builds it (either half may be absent).
fn parse_range(header: &str) -> Option<(Option<usize>, Option<usize>)> {
    let rest = header.strip_prefix("bytes=")?;
    let (start, end) = rest.split_once('-')?;
    let start = if start.is_empty() {
        None
    } else {
        start.parse().ok()
    };
    let end = if end.is_empty() {
        None
    } else {
        end.parse().ok()
    };
    Some((start, end))
}

/// A fake `Mojolicious::Static`-style file server, replicating its Range
/// handling *including* the documented suffix-range bug (a `bytes=-N`
/// request is parsed as `start=None, end=N`, returning the head labelled as
/// a 206 tail) — so a regression that reintroduces a suffix-range tail
/// fetch is caught by wrong *content*, not just a header assertion.
/// `etags` cycles the `ETag` returned across successive calls (clamped to
/// its last entry), for the "log changed mid-read" test.
struct RangedFile {
    body: Vec<u8>,
    etags: Vec<&'static str>,
    calls: std::sync::atomic::AtomicUsize,
}

impl RangedFile {
    fn fixed(body: Vec<u8>, etag: &'static str) -> Self {
        Self {
            body,
            etags: vec![etag],
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn changing(body: Vec<u8>, etags: Vec<&'static str>) -> Self {
        Self {
            body,
            etags,
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn respond(&self, req: &Request) -> ResponseTemplate {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let etag = self.etags[call.min(self.etags.len() - 1)];
        let total = self.body.len();
        let Some(range) = req.headers.get("range").and_then(|v| v.to_str().ok()) else {
            return ResponseTemplate::new(200).set_body_bytes(self.body.clone());
        };
        let Some((start, end)) = parse_range(range) else {
            return ResponseTemplate::new(200).set_body_bytes(self.body.clone());
        };
        let start = start.unwrap_or(0);
        let end = end.filter(|&e| e < total).unwrap_or(total - 1);
        if start > end {
            return ResponseTemplate::new(416);
        }
        ResponseTemplate::new(206)
            .set_body_bytes(self.body[start..=end].to_vec())
            .insert_header("content-range", format!("bytes {start}-{end}/{total}"))
            .insert_header("etag", etag)
    }
}

fn numbered_lines(n: usize) -> String {
    use std::fmt::Write as _;
    (0..n).fold(String::new(), |mut acc, i| {
        let _ = writeln!(acc, "line{i:04}");
        acc
    })
}

#[tokio::test]
async fn get_job_log_plain_text_passthrough() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tests/7/file/autoinst-log.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("hello world\n"))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, true).await;

    let result = call(
        &client,
        "get_job_log",
        json!({"job_id": 7, "filename": "autoinst-log.txt"}),
    )
    .await
    .expect("call_tool");

    let value = text(&result);
    assert_eq!(value["filename"], "autoinst-log.txt");
    assert_eq!(value["content"], "hello world\n");
}

#[tokio::test]
async fn get_job_log_tail_uses_absolute_range_and_returns_the_tail() {
    let mock = MockServer::start().await;
    let body = numbered_lines(1000).into_bytes();
    assert_eq!(body.len(), 9000);
    let file = std::sync::Arc::new(RangedFile::fixed(body, "etag-1"));
    let responder = {
        let file = file.clone();
        move |req: &Request| file.respond(req)
    };
    Mock::given(method("GET"))
        .and(path("/tests/7/file/autoinst-log.txt"))
        .respond_with(responder)
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, true).await;

    let result = call(
        &client,
        "get_job_log",
        json!({"job_id": 7, "filename": "autoinst-log.txt", "tail_lines": 5}),
    )
    .await
    .expect("call_tool");

    let expected = (995..1000)
        .map(|i| format!("line{i:04}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(text(&result)["content"], expected);

    let requests = mock.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 2, "expected a probe then one tail request");
    let tail_range = requests[1].headers.get("range").unwrap().to_str().unwrap();
    assert!(
        tail_range.starts_with("bytes=") && !tail_range.contains("bytes=-"),
        "tail must use an absolute range, never a suffix range: {tail_range}"
    );
    assert!(
        requests.iter().all(|r| !r.headers.contains_key("referer")),
        "must never send a Referer to /tests/*"
    );
}

#[tokio::test]
async fn get_job_log_server_ignoring_range_is_still_correct() {
    let mock = MockServer::start().await;
    let body = numbered_lines(10);
    Mock::given(method("GET"))
        .and(path("/tests/9/file/small.log"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, true).await;

    let result = call(
        &client,
        "get_job_log",
        json!({"job_id": 9, "filename": "small.log", "tail_lines": 3}),
    )
    .await
    .expect("call_tool");

    assert_eq!(text(&result)["content"], "line0007\nline0008\nline0009");
    let requests = mock.received_requests().await.expect("requests");
    assert_eq!(
        requests.len(),
        1,
        "a 200 response already is the whole file"
    );
}

#[tokio::test]
async fn get_job_log_tail_reports_etag_change_after_one_retry() {
    let mock = MockServer::start().await;
    let body = numbered_lines(1000).into_bytes();
    let file = std::sync::Arc::new(RangedFile::changing(body, vec!["v1", "v2"]));
    let responder = {
        let file = file.clone();
        move |req: &Request| file.respond(req)
    };
    Mock::given(method("GET"))
        .and(path("/tests/7/file/autoinst-log.txt"))
        .respond_with(responder)
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, true).await;

    let result = call(
        &client,
        "get_job_log",
        json!({"job_id": 7, "filename": "autoinst-log.txt", "tail_lines": 5}),
    )
    .await
    .expect("call_tool");

    assert_eq!(text(&result)["changed_during_read"], true);
    let requests = mock.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 3, "probe + first tail attempt + one retry");
}

#[tokio::test]
async fn get_job_log_decodes_gzip() {
    let mock = MockServer::start().await;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut encoder, b"decoded gzip content\n").unwrap();
    let gz = encoder.finish().unwrap();
    Mock::given(method("GET"))
        .and(path("/tests/3/file/y2logs.tar.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(gz))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, true).await;

    let result = call(
        &client,
        "get_job_log",
        json!({"job_id": 3, "filename": "y2logs.tar.gz"}),
    )
    .await
    .expect("call_tool");

    assert_eq!(text(&result)["content"], "decoded gzip content\n");
}

fn build_tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut bytes);
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_cksum();
            builder.append_data(&mut header, name, *data).unwrap();
        }
        builder.finish().unwrap();
    }
    bytes
}

#[tokio::test]
async fn get_job_log_extracts_tar_xz_member() {
    let mock = MockServer::start().await;
    let tar_bytes = build_tar(&[("logs/inner.txt", b"member contents")]);
    let mut xz_bytes = Vec::new();
    lzma_rs::xz_compress(&mut &tar_bytes[..], &mut xz_bytes).unwrap();

    Mock::given(method("GET"))
        .and(path("/tests/4/file/logs.tar.xz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(xz_bytes))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, true).await;

    let result = call(
        &client,
        "get_job_log",
        json!({"job_id": 4, "filename": "logs.tar.xz", "member": "logs/inner.txt"}),
    )
    .await
    .expect("call_tool");

    let value = text(&result);
    assert_eq!(value["content"], "member contents");
    assert_eq!(value["member"], "logs/inner.txt");
}

#[tokio::test]
async fn list_job_log_members_lists_tar_entries() {
    let mock = MockServer::start().await;
    let tar_bytes = build_tar(&[("a.txt", b"aaa"), ("b.txt", b"bb")]);
    Mock::given(method("GET"))
        .and(path("/tests/4/file/logs.tar"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(tar_bytes))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, true).await;

    let result = call(
        &client,
        "list_job_log_members",
        json!({"job_id": 4, "filename": "logs.tar"}),
    )
    .await
    .expect("call_tool");

    let value = text(&result);
    let members = value["members"].as_array().unwrap();
    assert_eq!(members.len(), 2);
    assert_eq!(members[0]["path"], "a.txt");
    assert_eq!(members[0]["size"], 3);
    assert_eq!(value["truncated"], false);
}

#[tokio::test]
async fn get_job_log_binary_content_is_unsupported_media() {
    let mock = MockServer::start().await;
    let mut body = vec![0x89u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    body.extend_from_slice(&[0xFF, 0xFE, 0x00, 0x01, 0xFF]);
    Mock::given(method("GET"))
        .and(path("/tests/2/file/video.ogv"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, true).await;

    let result = call(
        &client,
        "get_job_log",
        json!({"job_id": 2, "filename": "video.ogv"}),
    )
    .await
    .expect("call_tool");

    assert_eq!(result.is_error, Some(true));
    assert_eq!(text(&result)["error"]["kind"], "unsupported_media");
}

#[tokio::test]
async fn get_job_log_oversized_body_is_response_too_large() {
    let mock = MockServer::start().await;
    let body = "x".repeat(500);
    Mock::given(method("GET"))
        .and(path("/tests/5/file/big.log"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, true).await;

    let result = call(
        &client,
        "get_job_log",
        json!({"job_id": 5, "filename": "big.log", "max_bytes": 100}),
    )
    .await
    .expect("call_tool");

    assert_eq!(result.is_error, Some(true));
    assert_eq!(text(&result)["error"]["kind"], "response_too_large");
}

#[tokio::test]
async fn get_job_log_gzip_bomb_is_response_too_large() {
    let mock = MockServer::start().await;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    std::io::Write::write_all(&mut encoder, &vec![0u8; 10 * 1024 * 1024]).unwrap();
    let gz = encoder.finish().unwrap();
    let ceiling = 1024 * 1024;
    assert!(
        gz.len() < ceiling,
        "compressed bomb must still fit under the ceiling raw"
    );

    Mock::given(method("GET"))
        .and(path("/tests/6/file/bomb.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(gz))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, true).await;

    let result = call(
        &client,
        "get_job_log",
        json!({"job_id": 6, "filename": "bomb.gz", "max_bytes": ceiling}),
    )
    .await
    .expect("call_tool");

    assert_eq!(result.is_error, Some(true));
    assert_eq!(text(&result)["error"]["kind"], "response_too_large");
}

#[tokio::test]
async fn get_job_log_404_is_not_found() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tests/1/file/missing.txt"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, true).await;

    let result = call(
        &client,
        "get_job_log",
        json!({"job_id": 1, "filename": "missing.txt"}),
    )
    .await
    .expect("call_tool");

    assert_eq!(result.is_error, Some(true));
    assert_eq!(text(&result)["error"]["kind"], "not_found");
}

#[tokio::test]
async fn get_job_log_filename_with_slash_is_rejected() {
    let mock = MockServer::start().await;
    let client = server_with_mock(&mock, true).await;

    let err = call(
        &client,
        "get_job_log",
        json!({"job_id": 1, "filename": "a/b.txt"}),
    )
    .await
    .expect_err("filename with a slash must be rejected");

    let rmcp::ServiceError::McpError(mcp_err) = err else {
        panic!("expected McpError, got {err:?}");
    };
    assert_eq!(mcp_err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    assert!(mock.received_requests().await.expect("requests").is_empty());
}

#[tokio::test]
async fn get_job_log_grep_returns_matches_with_context() {
    let mock = MockServer::start().await;
    let body = "one\ntwo\nERROR: boom\nfour\nfive\n";
    Mock::given(method("GET"))
        .and(path("/tests/8/file/log.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, true).await;

    let result = call(
        &client,
        "get_job_log",
        json!({"job_id": 8, "filename": "log.txt", "grep": "ERROR", "context_lines": 1}),
    )
    .await
    .expect("call_tool");

    let value = text(&result);
    assert_eq!(value["total_matches"], 1);
    let matches = value["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 3);
    assert_eq!(matches[1]["text"], "ERROR: boom");
}

#[tokio::test]
async fn list_job_logs_parses_downloads_ajax() {
    let mock = MockServer::start().await;
    let html = r#"<a href="/tests/11/file/autoinst-log.txt">autoinst-log.txt</a>
        <h2>Uploaded logs</h2>
        <a href="/tests/11/file/my_custom.log">my_custom.log</a>"#;
    Mock::given(method("GET"))
        .and(path("/tests/11/downloads_ajax"))
        .respond_with(ResponseTemplate::new(200).set_body_string(html))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, true).await;

    let result = call(&client, "list_job_logs", json!({"job_id": 11}))
        .await
        .expect("call_tool");

    let value = text(&result);
    assert_eq!(value["source"], "downloads_ajax");
    let files = value["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0]["kind"], "result");
    assert_eq!(files[1]["kind"], "ulog");
}

#[tokio::test]
async fn list_job_logs_falls_back_to_details_on_404() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tests/12/downloads_ajax"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs/12/details"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "logs": ["autoinst-log.txt"],
            "ulogs": ["my_custom.log"],
        })))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, true).await;

    let result = call(&client, "list_job_logs", json!({"job_id": 12}))
        .await
        .expect("call_tool");

    let value = text(&result);
    assert_eq!(value["source"], "details");
    let files = value["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
}

#[tokio::test]
async fn list_job_logs_falls_back_to_details_when_ajax_parse_is_empty() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tests/13/downloads_ajax"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<p>no files here</p>"))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs/13/details"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"logs": ["a.txt"], "ulogs": []})),
        )
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, true).await;

    let result = call(&client, "list_job_logs", json!({"job_id": 13}))
        .await
        .expect("call_tool");

    assert_eq!(text(&result)["source"], "details");
}
