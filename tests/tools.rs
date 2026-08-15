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
use wiremock::{Mock, MockServer, ResponseTemplate};

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
    let mut builder = ruoqa::ClientBuilder::new()
        .server(mock.uri())
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

    let err = call(&client, "cancel_job", json!({"job_id": 7}))
        .await
        .expect_err("expected an error");

    let message = err.to_string();
    assert!(
        message.contains("403"),
        "message should mention 403: {message}"
    );
    assert!(
        !message.contains("TOPSECRET"),
        "message must not leak the API secret: {message}"
    );
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

    let err = call(&client, "list_jobs", json!({}))
        .await
        .expect_err("call must fail once the deadline elapses");

    let rmcp::ServiceError::McpError(mcp_err) = err else {
        panic!("expected McpError, got {err:?}");
    };
    assert!(
        mcp_err.message.contains("OPENQA_MCP_CALL_TIMEOUT"),
        "message should name the deadline variable: {}",
        mcp_err.message
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
