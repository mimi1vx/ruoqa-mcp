pub mod artifact;
pub mod read;
pub mod write;

use rmcp::ErrorData;

/// `list_jobs`/`list_jobs_overview` `ids`: repeated `ids=` query pairs must
/// fit nginx's default 8 KiB request-line limit.
pub(crate) const MAX_IDS: usize = 500;
/// `restart_jobs.job_ids`: one request, openQA loops over every id.
pub(crate) const MAX_RESTART_JOBS: usize = 500;
/// `trigger_isos.extra`: each entry becomes a scheduled-product/job-settings
/// row; entry *values* stay unbounded (e.g. inline `SCENARIO_DEFINITIONS_YAML`).
pub(crate) const MAX_EXTRA_ENTRIES: usize = 100;

/// Ceiling on both a raw artifact download and its decompressed output.
/// Matches ruoqa's own `max_response_bytes` default; re-imposed here because
/// `Client::send_raw`/`execute` bypass that cap entirely.
pub(crate) const MAX_ARTIFACT_BYTES: usize = 32 * 1024 * 1024;
/// Cap on how many entries `list_job_log_members` reports from one tar
/// archive, so a hostile or huge archive can't produce an unbounded reply.
pub(crate) const MAX_ARCHIVE_MEMBERS: usize = 5000;
/// Bytes read by the initial `Range: bytes=0-511` probe: enough to sniff
/// gzip/xz/tar magic bytes and, on a 206, learn the artifact's total size.
pub(crate) const PROBE_BYTES: u64 = 512;

/// `get_job_log_errors`: matching lines kept per tier before the rest only
/// bump `more_hits` — worst case (3 lines/hit at `DIGEST_CONTEXT_LINES=1`)
/// keeps one reply under ~45 lines of `hits`.
pub(crate) const DIGEST_MAX_HITS: usize = 15;
/// `get_job_log_errors`: context lines kept on each side of a marker hit.
pub(crate) const DIGEST_CONTEXT_LINES: usize = 1;
/// `get_job_log_errors`: lines returned by the `tail` tier when no marker
/// tier matched.
pub(crate) const DIGEST_TAIL_LINES: usize = 30;
/// `get_job_log_errors`: a hit's displayed line is truncated here (matching
/// still runs on the full line, so a marker past this column is never
/// missed).
pub(crate) const DIGEST_MAX_LINE_CHARS: usize = 300;
/// `get_job_log_errors`: cap on how many failed test modules `failed_modules`
/// reports, so a job with a huge number of failures can't produce an
/// unbounded reply.
pub(crate) const DIGEST_MAX_MODULES: usize = 10;

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
