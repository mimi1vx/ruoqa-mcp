//! Table-driven request-shape check for all 43 registered tools: method,
//! path, query string, and body/form-encoding. `tests/tools.rs` asserts
//! response handling and edge cases; this file asserts only where and how
//! each tool's request goes out, so a typo'd `format!` path or a `GET` where
//! openQA wants `POST` fails here instead of shipping unnoticed.

use std::collections::BTreeSet;

use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use ruoqa_mcp::OpenQaServer;
use serde_json::{Value, json};
use wiremock::matchers::{any, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn run_server(mock: &MockServer) -> RunningService<RoleClient, ()> {
    let client = ruoqa::ClientBuilder::new()
        .server(mock.uri())
        .config_paths(vec![])
        .build()
        .expect("build client");
    let server = OpenQaServer::new(client, false);

    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_transport).await {
            let _ = running.waiting().await;
        }
    });

    ().serve(client_transport).await.expect("client handshake")
}

async fn call(client: &RunningService<RoleClient, ()>, name: &str, args: &Value) {
    let mut params = CallToolRequestParams::new(name.to_string());
    if let Some(obj) = args.as_object() {
        params = params.with_arguments(obj.clone());
    }
    // A mock backend answers every route with `200 {}`, so a call error here
    // means the client-side plumbing broke, not the routing under test.
    client
        .peer()
        .call_tool(params)
        .await
        .unwrap_or_else(|e| panic!("{name}: call_tool failed: {e}"));
}

/// One row of the routing matrix. `body: None` means the request must carry
/// no body and no `content-type` header; `Some(form)` is the expected
/// `application/x-www-form-urlencoded` body.
struct Case {
    tool: &'static str,
    args: Value,
    method: &'static str,
    path: &'static str,
    query: &'static str,
    body: Option<&'static str>,
}

// One row per tool; splitting the table would only obscure it.
#[allow(clippy::too_many_lines)]
fn cases() -> Vec<Case> {
    vec![
        // --- Read tools: all GET, all body-less (28) ---
        Case {
            tool: "list_jobs",
            args: json!({"state": "done"}),
            method: "GET",
            path: "/api/v1/jobs",
            query: "state=done",
            body: None,
        },
        Case {
            tool: "list_jobs_overview",
            args: json!({"arch": "x86_64"}),
            method: "GET",
            path: "/api/v1/jobs/overview",
            query: "arch=x86_64",
            body: None,
        },
        Case {
            tool: "get_job",
            args: json!({"job_id": 7}),
            method: "GET",
            path: "/api/v1/jobs/7",
            query: "",
            body: None,
        },
        Case {
            tool: "get_job_comments",
            args: json!({"job_id": 7}),
            method: "GET",
            path: "/api/v1/jobs/7/comments",
            query: "",
            body: None,
        },
        Case {
            tool: "list_machines",
            args: json!({}),
            method: "GET",
            path: "/api/v1/machines",
            query: "",
            body: None,
        },
        Case {
            tool: "list_test_suites",
            args: json!({}),
            method: "GET",
            path: "/api/v1/test_suites",
            query: "",
            body: None,
        },
        Case {
            tool: "list_products",
            args: json!({}),
            method: "GET",
            path: "/api/v1/products",
            query: "",
            body: None,
        },
        Case {
            tool: "find_jobs_by_setting",
            args: json!({"key": "BUILD", "list_value": "42"}),
            method: "GET",
            path: "/api/v1/job_settings/jobs",
            query: "key=BUILD&list_value=42",
            body: None,
        },
        Case {
            tool: "get_job_details",
            args: json!({"job_id": 7}),
            method: "GET",
            path: "/api/v1/jobs/7/details",
            query: "",
            body: None,
        },
        Case {
            tool: "get_job_status",
            args: json!({"job_id": 7, "follow": 1}),
            method: "GET",
            path: "/api/v1/experimental/jobs/7/status",
            query: "follow=1",
            body: None,
        },
        Case {
            tool: "list_job_groups",
            args: json!({}),
            method: "GET",
            path: "/api/v1/job_groups",
            query: "",
            body: None,
        },
        Case {
            tool: "get_job_group",
            args: json!({"group_id": 3}),
            method: "GET",
            path: "/api/v1/job_groups/3",
            query: "",
            body: None,
        },
        Case {
            tool: "list_job_group_jobs",
            args: json!({"group_id": 3}),
            method: "GET",
            path: "/api/v1/job_groups/3/jobs",
            query: "",
            body: None,
        },
        Case {
            tool: "get_job_group_build_results",
            args: json!({
                "group_id": 3,
                "limit_builds": 2,
                "time_limit_days": 1.5,
                "only_tagged": 1,
                "show_tags": 0
            }),
            method: "GET",
            path: "/api/v1/job_groups/3/build_results",
            query: "limit_builds=2&time_limit_days=1.5&only_tagged=1&show_tags=0",
            body: None,
        },
        Case {
            tool: "list_parent_groups",
            args: json!({}),
            method: "GET",
            path: "/api/v1/parent_groups",
            query: "",
            body: None,
        },
        Case {
            tool: "get_parent_group",
            args: json!({"group_id": 3}),
            method: "GET",
            path: "/api/v1/parent_groups/3",
            query: "",
            body: None,
        },
        Case {
            tool: "list_assets",
            args: json!({}),
            method: "GET",
            path: "/api/v1/assets",
            query: "",
            body: None,
        },
        Case {
            tool: "get_asset",
            args: json!({"asset_id": 11}),
            method: "GET",
            path: "/api/v1/assets/11",
            query: "",
            body: None,
        },
        Case {
            tool: "list_workers",
            args: json!({}),
            method: "GET",
            path: "/api/v1/workers",
            query: "",
            body: None,
        },
        Case {
            tool: "list_bugs",
            args: json!({}),
            method: "GET",
            path: "/api/v1/bugs",
            query: "",
            body: None,
        },
        Case {
            tool: "search",
            args: json!({"q": "boot"}),
            method: "GET",
            path: "/api/v1/experimental/search",
            query: "q=boot",
            body: None,
        },
        Case {
            tool: "get_scheduled_product",
            args: json!({"scheduled_product_id": 5}),
            method: "GET",
            path: "/api/v1/isos/5",
            query: "",
            body: None,
        },
        Case {
            tool: "get_iso_job_stats",
            args: json!({}),
            method: "GET",
            path: "/api/v1/isos/job_stats",
            query: "",
            body: None,
        },
        Case {
            tool: "list_group_comments",
            args: json!({"group_id": 3}),
            method: "GET",
            path: "/api/v1/groups/3/comments",
            query: "",
            body: None,
        },
        Case {
            tool: "list_parent_group_comments",
            args: json!({"parent_group_id": 4}),
            method: "GET",
            path: "/api/v1/parent_groups/4/comments",
            query: "",
            body: None,
        },
        // `list_job_logs` is covered separately below (see
        // `list_job_logs_requests_downloads_ajax_first`): its conditional
        // fallback to `/details` means it doesn't fit this matrix's
        // exactly-one-request assumption against the shared any() mock.
        Case {
            tool: "list_job_log_members",
            args: json!({"job_id": 7, "filename": "y2logs.tar.xz"}),
            method: "GET",
            path: "/tests/7/file/y2logs.tar.xz",
            query: "",
            body: None,
        },
        Case {
            tool: "get_job_log",
            args: json!({"job_id": 7, "filename": "autoinst-log.txt"}),
            method: "GET",
            path: "/tests/7/file/autoinst-log.txt",
            query: "",
            body: None,
        },
        // --- Mutating tools (14) ---
        Case {
            tool: "restart_jobs",
            args: json!({"job_ids": [1, 2], "force": 1, "prio": 50}),
            method: "POST",
            path: "/api/v1/jobs/restart",
            query: "",
            body: Some("jobs=1&jobs=2&force=1&prio=50"),
        },
        Case {
            tool: "cancel_job",
            args: json!({"job_id": 7}),
            method: "POST",
            path: "/api/v1/jobs/7/cancel",
            query: "",
            body: None,
        },
        Case {
            tool: "add_job_comment",
            args: json!({"job_id": 7, "text": "hi"}),
            method: "POST",
            path: "/api/v1/jobs/7/comments",
            query: "",
            body: Some("text=hi"),
        },
        Case {
            tool: "trigger_isos",
            args: json!({
                "distri": "sle",
                "version": "15-SP6",
                "flavor": "Online",
                "arch": "x86_64"
            }),
            method: "POST",
            path: "/api/v1/isos",
            query: "",
            body: Some("DISTRI=sle&VERSION=15-SP6&FLAVOR=Online&ARCH=x86_64"),
        },
        Case {
            tool: "delete_job",
            args: json!({"job_id": 7}),
            method: "DELETE",
            path: "/api/v1/jobs/7",
            query: "",
            body: None,
        },
        Case {
            tool: "duplicate_job",
            args: json!({"job_id": 7, "prio": 50, "dup_type_auto": 1}),
            method: "POST",
            path: "/api/v1/jobs/7/duplicate",
            query: "",
            body: Some("prio=50&dup_type_auto=1"),
        },
        Case {
            tool: "set_job_priority",
            args: json!({"job_id": 7, "prio": 50}),
            method: "POST",
            path: "/api/v1/jobs/7/prio",
            query: "",
            body: Some("prio=50"),
        },
        Case {
            tool: "cancel_jobs",
            args: json!({"state": "running"}),
            method: "POST",
            path: "/api/v1/jobs/cancel",
            query: "state=running",
            body: None,
        },
        Case {
            tool: "add_group_comment",
            args: json!({"group_id": 3, "text": "hi"}),
            method: "POST",
            path: "/api/v1/groups/3/comments",
            query: "",
            body: Some("text=hi"),
        },
        Case {
            tool: "add_parent_group_comment",
            args: json!({"parent_group_id": 4, "text": "hi"}),
            method: "POST",
            path: "/api/v1/parent_groups/4/comments",
            query: "",
            body: Some("text=hi"),
        },
        Case {
            tool: "update_job_comment",
            args: json!({"job_id": 7, "comment_id": 9, "text": "hi"}),
            method: "PUT",
            path: "/api/v1/jobs/7/comments/9",
            query: "",
            body: Some("text=hi"),
        },
        Case {
            tool: "delete_job_comment",
            args: json!({"job_id": 7, "comment_id": 9}),
            method: "DELETE",
            path: "/api/v1/jobs/7/comments/9",
            query: "",
            body: None,
        },
        Case {
            tool: "create_bug",
            args: json!({"bugid": "bsc#1234567", "title": "t"}),
            method: "POST",
            path: "/api/v1/bugs",
            query: "",
            body: Some("bugid=bsc%231234567&title=t"),
        },
        Case {
            tool: "cancel_scheduled_product",
            args: json!({"name": "SLE-15-SP6-Online-x86_64-Build1.1-Media1.iso"}),
            method: "POST",
            path: "/api/v1/isos/SLE-15-SP6-Online-x86_64-Build1.1-Media1.iso/cancel",
            query: "",
            body: None,
        },
    ]
}

/// Tools whose request shape is verified by a dedicated test below instead
/// of the shared matrix: `list_job_logs` conditionally issues a *second*
/// request (falling back to `/details`) whenever the first response isn't
/// recognizable HTML, which the shared `any()` mock never produces, so it
/// can't satisfy this file's exactly-one-request-per-case assumption.
/// `get_job_log_errors` issues a `/details` fetch plus a conditional number
/// of tier fetches, same problem.
const COVERED_OUTSIDE_MATRIX: &[&str] = &["list_job_logs", "get_job_log_errors"];

/// Names covered by [`cases`] plus [`COVERED_OUTSIDE_MATRIX`], for the
/// coverage-ratchet guard test below.
fn case_names() -> BTreeSet<&'static str> {
    cases()
        .iter()
        .map(|c| c.tool)
        .chain(COVERED_OUTSIDE_MATRIX.iter().copied())
        .collect()
}

#[tokio::test]
async fn every_tool_sends_the_expected_request() {
    let mock = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&mock)
        .await;
    let client = run_server(&mock).await;

    let mut failures = Vec::new();
    let mut seen = 0usize;
    for case in cases() {
        call(&client, case.tool, &case.args).await;

        let requests = mock.received_requests().await.expect("recorded requests");
        let new_requests = &requests[seen..];
        seen = requests.len();
        let [request] = new_requests else {
            failures.push(format!(
                "{}: expected exactly 1 new request, got {}",
                case.tool,
                new_requests.len()
            ));
            continue;
        };

        if request.method.as_str() != case.method {
            failures.push(format!(
                "{}: method {} != {}",
                case.tool, request.method, case.method
            ));
        }
        if request.url.path() != case.path {
            failures.push(format!(
                "{}: path {} != {}",
                case.tool,
                request.url.path(),
                case.path
            ));
        }
        let query = request.url.query().unwrap_or("");
        if query != case.query {
            failures.push(format!(
                "{}: query {query:?} != {:?}",
                case.tool, case.query
            ));
        }

        let content_type = request
            .headers
            .get("content-type")
            .map(|v| v.to_str().unwrap_or("<non-utf8>"));
        if let Some(expected) = case.body {
            if content_type != Some("application/x-www-form-urlencoded") {
                failures.push(format!(
                    "{}: content-type {content_type:?} != form-urlencoded",
                    case.tool
                ));
            }
            let body = String::from_utf8_lossy(&request.body);
            if body != expected {
                failures.push(format!("{}: body {body:?} != {expected:?}", case.tool));
            }
        } else {
            if !request.body.is_empty() {
                failures.push(format!(
                    "{}: expected a body-less request, got {:?}",
                    case.tool,
                    String::from_utf8_lossy(&request.body)
                ));
            }
            if content_type.is_some() {
                failures.push(format!(
                    "{}: expected no content-type on a body-less request, got {content_type:?}",
                    case.tool
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "routing mismatches:\n{}",
        failures.join("\n")
    );
}

/// Ratchets the matrix against the live router: a tool added to
/// `src/tools/{read,write}.rs` without a matching [`Case`] fails here until
/// one is added, and a stale `Case` for a removed tool fails symmetrically.
#[tokio::test]
async fn matrix_covers_exactly_the_registered_tools() {
    let mock = MockServer::start().await;
    let client = run_server(&mock).await;

    let registered: BTreeSet<String> = client
        .peer()
        .list_all_tools()
        .await
        .expect("list_tools")
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    let matrix = case_names();

    let missing: Vec<_> = registered
        .iter()
        .filter(|name| !matrix.contains(name.as_str()))
        .collect();
    let stale: Vec<_> = matrix
        .iter()
        .filter(|name| !registered.contains(**name))
        .collect();

    assert!(
        missing.is_empty() && stale.is_empty(),
        "routing matrix out of sync with the registered tools: missing from matrix {missing:?}, \
         stale in matrix {stale:?}"
    );
}

/// `list_job_logs`'s first request always targets `downloads_ajax`,
/// regardless of whether a fallback to `/details` follows.
#[tokio::test]
async fn list_job_logs_requests_downloads_ajax_first() {
    let mock = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&mock)
        .await;
    let client = run_server(&mock).await;

    call(&client, "list_job_logs", &json!({"job_id": 42})).await;

    let requests = mock.received_requests().await.expect("recorded requests");
    let first = requests.first().expect("at least one request");
    assert_eq!(first.method.as_str(), "GET");
    assert_eq!(first.url.path(), "/tests/42/downloads_ajax");
}

/// `get_job_log_errors` fetches `/details` first, then probes
/// `serial_terminal.txt` (present in `/details`'s `logs`); a tier-1 hit must
/// never go on to fetch `autoinst-log.txt` at all.
#[tokio::test]
async fn get_job_log_errors_requests_details_then_serial_terminal_and_skips_autoinst_on_tap_hit() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/jobs/50/details"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "job": {"logs": ["autoinst-log.txt", "serial_terminal.txt"], "testresults": []}
        })))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/tests/50/file/serial_terminal.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string("nice05.c:138: TFAIL: bad\n"))
        .mount(&mock)
        .await;
    let client = run_server(&mock).await;

    call(&client, "get_job_log_errors", &json!({"job_id": 50})).await;

    let requests = mock.received_requests().await.expect("recorded requests");
    assert_eq!(requests[0].url.path(), "/api/v1/jobs/50/details");
    assert_eq!(requests[1].url.path(), "/tests/50/file/serial_terminal.txt");
    assert!(
        requests
            .iter()
            .all(|r| r.url.path() != "/tests/50/file/autoinst-log.txt"),
        "a tap-tier hit must never fetch autoinst-log.txt: {requests:?}"
    );
}
