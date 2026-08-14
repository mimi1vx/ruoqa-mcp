pub mod read;
pub mod write;

use rmcp::ErrorData;

/// `list_jobs`/`list_jobs_overview` `ids`: repeated `ids=` query pairs must
/// fit nginx's default 8 KiB request-line limit.
pub(crate) const MAX_IDS: usize = 500;
/// `restart_jobs.job_ids`: the only tool that fans one call out to N
/// sequential openQA writes; `restart_jobs_bulk` exists for larger sets.
pub(crate) const MAX_RESTART_JOBS: usize = 50;
/// `restart_jobs_bulk.job_ids`: one request, openQA loops over every id.
pub(crate) const MAX_BULK_RESTART_JOBS: usize = 500;
/// `trigger_isos.extra`: each entry becomes a scheduled-product/job-settings
/// row; entry *values* stay unbounded (e.g. inline `SCENARIO_DEFINITIONS_YAML`).
pub(crate) const MAX_EXTRA_ENTRIES: usize = 100;

/// Reject `len` outside `[min, max]` with a message naming `field`, the
/// observed count, and the limit that was crossed.
pub(crate) fn bounded(field: &str, len: usize, min: usize, max: usize) -> Result<(), ErrorData> {
    if len < min {
        return Err(ErrorData::invalid_params(
            format!("{field} must have at least {min} item(s), got {len}"),
            None,
        ));
    }
    if len > max {
        return Err(ErrorData::invalid_params(
            format!("{field} must have at most {max} item(s), got {len}"),
            None,
        ));
    }
    Ok(())
}
