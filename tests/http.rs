//! HTTP transport tests: bearer authentication, per-principal scopes, and the
//! `Host` allowlist. Everything runs against a real listener on an ephemeral
//! port with a wiremock openQA behind it, so a rejection can be checked to have
//! reached openQA not at all.

use std::sync::Arc;

use axum::http::request::Parts;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RunningService};
use rmcp::transport::StreamableHttpServerConfig;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpService;
use rmcp::{ErrorData, RoleClient, RoleServer, ServerHandler, ServiceExt};
use ruoqa_mcp::OpenQaServer;
use ruoqa_mcp::http::{HttpAuth, HttpEnv, allowed_hosts, router};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const WRITE_TOKEN: &str = "0123456789abcdef0123456789abcdef";
const READ_TOKEN: &str = "fedcba9876543210fedcba9876543210";

/// Marker inserted by the middleware; the handler reports whether it arrived.
#[derive(Clone, Copy)]
struct Marker;

#[derive(Clone)]
struct Spy;

impl ServerHandler for Spy {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: vec![Tool::new("spy", "reports the marker", Arc::default())],
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        _request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let seen = context
            .extensions
            .get::<Parts>()
            .is_some_and(|parts| parts.extensions.get::<Marker>().is_some());
        Ok(CallToolResult::success(vec![ContentBlock::text(seen.to_string())]).into())
    }
}

/// Step 4 spike, kept as a regression test: an extension inserted by an axum
/// middleware must be visible to the tool handler through
/// `RequestContext::extensions` — the whole per-principal authorization design
/// rests on it.
#[tokio::test]
async fn axum_middleware_extension_reaches_tool_handler() {
    let service: StreamableHttpService<Spy, LocalSessionManager> = StreamableHttpService::new(
        || Ok(Spy),
        Arc::default(),
        StreamableHttpServerConfig::default()
            .with_cancellation_token(CancellationToken::new().child_token()),
    );
    let router =
        axum::Router::new()
            .nest_service("/mcp", service)
            .layer(axum::middleware::from_fn(
                async |mut req: axum::extract::Request, next: axum::middleware::Next| {
                    req.extensions_mut().insert(Marker);
                    next.run(req).await
                },
            ));

    let addr = spawn(router).await;
    let client = connect(addr, None).await.expect("client handshake");
    let result = client
        .peer()
        .call_tool(CallToolRequestParams::new("spy".to_string()))
        .await
        .expect("call_tool");

    let ContentBlock::Text(text) = &result.content[0] else {
        panic!("expected text content");
    };
    assert_eq!(
        text.text, "true",
        "middleware extension must reach the tool"
    );
}

/// Serve `router` on an ephemeral loopback port; returns its address.
async fn spawn(router: axum::Router) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move { axum::serve(listener, router).await });
    addr
}

/// A `ruoqa-mcp` HTTP router in front of `mock`, with the given credentials.
fn openqa_router(
    mock: &MockServer,
    env: &HttpEnv,
    insecure: bool,
    hosts: &[String],
) -> axum::Router {
    let client = ruoqa::ClientBuilder::new()
        .server(mock.uri())
        .config_paths(vec![])
        .api_key(ruoqa::ApiKey::new("key"))
        .api_secret(ruoqa::ApiSecret::new("secret"))
        .build()
        .expect("build client");
    let auth = HttpAuth::resolve(env, insecure).expect("resolve auth");
    let server = OpenQaServer::new(client, false).with_scope_enforcement(!auth.is_insecure());
    router(
        server,
        Arc::new(auth),
        allowed_hosts(hosts),
        &CancellationToken::new(),
    )
}

fn tokens() -> HttpEnv {
    HttpEnv {
        token: Some(WRITE_TOKEN.to_string()),
        read_token: Some(READ_TOKEN.to_string()),
    }
}

async fn connect(
    addr: std::net::SocketAddr,
    token: Option<&str>,
) -> Result<RunningService<RoleClient, ()>, rmcp::service::ClientInitializeError> {
    let mut config = StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp"));
    if let Some(token) = token {
        config = config.auth_header(token);
    }
    let transport = rmcp::transport::StreamableHttpClientTransport::with_client(
        reqwest::Client::default(),
        config,
    );
    ().serve(transport).await
}

/// A minimal MCP `initialize` POST, so the auth layer can be probed without
/// the client transport's error wrapping.
async fn post_initialize(
    addr: std::net::SocketAddr,
    authorization: Option<&str>,
    host: Option<&str>,
) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("http://{addr}/mcp"))
        .header("accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }
        }));
    if let Some(value) = authorization {
        request = request.header("authorization", value);
    }
    if let Some(value) = host {
        request = request.header("host", value);
    }
    request.send().await.expect("send initialize")
}

/// A mounted openQA endpoint that must not be reached on a rejected call.
async fn mock_openqa() -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/jobs/7/cancel"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": 1})))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"job": {"id": 7}})))
        .mount(&mock)
        .await;
    mock
}

#[tokio::test]
async fn anonymous_request_is_rejected_without_touching_openqa() {
    let mock = mock_openqa().await;
    let addr = spawn(openqa_router(&mock, &tokens(), false, &[])).await;

    let response = post_initialize(addr, None, None).await;

    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(
        response
            .headers()
            .get("www-authenticate")
            .map(|v| v.to_str().unwrap()),
        Some("Bearer")
    );
    assert!(response.bytes().await.expect("body").is_empty());
    assert!(mock.received_requests().await.expect("requests").is_empty());
}

#[tokio::test]
async fn bad_credentials_are_rejected() {
    let mock = mock_openqa().await;
    let addr = spawn(openqa_router(&mock, &tokens(), false, &[])).await;

    for header in [
        "",
        "Bearer",
        "Bearer ",
        "Bearer not-the-token-not-the-token",
        WRITE_TOKEN,
        "Basic MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        // A prefix of the real token must not be accepted.
        &WRITE_TOKEN[..16],
    ] {
        let response = post_initialize(addr, Some(header), None).await;
        assert_eq!(
            response.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "{header:?} must not authenticate"
        );
    }
    assert!(mock.received_requests().await.expect("requests").is_empty());
}

#[tokio::test]
async fn write_token_keeps_the_full_tool_set() {
    let mock = mock_openqa().await;
    let addr = spawn(openqa_router(&mock, &tokens(), false, &[])).await;
    let client = connect(addr, Some(WRITE_TOKEN)).await.expect("handshake");

    let tools = client.peer().list_all_tools().await.expect("list_tools");
    assert_eq!(tools.len(), 39);

    let mut params = CallToolRequestParams::new("cancel_job".to_string());
    params = params.with_arguments(json!({"job_id": 7}).as_object().unwrap().clone());
    client.peer().call_tool(params).await.expect("cancel_job");

    let requests = mock.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/api/v1/jobs/7/cancel");
}

#[tokio::test]
async fn read_token_sees_and_reaches_only_read_tools() {
    let mock = mock_openqa().await;
    let addr = spawn(openqa_router(&mock, &tokens(), false, &[])).await;
    let client = connect(addr, Some(READ_TOKEN)).await.expect("handshake");

    let tools = client.peer().list_all_tools().await.expect("list_tools");
    assert_eq!(tools.len(), 25);
    assert!(!tools.iter().any(|t| t.name == "cancel_job"));

    let mut params = CallToolRequestParams::new("cancel_job".to_string());
    params = params.with_arguments(json!({"job_id": 7}).as_object().unwrap().clone());
    let error = client
        .peer()
        .call_tool(params)
        .await
        .expect_err("cancel_job must be refused");
    assert!(
        error.to_string().contains("not authorized"),
        "unexpected error: {error}"
    );
    assert!(
        mock.received_requests().await.expect("requests").is_empty(),
        "a refused call must not reach openQA"
    );

    let mut params = CallToolRequestParams::new("get_job".to_string());
    params = params.with_arguments(json!({"job_id": 7}).as_object().unwrap().clone());
    client.peer().call_tool(params).await.expect("get_job");
    assert_eq!(mock.received_requests().await.expect("requests").len(), 1);
}

#[tokio::test]
async fn host_header_is_restricted_to_loopback_and_configured_authorities() {
    let mock = mock_openqa().await;
    let configured = vec!["mcp.example.com".to_string()];
    let addr = spawn(openqa_router(&mock, &tokens(), false, &configured)).await;
    let bearer = format!("Bearer {WRITE_TOKEN}");

    let forbidden = post_initialize(addr, Some(&bearer), Some("evil.example")).await;
    assert_eq!(forbidden.status(), reqwest::StatusCode::FORBIDDEN);

    let loopback = post_initialize(addr, Some(&bearer), Some(&addr.to_string())).await;
    assert!(loopback.status().is_success());

    let allowed = post_initialize(addr, Some(&bearer), Some("mcp.example.com")).await;
    assert!(allowed.status().is_success());
}

#[tokio::test]
async fn a_wildcard_bind_does_not_widen_the_host_allowlist() {
    let mock = mock_openqa().await;
    // Nothing configured; the server happens to be reachable on 0.0.0.0.
    let addr = spawn(openqa_router(&mock, &tokens(), false, &[])).await;
    let bearer = format!("Bearer {WRITE_TOKEN}");

    let response = post_initialize(addr, Some(&bearer), Some("mcp.example.com")).await;
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn insecure_no_auth_serves_anonymous_write_calls() {
    let mock = mock_openqa().await;
    let addr = spawn(openqa_router(&mock, &HttpEnv::default(), true, &[])).await;
    let client = connect(addr, None).await.expect("handshake");

    let tools = client.peer().list_all_tools().await.expect("list_tools");
    assert_eq!(tools.len(), 39);

    let mut params = CallToolRequestParams::new("cancel_job".to_string());
    params = params.with_arguments(json!({"job_id": 7}).as_object().unwrap().clone());
    client.peer().call_tool(params).await.expect("cancel_job");
    assert_eq!(mock.received_requests().await.expect("requests").len(), 1);
}
