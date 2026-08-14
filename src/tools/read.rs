//! The 25 read tools (port of the READ section of `server.py`).

use reqwest::Method;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::query::{Query, api};
use crate::server::{OpenQaServer, to_result};
use crate::summary::summarize_jobs;
use crate::tools::{MAX_IDS, bounded};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListJobsArgs {
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub distri: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub build: Option<String>,
    #[serde(default)]
    pub test: Option<String>,
    #[serde(default)]
    pub arch: Option<String>,
    #[serde(default)]
    pub machine: Option<String>,
    #[serde(default)]
    pub groupid: Option<i64>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub latest: Option<i64>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
    #[serde(default)]
    #[schemars(length(max = MAX_IDS))]
    pub ids: Option<Vec<i64>>,
    #[serde(default)]
    pub summary: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListJobsOverviewArgs {
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub distri: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub build: Option<String>,
    #[serde(default)]
    pub test: Option<String>,
    #[serde(default)]
    pub arch: Option<String>,
    #[serde(default)]
    pub machine: Option<String>,
    #[serde(default)]
    pub groupid: Option<i64>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub latest: Option<i64>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    #[schemars(length(max = MAX_IDS))]
    pub ids: Option<Vec<i64>>,
    #[serde(default)]
    pub summary: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct JobId {
    pub job_id: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindJobsBySetting {
    pub key: String,
    pub list_value: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct JobStatus {
    pub job_id: i64,
    #[serde(default)]
    pub follow: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GroupId {
    pub group_id: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BuildResults {
    pub group_id: i64,
    #[serde(default)]
    pub limit_builds: Option<i64>,
    #[serde(default)]
    pub time_limit_days: Option<f64>,
    #[serde(default)]
    pub only_tagged: Option<i64>,
    #[serde(default)]
    pub show_tags: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AssetId {
    pub asset_id: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Search {
    pub q: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScheduledProductId {
    pub scheduled_product_id: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ParentGroupId {
    pub parent_group_id: i64,
}

#[tool_router(router = read_tool_router, vis = "pub(crate)")]
impl OpenQaServer {
    #[tool(
        description = "List jobs matching the given filters. WARNING: the full result can be very \
large (~1.5 MB / 150+ jobs for a populated build) and may be truncated by MCP clients. For triage, \
pass summary=True for a compact per-result breakdown. To work with the full data, save it to a \
temporary file and process it with jq, e.g. `jq '.jobs[] | select(.result==\"failed\")'`.",
        annotations(read_only_hint = true)
    )]
    async fn list_jobs(
        &self,
        Parameters(args): Parameters<ListJobsArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        bounded("ids", args.ids.as_ref().map_or(0, Vec::len), 0, MAX_IDS)?;
        let path = Query::new()
            .push("state", args.state)
            .push("result", args.result)
            .push("distri", args.distri)
            .push("version", args.version)
            .push("build", args.build)
            .push("test", args.test)
            .push("arch", args.arch)
            .push("machine", args.machine)
            .push("groupid", args.groupid)
            .push("group", args.group)
            .push("latest", args.latest)
            .push("limit", args.limit)
            .push("offset", args.offset)
            .push_all("ids", args.ids.as_deref())
            .finish(&api("jobs"));
        let body = self.request_json(&ctx, Method::GET, &path).await;
        match body {
            Ok(value) if args.summary && value.is_object() => {
                let jobs = value
                    .get("jobs")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                to_result(Ok(summarize_jobs(&jobs)))
            }
            other => to_result(other),
        }
    }

    #[tool(
        description = "List a condensed jobs overview matching the given filters. WARNING: the full \
result can be very large (~1.5 MB / 150+ jobs for a populated build) and may be truncated by MCP \
clients. For triage, pass summary=True for a compact per-result breakdown. To work with the full \
data, save it to a temporary file and process it with jq, e.g. `jq '.jobs[] | select(.result==\"failed\")'`.",
        annotations(read_only_hint = true)
    )]
    async fn list_jobs_overview(
        &self,
        Parameters(args): Parameters<ListJobsOverviewArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        bounded("ids", args.ids.as_ref().map_or(0, Vec::len), 0, MAX_IDS)?;
        let path = Query::new()
            .push("state", args.state)
            .push("result", args.result)
            .push("distri", args.distri)
            .push("version", args.version)
            .push("build", args.build)
            .push("test", args.test)
            .push("arch", args.arch)
            .push("machine", args.machine)
            .push("groupid", args.groupid)
            .push("group", args.group)
            .push("latest", args.latest)
            .push("limit", args.limit)
            .push_all("ids", args.ids.as_deref())
            .finish(&api("jobs/overview"));
        let body = self.request_json(&ctx, Method::GET, &path).await;
        match body {
            Ok(value) if args.summary => {
                let jobs = if let Some(arr) = value.as_array() {
                    arr.clone()
                } else {
                    value
                        .get("jobs")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default()
                };
                to_result(Ok(summarize_jobs(&jobs)))
            }
            other => to_result(other),
        }
    }

    #[tool(
        description = "Get full details for a single job.",
        annotations(read_only_hint = true)
    )]
    async fn get_job(
        &self,
        Parameters(JobId { job_id }): Parameters<JobId>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        to_result(
            self.request_json(&ctx, Method::GET, &api(&format!("jobs/{job_id}")))
                .await,
        )
    }

    #[tool(
        description = "List comments on a job.",
        annotations(read_only_hint = true)
    )]
    async fn get_job_comments(
        &self,
        Parameters(JobId { job_id }): Parameters<JobId>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        to_result(
            self.request_json(&ctx, Method::GET, &api(&format!("jobs/{job_id}/comments")))
                .await,
        )
    }

    #[tool(
        description = "List configured worker machines.",
        annotations(read_only_hint = true)
    )]
    async fn list_machines(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        to_result(self.request_json(&ctx, Method::GET, &api("machines")).await)
    }

    #[tool(
        description = "List configured test suites.",
        annotations(read_only_hint = true)
    )]
    async fn list_test_suites(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        to_result(
            self.request_json(&ctx, Method::GET, &api("test_suites"))
                .await,
        )
    }

    #[tool(
        description = "List configured products (mediums).",
        annotations(read_only_hint = true)
    )]
    async fn list_products(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        to_result(self.request_json(&ctx, Method::GET, &api("products")).await)
    }

    #[tool(
        description = "Find jobs whose setting `key` equals `list_value`.",
        annotations(read_only_hint = true)
    )]
    async fn find_jobs_by_setting(
        &self,
        Parameters(FindJobsBySetting { key, list_value }): Parameters<FindJobsBySetting>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let path = Query::new()
            .push("key", Some(key))
            .push("list_value", Some(list_value))
            .finish(&api("job_settings/jobs"));
        to_result(self.request_json(&ctx, Method::GET, &path).await)
    }

    #[tool(
        description = "Get a single job with full test-module/step details.",
        annotations(read_only_hint = true)
    )]
    async fn get_job_details(
        &self,
        Parameters(JobId { job_id }): Parameters<JobId>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        to_result(
            self.request_json(&ctx, Method::GET, &api(&format!("jobs/{job_id}/details")))
                .await,
        )
    }

    #[tool(
        description = "Get a lightweight job status (id, state, result, blocked_by_id).",
        annotations(read_only_hint = true)
    )]
    async fn get_job_status(
        &self,
        Parameters(JobStatus { job_id, follow }): Parameters<JobStatus>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let path = Query::new()
            .push("follow", follow)
            .finish(&api(&format!("experimental/jobs/{job_id}/status")));
        to_result(self.request_json(&ctx, Method::GET, &path).await)
    }

    #[tool(description = "List job groups.", annotations(read_only_hint = true))]
    async fn list_job_groups(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        to_result(
            self.request_json(&ctx, Method::GET, &api("job_groups"))
                .await,
        )
    }

    #[tool(
        description = "Get a single job group.",
        annotations(read_only_hint = true)
    )]
    async fn get_job_group(
        &self,
        Parameters(GroupId { group_id }): Parameters<GroupId>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        to_result(
            self.request_json(&ctx, Method::GET, &api(&format!("job_groups/{group_id}")))
                .await,
        )
    }

    #[tool(
        description = "List jobs belonging to a job group.",
        annotations(read_only_hint = true)
    )]
    async fn list_job_group_jobs(
        &self,
        Parameters(GroupId { group_id }): Parameters<GroupId>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        to_result(
            self.request_json(
                &ctx,
                Method::GET,
                &api(&format!("job_groups/{group_id}/jobs")),
            )
            .await,
        )
    }

    #[tool(
        description = "Get aggregated build results for a job group.",
        annotations(read_only_hint = true)
    )]
    async fn get_job_group_build_results(
        &self,
        Parameters(BuildResults {
            group_id,
            limit_builds,
            time_limit_days,
            only_tagged,
            show_tags,
        }): Parameters<BuildResults>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let path = Query::new()
            .push("limit_builds", limit_builds)
            .push("time_limit_days", time_limit_days)
            .push("only_tagged", only_tagged)
            .push("show_tags", show_tags)
            .finish(&api(&format!("job_groups/{group_id}/build_results")));
        to_result(self.request_json(&ctx, Method::GET, &path).await)
    }

    #[tool(
        description = "List parent job groups.",
        annotations(read_only_hint = true)
    )]
    async fn list_parent_groups(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        to_result(
            self.request_json(&ctx, Method::GET, &api("parent_groups"))
                .await,
        )
    }

    #[tool(
        description = "Get a single parent job group.",
        annotations(read_only_hint = true)
    )]
    async fn get_parent_group(
        &self,
        Parameters(GroupId { group_id }): Parameters<GroupId>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        to_result(
            self.request_json(
                &ctx,
                Method::GET,
                &api(&format!("parent_groups/{group_id}")),
            )
            .await,
        )
    }

    #[tool(
        description = "List assets known to the system.",
        annotations(read_only_hint = true)
    )]
    async fn list_assets(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        to_result(self.request_json(&ctx, Method::GET, &api("assets")).await)
    }

    #[tool(
        description = "Get a single asset by id.",
        annotations(read_only_hint = true)
    )]
    async fn get_asset(
        &self,
        Parameters(AssetId { asset_id }): Parameters<AssetId>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        to_result(
            self.request_json(&ctx, Method::GET, &api(&format!("assets/{asset_id}")))
                .await,
        )
    }

    #[tool(
        description = "List registered worker instances.",
        annotations(read_only_hint = true)
    )]
    async fn list_workers(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        to_result(self.request_json(&ctx, Method::GET, &api("workers")).await)
    }

    #[tool(
        description = "List tracked bugs referenced by jobs.",
        annotations(read_only_hint = true)
    )]
    async fn list_bugs(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        to_result(self.request_json(&ctx, Method::GET, &api("bugs")).await)
    }

    #[tool(
        description = "Full-text search across jobs, groups, and test modules.",
        annotations(read_only_hint = true)
    )]
    async fn search(
        &self,
        Parameters(Search { q }): Parameters<Search>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let path = Query::new()
            .push("q", Some(q))
            .finish(&api("experimental/search"));
        to_result(self.request_json(&ctx, Method::GET, &path).await)
    }

    #[tool(
        description = "Get a scheduled product (result of a prior ISO trigger).",
        annotations(read_only_hint = true)
    )]
    async fn get_scheduled_product(
        &self,
        Parameters(ScheduledProductId {
            scheduled_product_id,
        }): Parameters<ScheduledProductId>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        to_result(
            self.request_json(
                &ctx,
                Method::GET,
                &api(&format!("isos/{scheduled_product_id}")),
            )
            .await,
        )
    }

    #[tool(
        description = "Get job statistics for scheduled products.",
        annotations(read_only_hint = true)
    )]
    async fn get_iso_job_stats(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        to_result(
            self.request_json(&ctx, Method::GET, &api("isos/job_stats"))
                .await,
        )
    }

    #[tool(
        description = "List comments on a job group.",
        annotations(read_only_hint = true)
    )]
    async fn list_group_comments(
        &self,
        Parameters(GroupId { group_id }): Parameters<GroupId>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        to_result(
            self.request_json(
                &ctx,
                Method::GET,
                &api(&format!("groups/{group_id}/comments")),
            )
            .await,
        )
    }

    #[tool(
        description = "List comments on a parent job group.",
        annotations(read_only_hint = true)
    )]
    async fn list_parent_group_comments(
        &self,
        Parameters(ParentGroupId { parent_group_id }): Parameters<ParentGroupId>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        to_result(
            self.request_json(
                &ctx,
                Method::GET,
                &api(&format!("parent_groups/{parent_group_id}/comments")),
            )
            .await,
        )
    }
}
