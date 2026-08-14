//! The 15 mutating tools (port of the MUTATING section of `server.py`).
//! `data=` in Python maps to `Client::request_form` (form-encoded, not JSON);
//! `params=` maps to a query string + `Client::request` with no body.

use std::collections::HashMap;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::Method;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::form::Form;
use crate::query::{Query, api};
use crate::server::{OpenQaServer, err, ok, to_result};
use crate::tools::{MAX_BULK_RESTART_JOBS, MAX_EXTRA_ENTRIES, MAX_RESTART_JOBS, bounded};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RestartJobs {
    #[schemars(length(min = 1, max = MAX_RESTART_JOBS))]
    pub job_ids: Vec<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct JobId {
    pub job_id: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct JobComment {
    pub job_id: i64,
    pub text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TriggerIsos {
    pub distri: String,
    pub version: String,
    pub flavor: String,
    pub arch: String,
    #[serde(default)]
    #[schemars(extend("maxProperties" = MAX_EXTRA_ENTRIES))]
    pub extra: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DuplicateJob {
    pub job_id: i64,
    #[serde(default)]
    pub prio: Option<i64>,
    #[serde(default)]
    pub dup_type_auto: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetJobPriority {
    pub job_id: i64,
    pub prio: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RestartJobsBulk {
    #[schemars(length(min = 1, max = MAX_BULK_RESTART_JOBS))]
    pub job_ids: Vec<i64>,
    #[serde(default)]
    pub force: Option<i64>,
    #[serde(default)]
    pub prio: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CancelJobs {
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
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GroupComment {
    pub group_id: i64,
    pub text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ParentGroupComment {
    pub parent_group_id: i64,
    pub text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateJobComment {
    pub job_id: i64,
    pub comment_id: i64,
    pub text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteJobComment {
    pub job_id: i64,
    pub comment_id: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateBug {
    pub bugid: String,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CancelScheduledProduct {
    pub name: String,
}

/// Blank-after-trim values are not filters: treat them the same as absent.
fn nonblank(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

/// Encode set = complement of RFC 3986 unreserved (`A-Za-z0-9` plus `-._~`).
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Percent-encode `value` into exactly one path segment. `.` and `..` are
/// rejected outright rather than encoded: the `url` crate resolves their
/// percent-encoded forms (`%2e`, `%2e%2e`, ...) back to dot segments too, so
/// encoding alone would not close the traversal.
fn path_segment(value: &str) -> Result<String, ErrorData> {
    if value.is_empty() || value == "." || value == ".." {
        return Err(ErrorData::invalid_params(
            "name must not be empty, \".\", or \"..\"",
            None,
        ));
    }
    Ok(utf8_percent_encode(value, SEGMENT).to_string())
}

#[tool_router(router = write_tool_router, vis = "pub(crate)")]
impl OpenQaServer {
    #[tool(
        description = "Restart each of the given jobs.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn restart_jobs(
        &self,
        Parameters(RestartJobs { job_ids }): Parameters<RestartJobs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        bounded(
            "job_ids (use restart_jobs_bulk for larger sets)",
            job_ids.len(),
            1,
            MAX_RESTART_JOBS,
        )?;
        let mut results = Vec::with_capacity(job_ids.len());
        for job_id in job_ids {
            match self
                .request_json(&ctx, Method::POST, &api(&format!("jobs/{job_id}/restart")))
                .await
            {
                Ok(v) => results.push(v),
                Err(e) => return Err(err(e)),
            }
        }
        ok(Value::Array(results))
    }

    #[tool(
        description = "Cancel a running or scheduled job.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn cancel_job(
        &self,
        Parameters(JobId { job_id }): Parameters<JobId>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        to_result(
            self.request_json(&ctx, Method::POST, &api(&format!("jobs/{job_id}/cancel")))
                .await,
        )
    }

    #[tool(
        description = "Add a comment to a job.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn add_job_comment(
        &self,
        Parameters(JobComment { job_id, text }): Parameters<JobComment>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let form = Form::new().push("text", text);
        to_result(
            self.request_form(
                &ctx,
                Method::POST,
                &api(&format!("jobs/{job_id}/comments")),
                &form,
            )
            .await,
        )
    }

    #[tool(
        description = "Trigger ISO test scheduling for a product.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn trigger_isos(
        &self,
        Parameters(TriggerIsos {
            distri,
            version,
            flavor,
            arch,
            extra,
        }): Parameters<TriggerIsos>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        bounded(
            "extra",
            extra.as_ref().map_or(0, HashMap::len),
            0,
            MAX_EXTRA_ENTRIES,
        )?;
        let mut form = Form::new()
            .push("DISTRI", distri)
            .push("VERSION", version)
            .push("FLAVOR", flavor)
            .push("ARCH", arch);
        if let Some(extra) = extra {
            for (k, v) in extra {
                form = form.push(&k, v);
            }
        }
        to_result(
            self.request_form(&ctx, Method::POST, &api("isos"), &form)
                .await,
        )
    }

    #[tool(
        description = "Delete a job.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn delete_job(
        &self,
        Parameters(JobId { job_id }): Parameters<JobId>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        to_result(
            self.request_json(&ctx, Method::DELETE, &api(&format!("jobs/{job_id}")))
                .await,
        )
    }

    #[tool(
        description = "Duplicate (clone) a job.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn duplicate_job(
        &self,
        Parameters(DuplicateJob {
            job_id,
            prio,
            dup_type_auto,
        }): Parameters<DuplicateJob>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let form = Form::new()
            .push_opt("prio", prio)
            .push_opt("dup_type_auto", dup_type_auto);
        to_result(
            self.request_form(
                &ctx,
                Method::POST,
                &api(&format!("jobs/{job_id}/duplicate")),
                &form,
            )
            .await,
        )
    }

    #[tool(
        description = "Set the priority of a job.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn set_job_priority(
        &self,
        Parameters(SetJobPriority { job_id, prio }): Parameters<SetJobPriority>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let form = Form::new().push("prio", prio);
        to_result(
            self.request_form(
                &ctx,
                Method::POST,
                &api(&format!("jobs/{job_id}/prio")),
                &form,
            )
            .await,
        )
    }

    #[tool(
        description = "Restart several jobs in one bulk request.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn restart_jobs_bulk(
        &self,
        Parameters(RestartJobsBulk {
            job_ids,
            force,
            prio,
        }): Parameters<RestartJobsBulk>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        bounded("job_ids", job_ids.len(), 1, MAX_BULK_RESTART_JOBS)?;
        let form = Form::new()
            .push_all("jobs", &job_ids)
            .push_opt("force", force)
            .push_opt("prio", prio);
        to_result(
            self.request_form(&ctx, Method::POST, &api("jobs/restart"), &form)
                .await,
        )
    }

    #[tool(
        description = "Cancel jobs matching the given filters; at least one filter is required.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn cancel_jobs(
        &self,
        Parameters(args): Parameters<CancelJobs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let query = Query::new()
            .push("state", nonblank(args.state))
            .push("result", nonblank(args.result))
            .push("distri", nonblank(args.distri))
            .push("version", nonblank(args.version))
            .push("build", nonblank(args.build))
            .push("test", nonblank(args.test))
            .push("arch", nonblank(args.arch))
            .push("machine", nonblank(args.machine))
            .push("groupid", args.groupid)
            .push("group", nonblank(args.group));
        if query.is_empty() {
            return Err(ErrorData::invalid_params(
                "cancel_jobs requires at least one filter (state, result, distri, version, \
                 build, test, arch, machine, groupid, or group)",
                None,
            ));
        }
        to_result(
            self.request_json(&ctx, Method::POST, &query.finish(&api("jobs/cancel")))
                .await,
        )
    }

    #[tool(
        description = "Add a comment to a job group.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn add_group_comment(
        &self,
        Parameters(GroupComment { group_id, text }): Parameters<GroupComment>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let form = Form::new().push("text", text);
        to_result(
            self.request_form(
                &ctx,
                Method::POST,
                &api(&format!("groups/{group_id}/comments")),
                &form,
            )
            .await,
        )
    }

    #[tool(
        description = "Add a comment to a parent job group.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    async fn add_parent_group_comment(
        &self,
        Parameters(ParentGroupComment {
            parent_group_id,
            text,
        }): Parameters<ParentGroupComment>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let form = Form::new().push("text", text);
        to_result(
            self.request_form(
                &ctx,
                Method::POST,
                &api(&format!("parent_groups/{parent_group_id}/comments")),
                &form,
            )
            .await,
        )
    }

    #[tool(
        description = "Update an existing job comment.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn update_job_comment(
        &self,
        Parameters(UpdateJobComment {
            job_id,
            comment_id,
            text,
        }): Parameters<UpdateJobComment>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let form = Form::new().push("text", text);
        to_result(
            self.request_form(
                &ctx,
                Method::PUT,
                &api(&format!("jobs/{job_id}/comments/{comment_id}")),
                &form,
            )
            .await,
        )
    }

    #[tool(
        description = "Delete a job comment.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn delete_job_comment(
        &self,
        Parameters(DeleteJobComment { job_id, comment_id }): Parameters<DeleteJobComment>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        to_result(
            self.request_json(
                &ctx,
                Method::DELETE,
                &api(&format!("jobs/{job_id}/comments/{comment_id}")),
            )
            .await,
        )
    }

    #[tool(
        description = "Create a tracked bug reference.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn create_bug(
        &self,
        Parameters(CreateBug { bugid, title }): Parameters<CreateBug>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let form = Form::new().push("bugid", bugid).push_opt("title", title);
        to_result(
            self.request_form(&ctx, Method::POST, &api("bugs"), &form)
                .await,
        )
    }

    #[tool(
        description = "Cancel a scheduled product / ISO by name.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn cancel_scheduled_product(
        &self,
        Parameters(CancelScheduledProduct { name }): Parameters<CancelScheduledProduct>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let name = path_segment(&name)?;
        to_result(
            self.request_json(&ctx, Method::POST, &api(&format!("isos/{name}/cancel")))
                .await,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_iso_name_passes_through_unchanged() {
        assert_eq!(
            path_segment("SLE-15-SP4-Online-x86_64-Build1.1-Media1.iso").unwrap(),
            "SLE-15-SP4-Online-x86_64-Build1.1-Media1.iso"
        );
    }

    #[test]
    fn slash_is_encoded() {
        assert_eq!(path_segment("../jobs/7").unwrap(), "..%2Fjobs%2F7");
    }

    #[test]
    fn percent_is_encoded() {
        assert_eq!(path_segment("%2f").unwrap(), "%252f");
    }

    #[test]
    fn query_and_fragment_markers_are_encoded() {
        assert_eq!(path_segment("x?foo=bar").unwrap(), "x%3Ffoo%3Dbar");
        assert_eq!(path_segment("x#frag").unwrap(), "x%23frag");
    }

    #[test]
    fn non_ascii_is_utf8_percent_escaped() {
        assert_eq!(path_segment("ünïcode").unwrap(), "%C3%BCn%C3%AFcode");
    }

    #[test]
    fn empty_and_dot_segments_are_rejected() {
        assert!(path_segment("").is_err());
        assert!(path_segment(".").is_err());
        assert!(path_segment("..").is_err());
    }
}
