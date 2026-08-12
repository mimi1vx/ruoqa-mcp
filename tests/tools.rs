//! wiremock-backed port of `tests/test_tools.py`. Drives `OpenQaServer` over
//! an in-memory MCP session (a `tokio::io::duplex` pair) so tool calls go
//! through the real router/handler, not internal shortcuts.

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
    let mut builder = ruoqa::ClientBuilder::new()
        .server(mock.uri())
        .config_paths(vec![]);
    if let (Some(key), Some(secret)) = (api_key, api_secret) {
        builder = builder
            .api_key(ruoqa::ApiKey::new(key))
            .api_secret(ruoqa::ApiSecret::new(secret));
    }
    let client = builder.build().expect("build client");
    let server = OpenQaServer::new(client, readonly);

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
async fn restart_jobs_bulk_repeats_jobs_key() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/jobs/restart"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": []})))
        .mount(&mock)
        .await;
    let client = server_with_mock(&mock, false).await;

    call(&client, "restart_jobs_bulk", json!({"job_ids": [1, 2]}))
        .await
        .expect("call_tool");

    let requests = mock.received_requests().await.expect("recorded requests");
    let body = String::from_utf8_lossy(&requests.last().unwrap().body);
    assert_eq!(body, "jobs=1&jobs=2");
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
async fn readonly_server_does_not_expose_mutating_tools() {
    let mock = MockServer::start().await;
    let client = server_with_mock(&mock, true).await;

    let err = call(&client, "delete_job", json!({"job_id": 1}))
        .await
        .expect_err("readonly server has no write tools");

    assert!(matches!(err, rmcp::ServiceError::McpError(_)));
}
