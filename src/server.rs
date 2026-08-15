//! The `OpenQaServer` handler and the request funnel (port of `server.py`'s
//! `mcp`, `_client`, and `_request`).

use std::time::Duration;

use reqwest::Method;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ResultType, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, tool_handler};
use serde_json::{Value, json};

use crate::form::Form;
use crate::heartbeat::with_heartbeat;
use crate::http::{Scope, scope_of};

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
    pub(crate) client: ruoqa::Client,
    tool_router: ToolRouter<Self>,
    /// Whether each request must carry a [`Scope`]. Off for stdio, where the
    /// process credential is the only principal there is.
    enforce_scopes: bool,
    /// Whole-call deadline, independent of the per-HTTP-request timeout.
    /// `None` disables it.
    call_timeout: Option<Duration>,
}

impl OpenQaServer {
    /// Merges the read router with the write router unless `readonly`, which
    /// keeps `--readonly` a compile-time-guaranteed subset rather than a
    /// hand-maintained list of mutating tool names to disable at runtime.
    #[must_use]
    pub fn new(client: ruoqa::Client, readonly: bool) -> Self {
        let tool_router = if readonly {
            Self::read_tool_router()
        } else {
            Self::read_tool_router() + Self::write_tool_router()
        };
        Self {
            client,
            tool_router,
            enforce_scopes: false,
            call_timeout: Some(DEFAULT_CALL_TIMEOUT),
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

    /// GET (or other body-less request) under a heartbeat.
    pub(crate) async fn request_json(
        &self,
        ctx: &RequestContext<RoleServer>,
        method: Method,
        path: &str,
    ) -> ruoqa::Result<Value> {
        let token = ctx.meta.get_progress_token();
        with_heartbeat(&ctx.peer, token, self.client.request(method, path, None)).await
    }

    /// Form-encoded write under a heartbeat.
    pub(crate) async fn request_form(
        &self,
        ctx: &RequestContext<RoleServer>,
        method: Method,
        path: &str,
        form: &Form,
    ) -> ruoqa::Result<Value> {
        let token = ctx.meta.get_progress_token();
        let pairs = form.pairs();
        with_heartbeat(
            &ctx.peer,
            token,
            self.client.request_form(method, path, &pairs),
        )
        .await
    }
}

/// `204 No Content` comes back as `Value::Null`; normalize to `{}` like the
/// Python did for its raw httpx response.
pub(crate) fn ok(value: Value) -> Result<CallToolResult, ErrorData> {
    let value = if value.is_null() { json!({}) } else { value };
    Ok(CallToolResult::success(vec![ContentBlock::json(value)?]))
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "used as map_err(err), which requires a by-value fn"
)]
pub(crate) fn err(e: ruoqa::Error) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

pub(crate) fn to_result(result: ruoqa::Result<Value>) -> Result<CallToolResult, ErrorData> {
    result.map_err(err).and_then(ok)
}

/// A tool is non-mutating only if it says so itself. Derived from the
/// annotation rather than a name list so a tool added later cannot slip past
/// the gate by being forgotten.
fn is_read_only(tool: &Tool) -> bool {
    tool.annotations
        .as_ref()
        .is_some_and(|a| a.read_only_hint == Some(true))
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for OpenQaServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(INSTRUCTIONS)
    }

    /// The macro's body, gated on the caller's scope.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.authorize(&request.name, &context)?;
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        let dispatch = self.tool_router.call(tcc);
        match self.call_timeout {
            Some(timeout) => tokio::time::timeout(timeout, dispatch)
                .await
                .unwrap_or_else(|_| {
                    Err(ErrorData::internal_error(
                        format!(
                            "tool call exceeded the {}s deadline (OPENQA_MCP_CALL_TIMEOUT)",
                            timeout.as_secs_f64()
                        ),
                        None,
                    ))
                }),
            None => dispatch.await,
        }
    }

    /// The macro's body, minus the tools this principal could not call anyway.
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut tools = self.tool_router.list_all();
        if self.enforce_scopes {
            match scope_of(&context) {
                Some(Scope::Write) => {}
                Some(Scope::Read) => tools.retain(is_read_only),
                None => return Err(ErrorData::invalid_request(DENIED, None)),
            }
        }
        let supports_cache_hints = context
            .protocol_version()
            .is_some_and(|version| version >= rmcp::model::ProtocolVersion::V_2026_07_28);
        Ok(ListToolsResult {
            result_type: Some(ResultType::COMPLETE),
            tools,
            meta: None,
            next_cursor: None,
            ttl_ms: supports_cache_hints.then_some(0),
            // Tool visibility depends on the caller's scope, so a shared cache
            // would hand a read principal the write list.
            cache_scope: supports_cache_hints.then_some(rmcp::model::CacheScope::Private),
        })
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
        assert_eq!(READ_TOOL_NAMES.len(), 25);
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
        assert_eq!(full.list_all().len(), 39);
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
