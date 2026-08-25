//! The `OpenQaServer` handler and the request funnel (port of `server.py`'s
//! `mcp`, `_client`, and `_request`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Method;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, InitializeRequestParams,
    InitializeResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion, ResultType,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, tool_handler};
use serde_json::{Value, json};

use crate::audit::{self, Auditor, Outcome, RecordScope, Transport};
use crate::error::{classify, kind_of, tool_error};
use crate::form::Form;
use crate::heartbeat::with_heartbeat;
use crate::http::{Scope, scope_of};
use crate::servers::ServerRegistry;

const INSTRUCTIONS: &str = "Query and control an openQA instance. Read tools inspect jobs, \
machines, test suites, and products; mutating tools restart/cancel/delete jobs, comment, and \
trigger ISOs, and require API credentials.";

/// Denial text for both refusal paths. Deliberately identical and detail-free:
/// a caller learns that it may not do this, not how the gate is wired.
const DENIED: &str = "this credential is not authorized for mutating tools";

/// Mirrors `config::call_timeout`'s default; kept here too so a bare `new`
/// (as used by tests) still gets a deadline without every caller opting in.
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_mins(5);

#[derive(Clone)]
pub struct OpenQaServer {
    pub(crate) servers: ServerRegistry,
    tool_router: ToolRouter<Self>,
    /// Whether each request must carry a [`Scope`]. Off for stdio, where the
    /// process credential is the only principal there is.
    enforce_scopes: bool,
    /// Whole-call deadline, independent of the per-HTTP-request timeout.
    /// `None` disables it.
    call_timeout: Option<Duration>,
    /// The audit stream. `None` means auditing is off: no record is built, no
    /// mutex is touched.
    audit: Option<Arc<Auditor>>,
    /// Which transport is serving this instance, carried onto every audit
    /// record.
    transport: Transport,
}

impl OpenQaServer {
    /// Merges the read router with the write router unless `readonly`, which
    /// keeps `--readonly` a compile-time-guaranteed subset rather than a
    /// hand-maintained list of mutating tool names to disable at runtime.
    #[must_use]
    pub fn new(servers: ServerRegistry, readonly: bool) -> Self {
        let mut tool_router = if readonly {
            Self::read_tool_router()
        } else {
            Self::read_tool_router() + Self::write_tool_router()
        };
        // Vertex's FunctionDeclaration validator rejects an `anyOf` with
        // sibling keys, which is what a Gemini client turns schemars'
        // `Option<T>` rendering (`"type": [T, "null"]`) into.
        for route in tool_router.map.values_mut() {
            let attr = Arc::make_mut(&mut route.attr.input_schema);
            crate::schema::degrade_nullable_unions(attr);
            if let Some(output_schema) = &mut route.attr.output_schema {
                crate::schema::degrade_nullable_unions(Arc::make_mut(output_schema));
            }
        }
        Self {
            servers,
            tool_router,
            enforce_scopes: false,
            call_timeout: Some(DEFAULT_CALL_TIMEOUT),
            audit: None,
            transport: Transport::Stdio,
        }
    }

    /// Require every request to carry a [`Scope`] and refuse mutating tools to
    /// read-scope principals. Enabled for authenticated HTTP only.
    #[must_use]
    pub fn with_scope_enforcement(mut self, yes: bool) -> Self {
        self.enforce_scopes = yes;
        self
    }

    /// Set the whole-call deadline; `None` disables it.
    #[must_use]
    pub fn with_call_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.call_timeout = timeout;
        self
    }

    /// Set the audit stream; `None` (the default) disables it entirely.
    #[must_use]
    pub fn with_audit(mut self, audit: Option<Arc<Auditor>>) -> Self {
        self.audit = audit;
        self
    }

    /// Set which transport is serving this instance, carried onto every
    /// audit record.
    #[must_use]
    pub fn with_transport(mut self, transport: Transport) -> Self {
        self.transport = transport;
        self
    }

    /// How many tools this instance exposes, for the startup lifecycle event.
    #[must_use]
    pub fn tool_count(&self) -> usize {
        self.tool_router.list_all().len()
    }

    /// Resolve a tool call's `server` argument, or an `invalid_params` error
    /// naming the configured set. The only place an unrecognized `server`
    /// value is rejected: a lookup into an operator-configured allow-list,
    /// never a fetch to an arbitrary host.
    pub(crate) fn resolve_server(&self, selector: &str) -> Result<&ruoqa::Client, ErrorData> {
        self.servers.resolve(selector).ok_or_else(|| {
            ErrorData::invalid_params(
                format!(
                    "unknown server {selector:?}; configured servers: {}",
                    self.servers.identifiers().join(", ")
                ),
                None,
            )
        })
    }

    /// Fail-closed authorization for one tool call.
    fn authorize(&self, name: &str, context: &RequestContext<RoleServer>) -> Result<(), ErrorData> {
        if !self.enforce_scopes {
            return Ok(());
        }
        match scope_of(context) {
            Some(Scope::Write) => Ok(()),
            Some(Scope::Read) if self.tool_router.get(name).is_some_and(is_read_only) => Ok(()),
            // No scope means the auth middleware did not run: deny rather than
            // assume the transport was configured the way we expect.
            Some(Scope::Read) | None => Err(ErrorData::invalid_request(DENIED, None)),
        }
    }

    /// A tool call's audit scope: `read`/`write` from the same
    /// `read_only_hint` annotation [`is_read_only`] and [`authorize`] use, so
    /// the audit stream can never disagree with what the gate actually
    /// enforced.
    fn record_scope(&self, name: &str) -> RecordScope {
        if self.tool_router.get(name).is_some_and(is_read_only) {
            RecordScope::Read
        } else {
            RecordScope::Write
        }
    }

    /// GET (or other body-less request) under a heartbeat.
    pub(crate) async fn request_json(
        &self,
        ctx: &RequestContext<RoleServer>,
        client: &ruoqa::Client,
        method: Method,
        path: &str,
    ) -> ruoqa::Result<Value> {
        let token = ctx.meta.get_progress_token();
        let start = Instant::now();
        let result =
            with_heartbeat(&ctx.peer, token, client.request(method.clone(), path, None)).await;
        log_upstream_request(&method, path, start.elapsed(), &result);
        result
    }

    /// Form-encoded write under a heartbeat.
    pub(crate) async fn request_form(
        &self,
        ctx: &RequestContext<RoleServer>,
        client: &ruoqa::Client,
        method: Method,
        path: &str,
        form: &Form,
    ) -> ruoqa::Result<Value> {
        let token = ctx.meta.get_progress_token();
        let pairs = form.pairs();
        let start = Instant::now();
        let result = with_heartbeat(
            &ctx.peer,
            token,
            client.request_form(method.clone(), path, &pairs),
        )
        .await;
        log_upstream_request(&method, path, start.elapsed(), &result);
        result
    }

    /// The macro's body, gated on the caller's scope: `authorize` then
    /// dispatch under the whole-call deadline.
    async fn dispatch(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.authorize(&request.name, &context)?;
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        let dispatch = self.tool_router.call(tcc);
        match self.call_timeout {
            Some(timeout) => match tokio::time::timeout(timeout, dispatch).await {
                Ok(result) => result,
                Err(_) => Ok(tool_error(
                    "timeout",
                    None,
                    format!(
                        "tool call exceeded the {}s deadline (OPENQA_MCP_CALL_TIMEOUT); an \
                         in-flight write may already have been applied",
                        timeout.as_secs_f64()
                    ),
                    None,
                )?
                .into()),
            },
            None => dispatch.await,
        }
    }
}

/// DEBUG diagnostics for one upstream openQA request: method, path,
/// duration, and ok/error kind. Never the response body, never a query
/// string — a caller passes `path` as the bare resource path, and this
/// never looks at the response.
fn log_upstream_request(
    method: &Method,
    path: &str,
    elapsed: Duration,
    result: &ruoqa::Result<Value>,
) {
    let duration_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    match result {
        Ok(_) => {
            tracing::debug!(method = %method, path, duration_ms, ok = true, "upstream request");
        }
        Err(e) => tracing::debug!(
            method = %method,
            path,
            duration_ms,
            ok = false,
            "error.kind" = kind_of(e),
            "upstream request"
        ),
    }
}

/// `204 No Content` comes back as `Value::Null`; normalize to `{}` like the
/// Python did for its raw httpx response.
pub(crate) fn ok(value: Value) -> Result<CallToolResult, ErrorData> {
    let value = if value.is_null() { json!({}) } else { value };
    Ok(CallToolResult::success(vec![ContentBlock::json(value)?]))
}

pub(crate) fn to_result(result: ruoqa::Result<Value>) -> Result<CallToolResult, ErrorData> {
    match result {
        Ok(value) => ok(value),
        Err(e) => classify(e),
    }
}

/// A tool is non-mutating only if it says so itself. Derived from the
/// annotation rather than a name list so a tool added later cannot slip past
/// the gate by being forgotten.
fn is_read_only(tool: &Tool) -> bool {
    tool.annotations
        .as_ref()
        .is_some_and(|a| a.read_only_hint == Some(true))
}

/// The session id an audit record should carry: the `Mcp-Session-Id` request
/// header when present (HTTP, after the session that `initialize` created),
/// else `process_session` (stdio, or the `initialize` request itself).
fn session_of(context: &RequestContext<RoleServer>, process_session: &str) -> String {
    context
        .extensions
        .get::<axum::http::request::Parts>()
        .and_then(|parts| parts.headers.get("mcp-session-id"))
        .and_then(|value| value.to_str().ok())
        .map_or_else(|| process_session.to_string(), str::to_string)
}

/// [`rmcp::service::negotiate_protocol_version`]'s body (`rmcp` 3.1.3 keeps it
/// crate-private): echo the client's requested version if this server
/// supports it, else fall back to the server's own default.
fn negotiate_protocol_version(
    client_requested: &ProtocolVersion,
    server_fallback: ProtocolVersion,
    server_supported: &[ProtocolVersion],
) -> ProtocolVersion {
    if server_supported.contains(client_requested) {
        client_requested.clone()
    } else {
        server_fallback
    }
}

/// Extract an [`Outcome`] from a tool call's result, per the classification
/// table: a completed call with `is_error == Some(true)` reads back
/// `error.rs::tool_error`'s own JSON shape (`kind`/`status`), an
/// unparseable one (no such producer exists today) becomes `kind: "unknown"`
/// rather than failing the call, and a protocol-level `Err` carries its
/// JSON-RPC code.
fn outcome_of(result: &Result<CallToolResponse, ErrorData>) -> Outcome {
    match result {
        Ok(CallToolResponse::Complete(r)) if r.is_error == Some(true) => tool_error_outcome(r),
        // `InputRequired`/`Task` (and anything `#[non_exhaustive]` adds
        // later) are unreachable for this crate's tools, which use neither
        // elicitation nor the tasks extension; treat them as success.
        Ok(_) => Outcome::Ok,
        Err(e) => Outcome::ProtocolError { code: e.code.0 },
    }
}

fn tool_error_outcome(result: &CallToolResult) -> Outcome {
    let text = result.content.first().and_then(|c| match c {
        ContentBlock::Text(t) => Some(t.text.as_str()),
        _ => None,
    });
    let payload = text.and_then(|t| serde_json::from_str::<Value>(t).ok());
    let error = payload.as_ref().and_then(|v| v.get("error"));
    let kind = error
        .and_then(|e| e.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let status = error
        .and_then(|e| e.get("status"))
        .and_then(Value::as_u64)
        .and_then(|n| u16::try_from(n).ok());
    Outcome::ToolError { kind, status }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for OpenQaServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(INSTRUCTIONS)
    }

    /// The default impl's body (`rmcp` 3.1.3), plus a `session_open` audit
    /// record. Copied rather than delegated to `get_info()` alone, or
    /// protocol-version negotiation silently regresses: there is no
    /// `Mcp-Session-Id` yet at this point, so the record uses the
    /// per-process session id even over HTTP.
    fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<InitializeResult, ErrorData>> + rmcp::service::MaybeSendFuture + '_
    {
        context.peer.set_peer_info(request.clone());
        let mut info = self.get_info();
        info.protocol_version = negotiate_protocol_version(
            &request.protocol_version,
            info.protocol_version,
            &self.supported_protocol_versions(),
        );
        if let Some(audit) = &self.audit {
            audit.session_open(audit.process_session(), self.transport);
        }
        std::future::ready(Ok(info))
    }

    /// The macro's body, gated on the caller's scope, always producing a
    /// DEBUG diagnostics event and, when auditing is on, an audit record too
    /// — one classification (`outcome_of`/`record_scope`), never two. Timing,
    /// the `server` selector capture, and its resolution are hoisted out of
    /// the audit-only branch so the diagnostics event fires whether or not
    /// auditing is configured; only the audit-specific captures (arguments,
    /// session id) stay inside `if let Some(audit)`.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let start = Instant::now();
        let tool = request.name.to_string();
        let scope = self.record_scope(&tool);
        let server_selector = request
            .arguments
            .as_ref()
            .and_then(|a| a.get("server"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let audit = self.audit.clone();
        let args_and_session = audit.as_ref().map(|audit| {
            (
                audit::capture_args(request.arguments.as_ref()),
                session_of(&context, audit.process_session()),
            )
        });

        let result = self.dispatch(request, context).await;

        let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        let server = server_selector
            .as_deref()
            .and_then(|s| self.servers.resolve_id(s));
        let outcome = outcome_of(&result);
        let error_kind = match &outcome {
            Outcome::ToolError { kind, .. } => Some(kind.as_str()),
            Outcome::Ok | Outcome::ProtocolError { .. } => None,
        };
        tracing::debug!(
            tool = %tool,
            server = server.as_deref(),
            scope = ?scope,
            duration_ms,
            outcome = ?outcome,
            "error.kind" = error_kind,
            "tool call"
        );

        if let (Some(audit), Some((args, session))) = (audit, args_and_session) {
            audit.tool_call(
                session,
                self.transport,
                scope,
                tool,
                server,
                args,
                outcome,
                duration_ms,
            );
        }
        result
    }

    /// The macro's body, minus the tools this principal could not call anyway.
    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + rmcp::service::MaybeSendFuture + '_
    {
        let mut tools = self.tool_router.list_all();
        if self.enforce_scopes {
            match scope_of(&context) {
                Some(Scope::Write) => {}
                Some(Scope::Read) => tools.retain(is_read_only),
                None => return std::future::ready(Err(ErrorData::invalid_request(DENIED, None))),
            }
        }
        let supports_cache_hints = context
            .protocol_version()
            .is_some_and(|version| version >= rmcp::model::ProtocolVersion::V_2026_07_28);
        std::future::ready(Ok(ListToolsResult {
            result_type: Some(ResultType::COMPLETE),
            tools,
            meta: None,
            next_cursor: None,
            ttl_ms: supports_cache_hints.then_some(0),
            // Tool visibility depends on the caller's scope, so a shared cache
            // would hand a read principal the write list.
            cache_scope: supports_cache_hints.then_some(rmcp::model::CacheScope::Private),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_normalizes_null_to_empty_object() {
        let result = ok(Value::Null).unwrap();
        assert_eq!(result.structured_content, None);
        let ContentBlock::Text(text) = &result.content[0] else {
            panic!("expected text content");
        };
        assert_eq!(text.text, "{}");
    }

    fn fixture_registry() -> ServerRegistry {
        let mut clients = std::collections::HashMap::new();
        for id in ["one", "two"] {
            let client = ruoqa::ClientBuilder::new()
                .server(format!("https://{id}.example.com"))
                .config_paths(vec![])
                .build()
                .expect("build client");
            clients.insert(id.to_string(), client);
        }
        ServerRegistry::from_map(clients)
    }

    #[test]
    fn resolve_server_finds_a_known_id() {
        let server = OpenQaServer::new(fixture_registry(), false);
        assert!(server.resolve_server("one").is_ok());
        assert!(server.resolve_server("two").is_ok());
    }

    #[test]
    fn resolve_server_names_the_configured_set_for_an_unknown_id() {
        let server = OpenQaServer::new(fixture_registry(), false);
        let err = server.resolve_server("nope").unwrap_err();
        let message = err.message.to_string();
        assert!(message.contains("nope"), "{message}");
        assert!(message.contains("one"), "{message}");
        assert!(message.contains("two"), "{message}");
    }

    // Vertex rejects an `anyOf` with sibling keys, which is what a Gemini
    // client turns schemars' `Option<T>` union into; every tool schema must
    // come out already collapsed to a bare scalar type.
    #[test]
    fn tool_schemas_have_no_nullable_unions() {
        fn assert_no_union(node: &Value, path: &str) {
            match node {
                Value::Object(map) => {
                    if let Some(Value::Array(_)) = map.get("type") {
                        panic!("{path}: array \"type\" was not collapsed: {node}");
                    }
                    assert!(
                        map.get("default") != Some(&Value::Null),
                        "{path}: leftover \"default\": null at {node}"
                    );
                    for (key, value) in map {
                        assert_no_union(value, &format!("{path}.{key}"));
                    }
                }
                Value::Array(items) => {
                    for (i, item) in items.iter().enumerate() {
                        assert_no_union(item, &format!("{path}[{i}]"));
                    }
                }
                _ => {}
            }
        }

        let server = OpenQaServer::new(fixture_registry(), false);
        for tool in server.tool_router.list_all() {
            assert_no_union(&Value::Object((*tool.input_schema).clone()), &tool.name);
            if let Some(output_schema) = &tool.output_schema {
                assert_no_union(&Value::Object((**output_schema).clone()), &tool.name);
            }
        }
    }
}

#[cfg(test)]
mod router_tests {
    use std::collections::BTreeSet;

    use super::*;

    // Matches the README's "Read tools" table exactly.
    const READ_TOOL_NAMES: &[&str] = &[
        "list_jobs",
        "list_jobs_overview",
        "get_job",
        "get_job_comments",
        "list_machines",
        "list_test_suites",
        "list_products",
        "find_jobs_by_setting",
        "get_job_details",
        "get_job_status",
        "list_job_groups",
        "get_job_group",
        "list_job_group_jobs",
        "get_job_group_build_results",
        "list_parent_groups",
        "get_parent_group",
        "list_assets",
        "get_asset",
        "list_workers",
        "list_bugs",
        "search",
        "get_scheduled_product",
        "get_iso_job_stats",
        "list_group_comments",
        "list_parent_group_comments",
        "list_job_logs",
        "list_job_log_members",
        "get_job_log",
        "get_job_log_errors",
        "list_servers",
    ];

    // Matches the README's "Mutating tools" table exactly.
    const WRITE_TOOL_NAMES: &[&str] = &[
        "restart_jobs",
        "cancel_job",
        "add_job_comment",
        "trigger_isos",
        "delete_job",
        "duplicate_job",
        "set_job_priority",
        "cancel_jobs",
        "add_group_comment",
        "add_parent_group_comment",
        "update_job_comment",
        "delete_job_comment",
        "create_bug",
        "cancel_scheduled_product",
    ];

    fn names(router: &ToolRouter<OpenQaServer>) -> BTreeSet<String> {
        router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect()
    }

    #[test]
    fn read_router_matches_readme_table() {
        assert_eq!(READ_TOOL_NAMES.len(), 30);
        let expected: BTreeSet<String> = READ_TOOL_NAMES
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        assert_eq!(names(&OpenQaServer::read_tool_router()), expected);
    }

    #[test]
    fn write_router_matches_readme_table() {
        assert_eq!(WRITE_TOOL_NAMES.len(), 14);
        let expected: BTreeSet<String> = WRITE_TOOL_NAMES
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        assert_eq!(names(&OpenQaServer::write_tool_router()), expected);
    }

    #[test]
    fn readonly_excludes_write_tools() {
        let full = OpenQaServer::read_tool_router() + OpenQaServer::write_tool_router();
        assert_eq!(full.list_all().len(), 44);
    }

    // The scope gate reads `read_only_hint`, so an unannotated (or inverted)
    // tool would silently become callable by a read-scope principal.
    #[test]
    fn annotations_agree_with_the_routers() {
        for tool in OpenQaServer::read_tool_router().list_all() {
            assert!(
                is_read_only(&tool),
                "{} must be read_only_hint=true",
                tool.name
            );
        }
        for tool in OpenQaServer::write_tool_router().list_all() {
            assert!(
                !is_read_only(&tool),
                "{} must not be read_only_hint=true",
                tool.name
            );
        }
    }
}
