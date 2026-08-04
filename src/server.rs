//! The `OpenQaServer` handler and the request funnel (port of `server.py`'s
//! `mcp`, `_client`, and `_request`).

use reqwest::Method;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, tool_handler};
use serde_json::{Value, json};

use crate::form::Form;
use crate::heartbeat::with_heartbeat;

const INSTRUCTIONS: &str = "Query and control an openQA instance. Read tools inspect jobs, \
machines, test suites, and products; mutating tools restart/cancel/delete jobs, comment, and \
trigger ISOs, and require API credentials.";

#[derive(Clone)]
pub struct OpenQaServer {
    pub(crate) client: ruoqa::Client,
    tool_router: ToolRouter<Self>,
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

#[tool_handler(router = self.tool_router)]
impl ServerHandler for OpenQaServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(INSTRUCTIONS)
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
        "restart_jobs_bulk",
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
        assert_eq!(WRITE_TOOL_NAMES.len(), 15);
        let expected: BTreeSet<String> = WRITE_TOOL_NAMES
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        assert_eq!(names(&OpenQaServer::write_tool_router()), expected);
    }

    #[test]
    fn readonly_excludes_write_tools() {
        let full = OpenQaServer::read_tool_router() + OpenQaServer::write_tool_router();
        assert_eq!(full.list_all().len(), 40);
    }
}
