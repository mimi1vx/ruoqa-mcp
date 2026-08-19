//! The 29 read tools (port of the READ section of `server.py`, plus the job-
//! log-artifact tools that have no equivalent there).

use reqwest::Method;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::classify;
use crate::query::{Query, api};
use crate::server::{OpenQaServer, ok, to_result};
use crate::summary::summarize_jobs;
use crate::tools::artifact;
use crate::tools::{
    DIGEST_CONTEXT_LINES, DIGEST_MAX_HITS, DIGEST_TAIL_LINES, MAX_ARCHIVE_MEMBERS,
    MAX_ARTIFACT_BYTES, MAX_IDS, PROBE_BYTES, bounded,
};

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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct JobLogFile {
    pub job_id: i64,
    pub filename: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetJobLog {
    pub job_id: i64,
    pub filename: String,
    /// A path inside a tar/tar.gz/tar.xz archive to extract, from
    /// `list_job_log_members`. Required when `filename` is such an archive.
    #[serde(default)]
    pub member: Option<String>,
    /// Lowers the response-size ceiling; never raises it above the server's
    /// own limit.
    #[serde(default)]
    #[schemars(range(max = MAX_ARTIFACT_BYTES))]
    pub max_bytes: Option<usize>,
    /// Return only the last N lines. For a plain-text file with no
    /// `member`, this is served by a byte-ranged request instead of a full
    /// download.
    #[serde(default)]
    pub tail_lines: Option<usize>,
    /// A regex; only matching lines (plus `context_lines` of surrounding
    /// context) are returned, as `matches` instead of `content`.
    #[serde(default)]
    pub grep: Option<String>,
    /// Lines of context to include around each `grep` match. Ignored
    /// without `grep`.
    #[serde(default)]
    pub context_lines: Option<usize>,
    /// Caps how many matching lines `grep` expands into context; the true
    /// count is still reported as `total_matches`. Ignored without `grep`.
    #[serde(default)]
    pub max_matches: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetJobLogErrors {
    pub job_id: i64,
    /// Regex patterns that replace the whole tier chain: scans `filename`
    /// (default `autoinst-log.txt`) for any of these instead, still falling
    /// back to `tail` when none match.
    #[serde(default)]
    pub markers: Option<Vec<String>>,
    /// File to scan for tiers 2-4 instead of `autoinst-log.txt`. Skips the
    /// `serial_terminal.txt` tier, which is tied to that one file.
    #[serde(default)]
    pub filename: Option<String>,
}

/// Pull the `jobs` array a summary is built from, rejecting any shape other
/// than openQA's documented ones. `list_jobs` always renders `{"jobs": \
/// [...]}`; `list_jobs_overview` renders a bare array. A mismatch here is an
/// upstream-contract violation, not a caller error, so it is reported as
/// `internal_error` without echoing the (potentially huge) response body.
fn jobs_array<'a>(
    tool: &str,
    value: &'a Value,
    allow_top_level_array: bool,
) -> Result<&'a Vec<Value>, ErrorData> {
    if allow_top_level_array && let Some(arr) = value.as_array() {
        return Ok(arr);
    }
    value.get("jobs").and_then(Value::as_array).ok_or_else(|| {
        let expected = if allow_top_level_array {
            "a bare array or an object with a \"jobs\" array"
        } else {
            "an object with a \"jobs\" array"
        };
        ErrorData::internal_error(
            format!("{tool}: expected {expected} from openQA, got a different shape"),
            None,
        )
    })
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
            Ok(value) if args.summary => {
                let jobs = jobs_array("list_jobs", &value, false)?;
                to_result(Ok(summarize_jobs(jobs)))
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
                let jobs = jobs_array("list_jobs_overview", &value, true)?;
                to_result(Ok(summarize_jobs(jobs)))
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

    #[tool(
        description = "List a job's downloadable log files and uploaded (ulog) files, e.g. \
autoinst-log.txt or a supportconfig .txz. Pass a name to `get_job_log`, or to \
`list_job_log_members` first if it looks like an archive.",
        annotations(read_only_hint = true)
    )]
    async fn list_job_logs(
        &self,
        Parameters(JobId { job_id }): Parameters<JobId>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let ajax_path = format!("/tests/{job_id}/downloads_ajax");
        if let Ok(Value::String(html)) = self.request_json(&ctx, Method::GET, &ajax_path).await {
            let files = artifact::parse_downloads(&html, job_id);
            if !files.is_empty() {
                return ok(json!({"source": "downloads_ajax", "files": files}));
            }
        }
        match self
            .request_json(&ctx, Method::GET, &api(&format!("jobs/{job_id}/details")))
            .await
        {
            Ok(value) => ok(json!({
                "source": "details",
                "files": artifact::details_logs(&value),
            })),
            Err(e) => classify(e),
        }
    }

    #[tool(
        description = "List the members of a job log archive (tar, tar.gz, or tar.xz), e.g. \
the files inside a supportconfig .txz. Pass one entry's `path` as `member` to `get_job_log`.",
        annotations(read_only_hint = true)
    )]
    async fn list_job_log_members(
        &self,
        Parameters(JobLogFile { job_id, filename }): Parameters<JobLogFile>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        artifact::validate_filename(&filename)?;
        let probe = match artifact::probe(
            &self.client,
            &ctx,
            job_id,
            &filename,
            PROBE_BYTES,
            MAX_ARTIFACT_BYTES,
        )
        .await
        {
            Ok(p) => p,
            Err(bail) => return bail,
        };
        let content = match artifact::fetch_decoded(
            &self.client,
            &ctx,
            job_id,
            &filename,
            &probe,
            MAX_ARTIFACT_BYTES,
        )
        .await
        {
            Ok(c) => c,
            Err(bail) => return bail,
        };
        match artifact::tar_members(&content, MAX_ARCHIVE_MEMBERS) {
            Ok((members, truncated)) => ok(json!({"members": members, "truncated": truncated})),
            Err(bail) => bail,
        }
    }

    #[tool(
        description = "Read a job log or uploaded file. For a large plain-text log, pass \
`tail_lines` to fetch only the end of it (a byte-ranged request, not a full download); pass \
`grep` (a regex, with `context_lines` around each match) to search it instead of returning \
the whole thing. Archives (tar, tar.gz, tar.xz) are decoded automatically — pass `member` \
(from `list_job_log_members`) to read one entry. Binary artifacts (images, videos) are \
refused with an `unsupported_media` error.",
        annotations(read_only_hint = true)
    )]
    async fn get_job_log(
        &self,
        Parameters(GetJobLog {
            job_id,
            filename,
            member,
            max_bytes,
            tail_lines,
            grep,
            context_lines,
            max_matches,
        }): Parameters<GetJobLog>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        artifact::validate_filename(&filename)?;
        if let Some(member) = &member {
            artifact::validate_member(member)?;
        }
        let ceiling = max_bytes.map_or(MAX_ARTIFACT_BYTES, |m| m.min(MAX_ARTIFACT_BYTES));
        let context_lines = context_lines.unwrap_or(0);
        let max_matches = max_matches.unwrap_or(artifact::DEFAULT_MAX_MATCHES);

        let probe = match artifact::probe(
            &self.client,
            &ctx,
            job_id,
            &filename,
            PROBE_BYTES,
            ceiling,
        )
        .await
        {
            Ok(p) => p,
            Err(bail) => return bail,
        };
        let needs_full_decode =
            member.is_some() || artifact::sniff(&probe.body) != artifact::Sniff::Plain;

        let (raw_text, transferred, size, changed_during_read) = if needs_full_decode {
            let content = match artifact::fetch_decoded(
                &self.client,
                &ctx,
                job_id,
                &filename,
                &probe,
                ceiling,
            )
            .await
            {
                Ok(c) => c,
                Err(bail) => return bail,
            };
            let final_bytes = if let Some(member) = &member {
                match artifact::extract_tar_member(&content, member, ceiling) {
                    Ok(b) => b,
                    Err(bail) => return bail,
                }
            } else if artifact::sniff(&content) == artifact::Sniff::Tar {
                return Err(ErrorData::invalid_params(
                    format!(
                        "{filename:?} is an archive; pass `member` (see list_job_log_members) to read one entry"
                    ),
                    None,
                ));
            } else {
                content
            };
            let len = final_bytes.len() as u64;
            let Ok(text) = String::from_utf8(final_bytes) else {
                return artifact::unsupported_media(&probe.url, len);
            };
            (text, len, probe.size.map_or(len, |s| s.max(len)), false)
        } else {
            match tail_lines {
                Some(n) if !probe.complete => {
                    let window = artifact::tail_window(n, probe.size, ceiling as u64);
                    let tail = match artifact::fetch_tail(
                        &self.client,
                        &ctx,
                        job_id,
                        &filename,
                        &probe,
                        window,
                        ceiling,
                    )
                    .await
                    {
                        Ok(t) => t,
                        Err(bail) => return bail,
                    };
                    let len = tail.bytes.len() as u64;
                    let Ok(text) = String::from_utf8(tail.bytes) else {
                        // `len` is the tail window, not the file: report the probe's total.
                        return artifact::unsupported_media(&probe.url, probe.size.unwrap_or(len));
                    };
                    let text = artifact::drop_partial_first_line(&text).to_string();
                    (
                        text,
                        len,
                        probe.size.unwrap_or(len),
                        tail.changed_during_read,
                    )
                }
                _ => {
                    let raw = if probe.complete {
                        probe.body.clone()
                    } else {
                        match artifact::fetch_all(&self.client, &ctx, job_id, &filename, ceiling)
                            .await
                        {
                            Ok(b) => b,
                            Err(bail) => return bail,
                        }
                    };
                    let len = raw.len() as u64;
                    let Ok(text) = String::from_utf8(raw) else {
                        return artifact::unsupported_media(&probe.url, len);
                    };
                    (text, len, probe.size.unwrap_or(len), false)
                }
            }
        };

        let mut reply = json!({"filename": filename});
        if let Some(member) = &member {
            reply["member"] = json!(member);
        }
        match artifact::slice(
            &raw_text,
            tail_lines,
            grep.as_deref(),
            context_lines,
            max_matches,
        )? {
            artifact::Sliced::Text(content) => {
                reply["bytes"] = json!(transferred);
                reply["size"] = json!(size);
                reply["content"] = json!(content);
                if changed_during_read {
                    reply["changed_during_read"] = json!(true);
                }
            }
            artifact::Sliced::Matches(m) => {
                reply["matches"] = json!(m.hits);
                reply["total_matches"] = json!(m.total);
                reply["truncated"] = json!(m.truncated);
            }
        }
        ok(reply)
    }

    #[tool(
        description = "Digest a job's logs down to the failure signal instead of returning the \
whole file. Checks, in order: `serial_terminal.txt` for LTP/TAP-style TFAIL/TBROK (the real \
verdict for serial-console-driven frameworks, never duplicated into autoinst-log.txt); \
autoinst-log.txt for \"Test died\"; a generic error/timeout fallback; finally just the tail. The \
first tier that matches wins. Pass `markers` (a list of regexes) to scan `filename` (default \
autoinst-log.txt) for something else entirely instead of the tier chain. Also names the failing \
test module(s), each with a `#step/<module>/<num>` deep link, from a best-effort `/details` fetch.",
        annotations(read_only_hint = true)
    )]
    async fn get_job_log_errors(
        &self,
        Parameters(GetJobLogErrors {
            job_id,
            markers,
            filename,
        }): Parameters<GetJobLogErrors>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(name) = &filename {
            artifact::validate_filename(name)?;
        }
        let custom_set = match &markers {
            Some(patterns) => Some(artifact::compile_markers(patterns)?),
            None => None,
        };
        let filename_given = filename.is_some();
        let scan_file = filename.unwrap_or_else(|| "autoinst-log.txt".to_string());

        // Best-effort: a failed `/details` fetch just means no
        // `failed_modules` and no serial_terminal.txt probe-skip, never an
        // aborted digest.
        let details = self
            .request_json(&ctx, Method::GET, &api(&format!("jobs/{job_id}/details")))
            .await
            .ok();
        let failed_modules = details
            .as_ref()
            .map(|v| artifact::failed_modules(v, job_id, self.client.base_url()));

        if let Some(set) = &custom_set {
            let text = match artifact::fetch_text_lossy(
                &self.client,
                &ctx,
                job_id,
                &scan_file,
                MAX_ARTIFACT_BYTES,
            )
            .await
            {
                Ok(t) => t,
                Err(bail) => return bail,
            };
            let scan = artifact::scan_markers(&text, set, DIGEST_CONTEXT_LINES, DIGEST_MAX_HITS);
            return digest_reply(
                job_id,
                "custom",
                &scan_file,
                &with_tail(scan, &text),
                failed_modules,
            );
        }

        if !filename_given {
            let has_serial = details
                .as_ref()
                .is_some_and(|v| artifact::has_log(v, "serial_terminal.txt"));
            if has_serial {
                let text = match artifact::fetch_text_lossy(
                    &self.client,
                    &ctx,
                    job_id,
                    "serial_terminal.txt",
                    MAX_ARTIFACT_BYTES,
                )
                .await
                {
                    Ok(t) => t,
                    Err(bail) => return bail,
                };
                let scan = artifact::scan_markers(
                    &text,
                    &artifact::TAP_MARKERS,
                    DIGEST_CONTEXT_LINES,
                    DIGEST_MAX_HITS,
                );
                if scan.hit_count > 0 {
                    return digest_reply(
                        job_id,
                        "tap",
                        "serial_terminal.txt",
                        &with_tail(scan, &text),
                        failed_modules,
                    );
                }
            }
        }

        let text = match artifact::fetch_text_lossy(
            &self.client,
            &ctx,
            job_id,
            &scan_file,
            MAX_ARTIFACT_BYTES,
        )
        .await
        {
            Ok(t) => t,
            Err(bail) => return bail,
        };

        let died = artifact::scan_markers(
            &text,
            &artifact::FATAL_MARKERS,
            DIGEST_CONTEXT_LINES,
            DIGEST_MAX_HITS,
        );
        if died.hit_count > 0 {
            return digest_reply(
                job_id,
                "test_died",
                &scan_file,
                &with_tail(died, &text),
                failed_modules,
            );
        }

        let generic = artifact::scan_markers(
            &text,
            &artifact::NOISE_MARKERS,
            DIGEST_CONTEXT_LINES,
            DIGEST_MAX_HITS,
        );
        if generic.hit_count > 0 {
            return digest_reply(
                job_id,
                "generic",
                &scan_file,
                &with_tail(generic, &text),
                failed_modules,
            );
        }

        let scan = artifact::ScanResult {
            hits: artifact::tail_hits(&text, DIGEST_TAIL_LINES),
            hit_count: 0,
            more_hits: false,
            total_lines: text.lines().count(),
        };
        digest_reply(job_id, "tail", &scan_file, &scan, failed_modules)
    }
}

/// Appends the tail to `scan`'s hits when [`artifact::TAIL_ALWAYS`] is
/// flipped on; a no-op by default (see its doc comment).
fn with_tail(mut scan: artifact::ScanResult, text: &str) -> artifact::ScanResult {
    if artifact::TAIL_ALWAYS {
        scan.hits
            .extend(artifact::tail_hits(text, DIGEST_TAIL_LINES));
    }
    scan
}

/// Builds `get_job_log_errors`'s reply; `failed_modules` is omitted when the
/// `/details` fetch failed or found no failed/softfailed module.
fn digest_reply(
    job_id: i64,
    tier: &str,
    source: &str,
    scan: &artifact::ScanResult,
    failed_modules: Option<Vec<artifact::FailedModule>>,
) -> Result<CallToolResult, ErrorData> {
    let mut reply = json!({
        "job_id": job_id,
        "tier": tier,
        "source": source,
        "total_lines": scan.total_lines,
        "hits": scan.hits,
        "hit_count": scan.hit_count,
        "more_hits": scan.more_hits,
    });
    if let Some(modules) = failed_modules
        && !modules.is_empty()
    {
        reply["failed_modules"] = json!(modules);
    }
    ok(reply)
}
