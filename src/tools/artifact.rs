//! Job-artifact download plumbing: openQA serves job logs and uploaded
//! (`ulogs`) files from `GET /tests/<id>/file/<filename>`, a
//! `Mojolicious::Static` route outside `/api/v1/`. `ruoqa::Client::request`
//! can neither carry a `Range` header nor return raw bytes, so this module
//! goes one layer down to `Client::prepare`/`Client::execute`.
//!
//! Two upstream quirks shape the transport half below:
//! - A suffix range (`Range: bytes=-N`) is mishandled by
//!   `Mojolicious::Static`: it returns the **first** N bytes labelled as a
//!   206, not the last N. Tails always use an absolute range instead
//!   (`bytes=<start>-`).
//! - There is no `If-Range` support, so a tail read on a job whose log is
//!   still growing is detected via `ETag`/`Last-Modified` comparison against
//!   the initial probe, not prevented.

use std::sync::LazyLock;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use regex::{Regex, RegexSet};
use reqwest::header::{CONTENT_RANGE, ETAG, HeaderMap, HeaderValue, LAST_MODIFIED, RANGE};
use reqwest::{Method, StatusCode};
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer};
use ruoqa::PreparedRequest;
use serde::Serialize;
use serde_json::Value;

use crate::error::{classify, status_kind, tool_error};
use crate::heartbeat::with_heartbeat;
use crate::tools::{DIGEST_MAX_LINE_CHARS, DIGEST_MAX_MODULES, PROBE_BYTES};

/// Bytes assumed per log line when sizing a tail read. Generous relative to
/// a typical openQA log line, so a `tail_lines` request rarely needs the
/// one-shot retry that a short read would otherwise force, while still
/// keeping the transfer at kilobytes rather than megabytes.
const TAIL_BYTES_PER_LINE: u64 = 200;
/// Floor on the tail window, so a small `tail_lines` value still reads
/// enough context to be useful.
const MIN_TAIL_WINDOW_BYTES: u64 = 8 * 1024;
/// How much of a non-2xx response body to keep for an error message.
const ERROR_BODY_PREVIEW_BYTES: usize = 8 * 1024;
/// Default `get_job_log` `grep` match cap, when the caller doesn't set one.
pub(crate) const DEFAULT_MAX_MATCHES: usize = 100;

/// A caller-visible failure, already shaped as this tool call's return
/// value: either a protocol-level [`ErrorData`] (bad input, a `ruoqa`
/// transport failure) or a tool-level `{"error": {...}}` result.
pub(crate) type Bail = Result<CallToolResult, ErrorData>;

fn params_bail(message: impl Into<String>) -> Bail {
    Err(ErrorData::invalid_params(message.into(), None))
}

/// Encode set = complement of RFC 3986 unreserved (`A-Za-z0-9` plus `-._~`).
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Rejects anything that isn't a bare filename: openQA's route placeholder
/// can't match a `/` anyway, but percent-encoding one instead of rejecting
/// it would only turn a clear error into an opaque 404 (a decoded `%2F`
/// stops the route matching, not the traversal).
pub(crate) fn validate_filename(name: &str) -> Result<(), ErrorData> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(ErrorData::invalid_params(
            format!("filename {name:?} must be a bare name: no \"/\", \"\\\", or \"..\""),
            None,
        ));
    }
    Ok(())
}

/// A tar member is an archive-internal path (legitimately containing `/`
/// for a nested entry), not a URL segment — only reject the nonsensical
/// cases, not `/` itself.
pub(crate) fn validate_member(name: &str) -> Result<(), ErrorData> {
    if name.is_empty() || name.split('/').any(|part| part == "..") {
        return Err(ErrorData::invalid_params(
            format!("member {name:?} must not be empty or contain \"..\""),
            None,
        ));
    }
    Ok(())
}

fn artifact_path(job_id: i64, filename: &str) -> String {
    format!(
        "/tests/{job_id}/file/{}",
        utf8_percent_encode(filename, SEGMENT)
    )
}

/// Result of the initial `Range: bytes=0-<PROBE_BYTES-1>` request.
pub(crate) struct Probe {
    /// The artifact's total size, when the server answered with a 206 and a
    /// parseable `Content-Range`.
    pub(crate) size: Option<u64>,
    pub(crate) etag: Option<String>,
    pub(crate) last_modified: Option<String>,
    /// The bytes read back: the probe window on a 206, or the whole
    /// artifact on a 200 (see `complete`).
    pub(crate) body: Vec<u8>,
    /// True when `body` already is the entire artifact — a 200 response
    /// (Range ignored, or the file is no larger than the probe window) — so
    /// the caller can skip a second request.
    pub(crate) complete: bool,
    /// The resolved artifact URL, for error messages.
    pub(crate) url: reqwest::Url,
}

/// GET `bytes=0-<n-1>` of the artifact under a heartbeat, classifying the
/// response into a [`Probe`]. `ceiling` only bounds the fallback full-body
/// read on a 200; a 206 is inherently no larger than the requested window.
pub(crate) async fn probe(
    client: &ruoqa::Client,
    ctx: &RequestContext<RoleServer>,
    job_id: i64,
    filename: &str,
    probe_bytes: u64,
    ceiling: usize,
) -> Result<Probe, Bail> {
    let path = artifact_path(job_id, filename);
    let mut prepared = client.prepare(Method::GET, &path, None).map_err(classify)?;
    insert_range(
        &mut prepared,
        &format!("bytes=0-{}", probe_bytes.saturating_sub(1)),
    );

    let mut resp = execute(client, ctx, &prepared).await?;
    check_not_bounced(&prepared, &resp)?;
    if !resp.status().is_success() {
        return Err(status_failure(&prepared, &mut resp).await);
    }

    let etag = header_string(resp.headers(), &ETAG);
    let last_modified = header_string(resp.headers(), &LAST_MODIFIED);

    if resp.status() == StatusCode::PARTIAL_CONTENT {
        let size = resp
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_content_range_total);
        let probe_bytes_usize = usize::try_from(probe_bytes).unwrap_or(usize::MAX);
        let body = read_capped(&mut resp, probe_bytes_usize.saturating_add(1024)).await?;
        Ok(Probe {
            size,
            etag,
            last_modified,
            body,
            complete: false,
            url: prepared.url,
        })
    } else {
        let body = read_capped(&mut resp, ceiling).await?;
        let size = Some(body.len() as u64);
        Ok(Probe {
            size,
            etag,
            last_modified,
            body,
            complete: true,
            url: prepared.url,
        })
    }
}

/// Fetches (if needed) and decompresses the whole artifact, reusing
/// `probe`'s body when it already is the complete artifact. Used by both
/// `list_job_log_members` and `get_job_log`'s archive/member path.
pub(crate) async fn fetch_decoded(
    client: &ruoqa::Client,
    ctx: &RequestContext<RoleServer>,
    job_id: i64,
    filename: &str,
    probe: &Probe,
    ceiling: usize,
) -> Result<Vec<u8>, Bail> {
    let format = sniff(&probe.body);
    let raw = if probe.complete {
        probe.body.clone()
    } else {
        fetch_all(client, ctx, job_id, filename, ceiling).await?
    };
    decompress(&raw, format, ceiling)
}

pub(crate) fn unsupported_media(url: &reqwest::Url, size: u64) -> Bail {
    tool_error(
        "unsupported_media",
        None,
        format!("{url} is {size} bytes of non-text content; get_job_log only supports text"),
        None,
    )
}

/// Result of a tail-range fetch.
pub(crate) struct TailFetch {
    pub(crate) bytes: Vec<u8>,
    /// True if `ETag`/`Last-Modified` moved between the probe and this read
    /// (even after one retry) — a job's log grew while we were reading it.
    pub(crate) changed_during_read: bool,
}

/// Byte window to request for `tail_lines` lines, given the artifact's
/// (possibly unknown) total size.
pub(crate) fn tail_window(tail_lines: usize, size: Option<u64>, ceiling: u64) -> u64 {
    let wanted = (tail_lines as u64 * TAIL_BYTES_PER_LINE).max(MIN_TAIL_WINDOW_BYTES);
    let bounded = wanted.min(ceiling);
    size.map_or(bounded, |size| bounded.min(size))
}

/// GET the last `window` bytes via an **absolute** range (`bytes=<start>-`,
/// never `bytes=-N`: openQA's `Mojolicious::Static` returns the head,
/// mislabelled, for a suffix range). Retries once on an `ETag`/
/// `Last-Modified` mismatch against `probe`, then reports the mismatch
/// instead of failing.
pub(crate) async fn fetch_tail(
    client: &ruoqa::Client,
    ctx: &RequestContext<RoleServer>,
    job_id: i64,
    filename: &str,
    probe: &Probe,
    window: u64,
    ceiling: usize,
) -> Result<TailFetch, Bail> {
    let size = probe.size.unwrap_or(window);
    let start = size.saturating_sub(window);
    let path = artifact_path(job_id, filename);

    for attempt in 0..2 {
        let mut prepared = client.prepare(Method::GET, &path, None).map_err(classify)?;
        insert_range(&mut prepared, &format!("bytes={start}-"));

        let mut resp = execute(client, ctx, &prepared).await?;
        check_not_bounced(&prepared, &resp)?;
        if !resp.status().is_success() {
            return Err(status_failure(&prepared, &mut resp).await);
        }

        let etag = header_string(resp.headers(), &ETAG);
        let last_modified = header_string(resp.headers(), &LAST_MODIFIED);
        let changed = (probe.etag.is_some() && probe.etag != etag)
            || (probe.last_modified.is_some() && probe.last_modified != last_modified);
        if changed && attempt == 0 {
            continue;
        }

        let bytes = read_capped(&mut resp, ceiling).await?;
        return Ok(TailFetch {
            bytes,
            changed_during_read: changed,
        });
    }
    unreachable!("the loop above always returns on attempt 0 or 1")
}

/// GET the whole artifact, aborting with `response_too_large` at `ceiling`
/// rather than buffering an unbounded body.
pub(crate) async fn fetch_all(
    client: &ruoqa::Client,
    ctx: &RequestContext<RoleServer>,
    job_id: i64,
    filename: &str,
    ceiling: usize,
) -> Result<Vec<u8>, Bail> {
    let path = artifact_path(job_id, filename);
    let prepared = client.prepare(Method::GET, &path, None).map_err(classify)?;
    let mut resp = execute(client, ctx, &prepared).await?;
    check_not_bounced(&prepared, &resp)?;
    if !resp.status().is_success() {
        return Err(status_failure(&prepared, &mut resp).await);
    }
    read_capped(&mut resp, ceiling).await
}

fn insert_range(prepared: &mut PreparedRequest, value: &str) {
    // `value` is always `bytes=<digits>-` or `bytes=<digits>-<digits>`,
    // built from `u64`s above: always valid header-value ASCII.
    prepared.headers.insert(
        RANGE,
        HeaderValue::from_str(value).expect("range value is ASCII"),
    );
}

async fn execute(
    client: &ruoqa::Client,
    ctx: &RequestContext<RoleServer>,
    prepared: &PreparedRequest,
) -> Result<reqwest::Response, Bail> {
    let token = ctx.meta.get_progress_token();
    with_heartbeat(&ctx.peer, token, client.execute(prepared, false))
        .await
        .map_err(classify)
}

/// openQA never requires auth for `/tests/*/file/*`; a response whose final
/// URL path differs from what was requested means a redirect to a login
/// page, not the artifact.
fn check_not_bounced(prepared: &PreparedRequest, resp: &reqwest::Response) -> Result<(), Bail> {
    if resp.url().path() != prepared.url.path() {
        return Err(tool_error(
            "unauthorized",
            None,
            format!(
                "openQA redirected {} to {} (likely a login page)",
                prepared.url.path(),
                resp.url().path()
            ),
            None,
        ));
    }
    Ok(())
}

async fn status_failure(prepared: &PreparedRequest, resp: &mut reqwest::Response) -> Bail {
    let status = resp.status();
    let body = read_truncated(resp, ERROR_BODY_PREVIEW_BYTES).await;
    let message = format!("{} {} returned {status}", prepared.method, prepared.url);
    tool_error(
        status_kind(status.as_u16()),
        Some(status.as_u16()),
        message,
        Some(&body),
    )
}

fn header_string(headers: &HeaderMap, name: &reqwest::header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// Streams `resp`'s body, aborting with a `response_too_large` [`Bail`] as
/// soon as `limit` would be exceeded.
async fn read_capped(resp: &mut reqwest::Response, limit: usize) -> Result<Vec<u8>, Bail> {
    let mut buf = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if chunk.len() > limit.saturating_sub(buf.len()) {
                    return Err(tool_error(
                        "response_too_large",
                        None,
                        format!("artifact exceeded the {limit}-byte limit"),
                        None,
                    ));
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(buf),
            Err(source) => {
                return Err(tool_error("connection", None, source.to_string(), None));
            }
        }
    }
}

/// Best-effort read of up to `limit` bytes for an error-message preview.
/// Never fails: a transport error while reading an already-failed
/// response's body just means a shorter (or empty) preview.
async fn read_truncated(resp: &mut reqwest::Response, limit: usize) -> String {
    let mut buf = Vec::new();
    while buf.len() < limit {
        match resp.chunk().await {
            Ok(Some(chunk)) => buf.extend_from_slice(&chunk),
            _ => break,
        }
    }
    buf.truncate(limit);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Parses a `Content-Range: bytes <start>-<end>/<size>` header value into
/// `size`. Returns `None` for an unparseable value or `*` (unknown size).
fn parse_content_range_total(value: &str) -> Option<u64> {
    value.rsplit('/').next()?.trim().parse().ok()
}

/// What an artifact's leading bytes turned out to be, sniffed by magic
/// bytes rather than the (openQA-supplied, often generic) content type or
/// file extension — `.txz` isn't in Mojo's MIME table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sniff {
    Gzip,
    Xz,
    Tar,
    Plain,
}

/// Sniffs `bytes` (expected to be the first ~512 bytes of an artifact, long
/// enough to reach the `ustar` magic at offset 257).
pub(crate) fn sniff(bytes: &[u8]) -> Sniff {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        Sniff::Gzip
    } else if bytes.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0x00]) {
        Sniff::Xz
    } else if bytes.len() >= 262 && &bytes[257..262] == b"ustar" {
        Sniff::Tar
    } else {
        Sniff::Plain
    }
}

/// A `Write` sink that errors as soon as writing more would exceed `limit`,
/// so a decompression bomb aborts as `response_too_large` instead of
/// growing `buf` without bound. Used for both gzip (via `io::copy` from a
/// `Read` decoder) and xz (via `lzma_rs`'s push-style API).
struct CappedWriter {
    buf: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl std::io::Write for CappedWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.buf.len() + data.len() > self.limit {
            self.exceeded = true;
            return Err(std::io::Error::other(
                "decompressed artifact exceeded the byte ceiling",
            ));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Decompresses `bytes` per `sniff`, bounding the output at `ceiling`.
/// `Plain`/`Tar` pass through unchanged: a plain tar is not itself
/// compressed, and its members are extracted separately.
pub(crate) fn decompress(bytes: &[u8], format: Sniff, ceiling: usize) -> Result<Vec<u8>, Bail> {
    match format {
        Sniff::Plain | Sniff::Tar => Ok(bytes.to_vec()),
        Sniff::Gzip => {
            let mut writer = CappedWriter {
                buf: Vec::new(),
                limit: ceiling,
                exceeded: false,
            };
            let mut decoder = flate2::read::GzDecoder::new(bytes);
            match std::io::copy(&mut decoder, &mut writer) {
                Ok(_) => Ok(writer.buf),
                Err(_) if writer.exceeded => Err(too_large(ceiling)),
                Err(e) => Err(tool_error(
                    "invalid_response",
                    None,
                    format!("gzip decode failed: {e}"),
                    None,
                )),
            }
        }
        Sniff::Xz => {
            let mut writer = CappedWriter {
                buf: Vec::new(),
                limit: ceiling,
                exceeded: false,
            };
            let mut cursor: &[u8] = bytes;
            match lzma_rs::xz_decompress(&mut cursor, &mut writer) {
                Ok(()) => Ok(writer.buf),
                Err(_) if writer.exceeded => Err(too_large(ceiling)),
                Err(e) => Err(tool_error(
                    "invalid_response",
                    None,
                    format!("xz decode failed: {e}"),
                    None,
                )),
            }
        }
    }
}

fn too_large(ceiling: usize) -> Bail {
    tool_error(
        "response_too_large",
        None,
        format!("decompressed artifact exceeded the {ceiling}-byte limit"),
        None,
    )
}

#[derive(Debug, Serialize)]
pub(crate) struct TarMember {
    pub(crate) path: String,
    pub(crate) size: u64,
}

/// Lists up to `limit` entries of the tar archive in `bytes`.
pub(crate) fn tar_members(bytes: &[u8], limit: usize) -> Result<(Vec<TarMember>, bool), Bail> {
    let mut archive = tar::Archive::new(bytes);
    let entries = archive.entries().map_err(|e| {
        tool_error(
            "invalid_response",
            None,
            format!("not a valid tar archive: {e}"),
            None,
        )
    })?;

    let mut members = Vec::new();
    let mut truncated = false;
    for (i, entry) in entries.enumerate() {
        if i >= limit {
            truncated = true;
            break;
        }
        let entry = entry.map_err(|e| {
            tool_error(
                "invalid_response",
                None,
                format!("corrupt tar entry: {e}"),
                None,
            )
        })?;
        let path = entry
            .path()
            .map_err(|e| {
                tool_error(
                    "invalid_response",
                    None,
                    format!("corrupt tar entry path: {e}"),
                    None,
                )
            })?
            .to_string_lossy()
            .into_owned();
        let size = entry.header().size().unwrap_or(0);
        members.push(TarMember { path, size });
    }
    Ok((members, truncated))
}

/// Extracts one member's bytes from the tar archive in `bytes`, bounded at
/// `ceiling`.
pub(crate) fn extract_tar_member(
    bytes: &[u8],
    member: &str,
    ceiling: usize,
) -> Result<Vec<u8>, Bail> {
    let mut archive = tar::Archive::new(bytes);
    let entries = archive.entries().map_err(|e| {
        tool_error(
            "invalid_response",
            None,
            format!("not a valid tar archive: {e}"),
            None,
        )
    })?;

    for entry in entries {
        let mut entry = entry.map_err(|e| {
            tool_error(
                "invalid_response",
                None,
                format!("corrupt tar entry: {e}"),
                None,
            )
        })?;
        let path = entry
            .path()
            .map_err(|e| {
                tool_error(
                    "invalid_response",
                    None,
                    format!("corrupt tar entry path: {e}"),
                    None,
                )
            })?
            .to_string_lossy()
            .into_owned();
        if path != member {
            continue;
        }
        let mut writer = CappedWriter {
            buf: Vec::new(),
            limit: ceiling,
            exceeded: false,
        };
        return match std::io::copy(&mut entry, &mut writer) {
            Ok(_) => Ok(writer.buf),
            Err(_) if writer.exceeded => Err(too_large(ceiling)),
            Err(e) => Err(tool_error(
                "invalid_response",
                None,
                format!("reading tar member {member:?}: {e}"),
                None,
            )),
        };
    }
    Err(params_bail(format!(
        "no member named {member:?} in this archive; call list_job_log_members to see available entries"
    )))
}

/// Trims `text` to its last `n` lines.
fn last_n_lines(text: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// Drops everything up to and including the first `\n`, for text read from
/// a byte-ranged tail (which may start mid-line). A window with no newline
/// at all is a single partial line with nothing else to show.
pub(crate) fn drop_partial_first_line(text: &str) -> &str {
    text.find('\n').map_or("", |i| &text[i + 1..])
}

#[derive(Debug, Serialize)]
pub(crate) struct MatchHit {
    pub(crate) line: usize,
    pub(crate) text: String,
}

pub(crate) struct GrepMatches {
    pub(crate) hits: Vec<MatchHit>,
    pub(crate) total: usize,
    pub(crate) truncated: bool,
}

pub(crate) enum Sliced {
    Text(String),
    Matches(GrepMatches),
}

/// `tail_lines` → keep only the last N lines (idempotent to apply again on
/// text a caller already tail-ranged: trimming an already-short text to the
/// same or fewer lines is a no-op). No `grep` → that text as-is. `grep` →
/// every matching line plus `context_lines` on each side, capped at
/// `max_matches` matching lines (context lines don't count against it);
/// `total_matches` still reports the true count.
pub(crate) fn slice(
    text: &str,
    tail_lines: Option<usize>,
    grep: Option<&str>,
    context_lines: usize,
    max_matches: usize,
) -> Result<Sliced, ErrorData> {
    let trimmed = match tail_lines {
        Some(n) => last_n_lines(text, n),
        None => text.to_string(),
    };
    let Some(pattern) = grep else {
        return Ok(Sliced::Text(trimmed));
    };
    let re = Regex::new(pattern).map_err(|e| {
        ErrorData::invalid_params(format!("invalid grep pattern {pattern:?}: {e}"), None)
    })?;
    Ok(Sliced::Matches(grep_lines(
        &trimmed,
        &re,
        context_lines,
        max_matches,
    )))
}

fn grep_lines(text: &str, re: &Regex, context_lines: usize, max_matches: usize) -> GrepMatches {
    let lines: Vec<&str> = text.lines().collect();
    let mut hits = Vec::new();
    let mut total = 0usize;
    let mut emitted = 0usize;
    for (idx, line) in lines.iter().enumerate() {
        if !re.is_match(line) {
            continue;
        }
        total += 1;
        if emitted >= max_matches {
            continue;
        }
        emitted += 1;
        let from = idx.saturating_sub(context_lines);
        let to = (idx + context_lines).min(lines.len().saturating_sub(1));
        for (i, context_line) in lines.iter().enumerate().take(to + 1).skip(from) {
            hits.push(MatchHit {
                line: i + 1,
                text: (*context_line).to_string(),
            });
        }
    }
    GrepMatches {
        hits,
        total,
        truncated: total > max_matches,
    }
}

// --- `get_job_log_errors` digest -------------------------------------------
//
// Three tiers, checked in priority order by the caller (tools/read.rs): a
// real openQA log is full of incidental "error"/"timeout" strings from
// subprocess output (grub, curl, ssh) that have nothing to do with the test
// verdict.
//   1. TAP_MARKERS in serial_terminal.txt — frameworks driven over the
//      serial console (LTP and similar) report their actual assertion
//      result only here (TINFO/TFAIL/TBROK, LTP's own TAP-like convention),
//      never in autoinst-log.txt.
//   2. FATAL_MARKERS in autoinst-log.txt — the literal "# Test died:"
//      os-autoinst itself writes when a module dies, plus the worker's own
//      terminal verdict line (`Result: <reason>`, `OpenQA::Constants`'
//      `WORKER_SR_*` enum) for the abnormal-termination reasons that never
//      go through a module death at all — e.g. an asset failing to
//      download before any module runs logs only `Result: setup failure`.
//      `done`/`finish-off` are deliberately excluded: `done` means the
//      worker exited normally (a job can still fail at the module level
//      with `Result: done` in its log), and `finish-off` is a graceful
//      worker shutdown, not a failure.
//   3. NOISE_MARKERS in autoinst-log.txt — fallback only.
// Ported from reviewpc's `lib/oqa-fetch.mjs` (TAP_MARKERS/FATAL_MARKERS/
// NOISE_MARKERS), with each pattern's `/i` flag inlined as `(?i)` for
// `RegexSet`, which compiles every member pattern independently.
pub(crate) static TAP_MARKERS: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSet::new([r"\bTFAIL\b", r"\bTBROK\b", r"^not ok"]).expect("static regex set")
});
pub(crate) static FATAL_MARKERS: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSet::new([
        "Test died",
        r"(?i)\bdied\b",
        "Result: setup failure",
        "Result: api-failure",
        "Result: worker broken",
        "Result: timeout",
    ])
    .expect("static regex set")
});
pub(crate) static NOISE_MARKERS: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSet::new([
        // Excludes the ubiquitous `timeout=<n>` keyword argument that
        // `testapi::wait_serial`/`assert_screen`/`type_string` log on nearly
        // every debug line (20000+ false matches burying the one real line
        // on job 23938357) and the `--timeout <n>` CLI-flag form every
        // job's startup `rsync -avHP --timeout 1800 ...` line also matches
        // (confirmed present verbatim in every autoinst-log.txt checked).
        // The `regex` crate has no lookaround, so both exclusions are
        // consuming alternatives instead: end of line, a non-'='/whitespace
        // character (", ." etc.), or whitespace not followed by a digit —
        // "timeout=undef", "timeout 1800", and "timeout=200)" all fail every
        // branch, while "Result: timeout", "timed out, retrying", and
        // "timeout waiting for serial" each satisfy one.
        r"(?i)\btimed? ?out(?:$|[^=\s]|\s\D)",
        "Failed to ",
        r"(?i)\berror:",
        r"\bERROR\b",
        r"(?i)command .* failed",
    ])
    .expect("static regex set")
});

/// When true, every tier's reply also carries the tail (the reference
/// implementation's behaviour); when false (the default — a tier match
/// means the tail's file may never even have been fetched), the `tail`
/// tier only fires on its own, once nothing else matched.
pub(crate) const TAIL_ALWAYS: bool = false;

/// Compiles each `markers` pattern individually first (so a bad one is
/// named in the error, mirroring `slice`'s grep-pattern message), then as
/// one combined [`RegexSet`].
pub(crate) fn compile_markers(patterns: &[String]) -> Result<RegexSet, ErrorData> {
    for pattern in patterns {
        Regex::new(pattern).map_err(|e| {
            ErrorData::invalid_params(format!("invalid marker pattern {pattern:?}: {e}"), None)
        })?;
    }
    RegexSet::new(patterns)
        .map_err(|e| ErrorData::invalid_params(format!("invalid markers: {e}"), None))
}

/// Truncates `line` to `max_chars` **characters** (not bytes) with a
/// trailing `…`; matching against `set` always runs on the untruncated line
/// first, so a marker past this column is never missed, only unshown.
fn truncate_line(line: &str, max_chars: usize) -> String {
    if line.chars().count() <= max_chars {
        return line.to_string();
    }
    let mut truncated: String = line.chars().take(max_chars).collect();
    truncated.push('…');
    truncated
}

pub(crate) struct ScanResult {
    pub(crate) hits: Vec<MatchHit>,
    /// True count of matching lines, uncapped (unlike `hits`, which stops
    /// growing past `max_hits`).
    pub(crate) hit_count: usize,
    pub(crate) more_hits: bool,
    pub(crate) total_lines: usize,
}

/// One pass over `text`, emitting every line matching `set` plus `context`
/// lines either side, capped at `max_hits` *matching* lines (context lines
/// don't count against it). Never emits the same line twice: a match whose
/// context range overlaps the previous one only contributes its new tail
/// (ports `oqa-fetch.mjs`'s `collectContextHits` `lastPrinted` guard).
pub(crate) fn scan_markers(
    text: &str,
    set: &RegexSet,
    context: usize,
    max_hits: usize,
) -> ScanResult {
    let lines: Vec<&str> = text.lines().collect();
    let mut hits = Vec::new();
    let mut hit_count = 0usize;
    let mut last_emitted: Option<usize> = None;
    for (idx, line) in lines.iter().enumerate() {
        if !set.is_match(line) {
            continue;
        }
        hit_count += 1;
        if hit_count > max_hits {
            continue;
        }
        let from = idx.saturating_sub(context);
        let to = (idx + context).min(lines.len().saturating_sub(1));
        let start = last_emitted.map_or(from, |last| from.max(last + 1));
        if start <= to {
            for (i, &l) in lines.iter().enumerate().take(to + 1).skip(start) {
                hits.push(MatchHit {
                    line: i + 1,
                    text: truncate_line(l, DIGEST_MAX_LINE_CHARS),
                });
            }
            last_emitted = Some(to);
        }
    }
    ScanResult {
        hits,
        hit_count,
        more_hits: hit_count > max_hits,
        total_lines: lines.len(),
    }
}

/// The last `n` lines of `text`, correctly numbered from the start of the
/// file (not from 1).
pub(crate) fn tail_hits(text: &str, n: usize) -> Vec<MatchHit> {
    let lines: Vec<&str> = text.lines().collect();
    if n == 0 {
        return Vec::new();
    }
    let start = lines.len().saturating_sub(n);
    lines[start..]
        .iter()
        .enumerate()
        .map(|(i, line)| MatchHit {
            line: start + i + 1,
            text: truncate_line(line, DIGEST_MAX_LINE_CHARS),
        })
        .collect()
}

/// GET, decode (gzip/xz transparently; a tar is refused, same message
/// `get_job_log` gives), and lossily decode as UTF-8 — `serial_terminal.txt`
/// is raw console output that routinely carries stray non-UTF-8 bytes,
/// unlike `get_job_log`'s strict `unsupported_media` refusal.
pub(crate) async fn fetch_text_lossy(
    client: &ruoqa::Client,
    ctx: &RequestContext<RoleServer>,
    job_id: i64,
    filename: &str,
    ceiling: usize,
) -> Result<String, Bail> {
    let probed = probe(client, ctx, job_id, filename, PROBE_BYTES, ceiling).await?;
    let content = fetch_decoded(client, ctx, job_id, filename, &probed, ceiling).await?;
    if sniff(&content) == Sniff::Tar {
        return Err(params_bail(format!(
            "{filename:?} is an archive; pass `member` (see list_job_log_members) to read one entry"
        )));
    }
    Ok(String::from_utf8_lossy(&content).into_owned())
}

#[derive(Debug, Serialize)]
pub(crate) struct FailedStep {
    pub(crate) num: u64,
    pub(crate) title: String,
    pub(crate) result: String,
    pub(crate) url: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct FailedModule {
    pub(crate) module: String,
    pub(crate) result: String,
    pub(crate) steps: Vec<FailedStep>,
}

/// `GET /api/v1/jobs/<id>/details` renders `{"job": {..., "testresults": \
/// [...]}}` (`OpenQA::WebAPI::Controller::API::V1::Job::show`, `render(json \
/// => {job => $job})`). Keeps failed/softfailed modules (capped at
/// `DIGEST_MAX_MODULES`) and, per module, the step results that are
/// themselves `fail`/`softfail` — never `text_data`, which is
/// `get_job_details`'s job. Deliberately **not** every non-`"ok"` step:
/// live-tested against job 23779447's `patch_and_reboot` (274 steps, only
/// one `fail` and five `softfail`), 139 of its steps are `unk` — a
/// needle-less screenshot os-autoinst never scored, not a failure signal —
/// and including them would have swamped the digest with noise.
pub(crate) fn failed_modules(
    details: &Value,
    job_id: i64,
    base_url: &reqwest::Url,
) -> Vec<FailedModule> {
    let job = details.get("job").unwrap_or(details);
    let Some(results) = job.get("testresults").and_then(Value::as_array) else {
        return Vec::new();
    };

    let mut modules = Vec::new();
    for module in results {
        let result = module.get("result").and_then(Value::as_str).unwrap_or("");
        if result != "failed" && result != "softfailed" {
            continue;
        }
        if modules.len() >= DIGEST_MAX_MODULES {
            break;
        }
        let name = module.get("name").and_then(Value::as_str).unwrap_or("");
        let steps = module
            .get("details")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|d| {
                let step_result = d.get("result").and_then(Value::as_str)?;
                if step_result != "fail" && step_result != "softfail" {
                    return None;
                }
                let num = d.get("num").and_then(Value::as_u64).unwrap_or(0);
                let title = d.get("title").and_then(Value::as_str).unwrap_or("");
                Some(FailedStep {
                    num,
                    title: title.to_string(),
                    result: step_result.to_string(),
                    url: format!("{base_url}tests/{job_id}#step/{name}/{num}"),
                })
            })
            .collect();
        modules.push(FailedModule {
            module: name.to_string(),
            result: result.to_string(),
            steps,
        });
    }
    modules
}

/// Whether `name` is listed in `/details`'s `logs`/`ulogs` arrays — used to
/// skip probing `serial_terminal.txt` on jobs that never wrote one, instead
/// of costing a 404.
pub(crate) fn has_log(details: &Value, name: &str) -> bool {
    let job = details.get("job").unwrap_or(details);
    ["logs", "ulogs"].iter().any(|key| {
        job.get(key)
            .and_then(Value::as_array)
            .is_some_and(|arr| arr.iter().any(|v| v.as_str() == Some(name)))
    })
}

#[derive(Debug, Serialize)]
pub(crate) struct LogFile {
    pub(crate) name: String,
    pub(crate) kind: &'static str,
}

/// Scrapes the file list out of `downloads_ajax`'s HTML fragment: every
/// `href="/tests/<job_id>/file/<name>"`, deduped, classified as `"ulog"` if
/// it appears after the "Uploaded logs" heading, `"result"` otherwise. A job
/// with no uploaded logs renders no such heading at all (`downloads.html.ep`
/// skips the whole section when `@$ulogs` is empty), so its absence means
/// "no ulogs", not a template change — everything found is `"result"`.
pub(crate) fn parse_downloads(html: &str, job_id: i64) -> Vec<LogFile> {
    static LINK: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"href="/tests/(\d+)/file/([^"]+)""#).expect("static regex"));

    let ulogs_heading = html.find("Uploaded logs");
    let mut seen = std::collections::BTreeSet::new();
    let mut files = Vec::new();
    for cap in LINK.captures_iter(html) {
        if cap[1].parse::<i64>() != Ok(job_id) {
            continue;
        }
        let name = percent_encoding::percent_decode_str(&cap[2])
            .decode_utf8_lossy()
            .into_owned();
        if !seen.insert(name.clone()) {
            continue;
        }
        let is_ulog =
            ulogs_heading.is_some_and(|heading| cap.get(0).is_some_and(|m| m.start() > heading));
        let kind = if is_ulog { "ulog" } else { "result" };
        files.push(LogFile { name, kind });
    }
    files
}

/// `logs`/`ulogs` from `GET /api/v1/jobs/<id>/details`, the fallback source
/// when `downloads_ajax` is unavailable or its parse comes back empty.
pub(crate) fn details_logs(value: &serde_json::Value) -> Vec<LogFile> {
    let mut files = Vec::new();
    for (key, kind) in [("logs", "result"), ("ulogs", "ulog")] {
        if let Some(arr) = value.get(key).and_then(serde_json::Value::as_array) {
            for v in arr {
                if let Some(name) = v.as_str() {
                    files.push(LogFile {
                        name: name.to_string(),
                        kind,
                    });
                }
            }
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;
    use crate::tools::DIGEST_MAX_HITS;

    #[test]
    fn validate_filename_rejects_traversal_and_separators() {
        for bad in ["", "a/b", "a\\b", "..", "../etc/passwd", "a/../b"] {
            assert!(
                validate_filename(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn validate_filename_accepts_bare_names() {
        for good in [
            "autoinst-log.txt",
            "destroy-scc_supportconfig_partial.txz",
            "y2logs.tar.xz",
        ] {
            assert!(
                validate_filename(good).is_ok(),
                "{good:?} should be accepted"
            );
        }
    }

    #[test]
    fn artifact_path_percent_encodes_special_characters() {
        assert_eq!(artifact_path(42, "a b.txt"), "/tests/42/file/a%20b.txt");
    }

    #[test]
    fn parse_content_range_total_reads_the_size() {
        assert_eq!(
            parse_content_range_total("bytes 0-511/123456"),
            Some(123_456)
        );
    }

    #[test]
    fn parse_content_range_total_none_for_unknown_size() {
        assert_eq!(parse_content_range_total("bytes 0-511/*"), None);
    }

    #[test]
    fn parse_content_range_total_none_for_garbage() {
        assert_eq!(parse_content_range_total("not a content range"), None);
    }

    #[test]
    fn sniff_detects_gzip_xz_tar_and_plain() {
        assert_eq!(sniff(&[0x1f, 0x8b, 0x08]), Sniff::Gzip);
        assert_eq!(
            sniff(&[0xFD, b'7', b'z', b'X', b'Z', 0x00, 0x00]),
            Sniff::Xz
        );
        let mut tar_header = vec![0u8; 512];
        tar_header[257..262].copy_from_slice(b"ustar");
        assert_eq!(sniff(&tar_header), Sniff::Tar);
        assert_eq!(sniff(b"plain text log line\n"), Sniff::Plain);
    }

    #[test]
    fn last_n_lines_keeps_only_the_last_n() {
        let text = "a\nb\nc\nd\ne";
        assert_eq!(last_n_lines(text, 2), "d\ne");
        assert_eq!(last_n_lines(text, 100), "a\nb\nc\nd\ne");
        assert_eq!(last_n_lines(text, 0), "");
    }

    #[test]
    fn drop_partial_first_line_drops_up_to_first_newline() {
        assert_eq!(drop_partial_first_line("rtial\nb\nc"), "b\nc");
        assert_eq!(drop_partial_first_line("no newline here"), "");
    }

    #[test]
    fn slice_without_grep_returns_whole_text() {
        let Sliced::Text(text) = slice("a\nb\nc", None, None, 0, 10).unwrap() else {
            panic!("expected Text");
        };
        assert_eq!(text, "a\nb\nc");
    }

    #[test]
    fn slice_applies_tail_lines_before_grep() {
        let text = "a\nb\nc\nd\ne";
        let Sliced::Text(text) = slice(text, Some(2), None, 0, 10).unwrap() else {
            panic!("expected Text");
        };
        assert_eq!(text, "d\ne");
    }

    #[test]
    fn slice_with_grep_returns_matches_with_context() {
        let text = "one\ntwo\nthree\nfour\nfive";
        let Sliced::Matches(m) = slice(text, None, Some("three"), 1, 10).unwrap() else {
            panic!("expected Matches");
        };
        assert_eq!(m.total, 1);
        assert!(!m.truncated);
        let lines: Vec<usize> = m.hits.iter().map(|h| h.line).collect();
        assert_eq!(lines, vec![2, 3, 4]);
    }

    #[test]
    fn slice_caps_matches_but_keeps_the_true_total() {
        let text = "x\nx\nx\nx";
        let Sliced::Matches(m) = slice(text, None, Some("x"), 0, 2).unwrap() else {
            panic!("expected Matches");
        };
        assert_eq!(m.total, 4);
        assert_eq!(m.hits.len(), 2);
        assert!(m.truncated);
    }

    #[test]
    fn slice_rejects_bad_regex() {
        assert!(slice("text", None, Some("("), 0, 10).is_err());
    }

    #[test]
    fn parse_downloads_splits_result_and_ulog_by_heading() {
        let html = r#"
            <a href="/tests/7/file/autoinst-log.txt">autoinst-log.txt</a>
            <h2>Uploaded logs</h2>
            <a href="/tests/7/file/my_custom.log">my_custom.log</a>
        "#;
        let files = parse_downloads(html, 7);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, "autoinst-log.txt");
        assert_eq!(files[0].kind, "result");
        assert_eq!(files[1].name, "my_custom.log");
        assert_eq!(files[1].kind, "ulog");
    }

    #[test]
    fn parse_downloads_dedupes_and_ignores_other_jobs() {
        let html = r#"
            <a href="/tests/7/file/a.txt">a.txt</a>
            <a href="/tests/7/file/a.txt">raw</a>
            <a href="/tests/9/file/other.txt">other.txt</a>
        "#;
        let files = parse_downloads(html, 7);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "a.txt");
    }

    #[test]
    fn details_logs_reads_logs_and_ulogs_arrays() {
        let value = serde_json::json!({
            "logs": ["autoinst-log.txt", "y2logs.tar.xz"],
            "ulogs": ["my_custom.log"],
        });
        let files = details_logs(&value);
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].kind, "result");
        assert_eq!(files[2].kind, "ulog");
    }

    #[test]
    fn tar_members_lists_entries_and_reports_truncation() {
        let bytes = build_tar(&[("a.txt", b"hello"), ("b.txt", b"world!")]);
        let (members, truncated) = tar_members(&bytes, 10).unwrap();
        assert_eq!(members.len(), 2);
        assert!(!truncated);
        assert_eq!(members[0].path, "a.txt");
        assert_eq!(members[0].size, 5);

        let (members, truncated) = tar_members(&bytes, 1).unwrap();
        assert_eq!(members.len(), 1);
        assert!(truncated);
    }

    #[test]
    fn extract_tar_member_reads_matching_entry() {
        let bytes = build_tar(&[("a.txt", b"hello"), ("b.txt", b"world!")]);
        let out = extract_tar_member(&bytes, "b.txt", 1024).unwrap();
        assert_eq!(out, b"world!");
    }

    #[test]
    fn extract_tar_member_missing_is_invalid_params() {
        let bytes = build_tar(&[("a.txt", b"hello")]);
        let err = extract_tar_member(&bytes, "missing.txt", 1024).unwrap_err();
        assert!(err.is_err());
    }

    #[test]
    fn decompress_gzip_round_trips() {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b"hello, gzip").unwrap();
        let gz = encoder.finish().unwrap();
        let out = decompress(&gz, Sniff::Gzip, 1024).unwrap();
        assert_eq!(out, b"hello, gzip");
    }

    #[test]
    fn decompress_gzip_bomb_hits_the_ceiling() {
        // Highly compressible input: a small gzip stream that decodes to
        // far more than the tiny ceiling below.
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(&vec![0u8; 1024 * 1024]).unwrap();
        let gz = encoder.finish().unwrap();
        let err = decompress(&gz, Sniff::Gzip, 1024).unwrap_err();
        assert!(err.is_err() || matches!(&err, Ok(r) if r.is_error == Some(true)));
    }

    fn build_tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_cksum();
            builder.append_data(&mut header, name, *data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    #[test]
    fn tap_markers_match_only_their_own_lines() {
        assert!(TAP_MARKERS.is_match("nice05.c:138: TFAIL: executes less cycles"));
        assert!(TAP_MARKERS.is_match("mount08.c:99: TBROK: mount failed"));
        assert!(TAP_MARKERS.is_match("not ok 3 - some assertion"));
        // `^not ok` must anchor at line start, not match a commented-out TAP line.
        assert!(!TAP_MARKERS.is_match("# not ok, this is just a comment"));
        assert!(!TAP_MARKERS.is_match("everything is fine"));
    }

    #[test]
    fn fatal_markers_match_test_died_and_died() {
        assert!(FATAL_MARKERS.is_match("# Test died: 'zypper -n ref' failed with code 4"));
        assert!(FATAL_MARKERS.is_match("the process died unexpectedly"));
        assert!(FATAL_MARKERS.is_match("the process DIED unexpectedly")); // case-insensitive
        assert!(!FATAL_MARKERS.is_match("everything is fine"));
    }

    #[test]
    fn fatal_markers_match_the_workers_own_terminal_verdict_lines() {
        // The worker's own `Result: <reason>` line (OpenQA::Constants'
        // WORKER_SR_* enum) for abnormal terminations that never go through
        // a module death at all, e.g. an asset failing to download.
        assert!(FATAL_MARKERS.is_match("[info] [pid:1] Result: setup failure"));
        assert!(FATAL_MARKERS.is_match("[info] [pid:1] Result: api-failure"));
        assert!(FATAL_MARKERS.is_match("[info] [pid:1] Result: worker broken"));
        assert!(FATAL_MARKERS.is_match("[info] [pid:1] Result: timeout"));
    }

    #[test]
    fn fatal_markers_do_not_match_a_normal_exit() {
        // `done` means the worker exited normally (a job can still fail at
        // the module level with `Result: done` in its log); `finish-off` is
        // a graceful shutdown. Neither is a failure signal.
        assert!(!FATAL_MARKERS.is_match("[info] [pid:1] Result: done"));
        assert!(!FATAL_MARKERS.is_match("[info] [pid:1] Result: finish-off"));
    }

    #[test]
    fn noise_markers_error_colon_is_case_insensitive_but_bare_error_is_not() {
        // `error:` alone only matches the case-insensitive `(?i)\berror:` member.
        assert!(NOISE_MARKERS.is_match("connection error: refused"));
        assert!(NOISE_MARKERS.is_match("connection ERROR: refused"));
        // Bare `error` (no colon) must not match: `\bERROR\b` is case-sensitive
        // and `(?i)\berror:` requires the colon.
        assert!(!NOISE_MARKERS.is_match("a harmless error occurred, retrying"));
        assert!(NOISE_MARKERS.is_match("ERROR"));
        assert!(NOISE_MARKERS.is_match("timeout waiting for serial"));
        assert!(NOISE_MARKERS.is_match("Failed to connect"));
        assert!(NOISE_MARKERS.is_match("command ssh failed"));
    }

    #[test]
    fn noise_markers_timeout_ignores_the_keyword_argument() {
        // Live-checked on a real timeout job (23938357): 23006 lines match
        // a bare `timed? ?out`, and 23003 of them are just the `timeout=`
        // kwarg testapi logs on nearly every debug line, burying the one
        // real line ("Result: timeout") past DIGEST_MAX_HITS.
        assert!(
            !NOISE_MARKERS
                .is_match(r#"<<< testapi::assert_screen(mustmatch="root-console", timeout=30)"#)
        );
        assert!(!NOISE_MARKERS.is_match("], timeout=200)"));
        assert!(!NOISE_MARKERS.is_match(r#"password="SECRET", timeout=undef, username="root""#));
        // Real timeout signals must still match: end of line, followed by
        // punctuation, or followed by whitespace and a non-digit word.
        assert!(NOISE_MARKERS.is_match("Result: timeout"));
        assert!(NOISE_MARKERS.is_match("connection timed out, retrying"));
        assert!(NOISE_MARKERS.is_match("the operation timed out."));
        assert!(NOISE_MARKERS.is_match("timeout waiting for serial"));
    }

    #[test]
    fn noise_markers_timeout_ignores_the_cli_flag_form() {
        // Every job's autoinst-log.txt opens with this exact rsync call
        // (confirmed verbatim across every job checked); `--timeout 1800`
        // is a CLI flag/value pair, not a failure, and would otherwise be
        // the very first (and often only) "generic" tier hit on every job,
        // including ones with no real error at all.
        assert!(!NOISE_MARKERS.is_match(
            "[info] [#2009] Calling: rsync -avHP --timeout 1800 rsync://openqa.suse.de/tests/"
        ));
        // A genuine dracut timeout message must still match even though it
        // is also followed by punctuation rather than whitespace.
        assert!(NOISE_MARKERS.is_match("dracut-initqueue: timeout, still waiting for"));
    }

    #[test]
    fn truncate_line_keeps_short_lines_untouched() {
        assert_eq!(truncate_line("short line", 300), "short line");
    }

    #[test]
    fn truncate_line_truncates_on_a_char_boundary() {
        // Multi-byte chars throughout; truncation must count chars, not
        // bytes, and never panic by slicing mid-character.
        let line: String = "é".repeat(310);
        let truncated = truncate_line(&line, 300);
        assert_eq!(truncated.chars().count(), 301); // 300 chars + the trailing …
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn scan_markers_merges_overlapping_context_without_duplicate_lines() {
        // Matches on adjacent lines 2 and 3 (1-based) with context=1 would
        // naively both claim line 3; each line must appear exactly once.
        let text = "one\nTFAIL here\nTFAIL again\nfour\nfive";
        let result = scan_markers(text, &TAP_MARKERS, 1, DIGEST_MAX_HITS);
        assert_eq!(result.hit_count, 2);
        assert!(!result.more_hits);
        let lines: Vec<usize> = result.hits.iter().map(|h| h.line).collect();
        assert_eq!(lines, vec![1, 2, 3, 4]);
        let unique: std::collections::BTreeSet<_> = lines.iter().collect();
        assert_eq!(unique.len(), lines.len(), "no line emitted twice");
    }

    #[test]
    fn scan_markers_reports_more_hits_past_the_cap() {
        let text = "TFAIL\nTFAIL\nTFAIL\nTFAIL";
        let result = scan_markers(text, &TAP_MARKERS, 0, 2);
        assert_eq!(result.hit_count, 4);
        assert!(result.more_hits);
        assert_eq!(result.hits.len(), 2);
        assert_eq!(result.total_lines, 4);
    }

    #[test]
    fn scan_markers_truncates_hit_text_on_a_char_boundary() {
        let long_line = "é".repeat(310);
        let text = format!("before\n{long_line} TFAIL\nafter");
        let result = scan_markers(&text, &TAP_MARKERS, 0, DIGEST_MAX_HITS);
        assert_eq!(result.hit_count, 1);
        assert!(result.hits[0].text.ends_with('…'));
        assert!(result.hits[0].text.chars().count() <= DIGEST_MAX_LINE_CHARS + 1);
    }

    #[test]
    fn tail_hits_numbers_from_the_start_of_the_file() {
        let text = numbered_text(10);
        let hits = tail_hits(&text, 3);
        let lines: Vec<usize> = hits.iter().map(|h| h.line).collect();
        assert_eq!(lines, vec![8, 9, 10]);
        assert_eq!(hits[0].text, "line08");
    }

    #[test]
    fn tail_hits_shorter_than_n_returns_the_whole_text() {
        let text = numbered_text(2);
        let hits = tail_hits(&text, 30);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].line, 1);
    }

    fn numbered_text(n: usize) -> String {
        (1..=n)
            .map(|i| format!("line{i:02}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn compile_markers_names_the_bad_pattern() {
        let err = compile_markers(&["good".to_string(), "(bad".to_string()]).unwrap_err();
        assert!(err.message.contains("(bad"));
    }

    #[test]
    fn compile_markers_builds_a_working_set() {
        let set = compile_markers(&["foo".to_string(), "bar".to_string()]).unwrap();
        assert!(set.is_match("a foo line"));
        assert!(!set.is_match("neither"));
    }

    #[test]
    fn failed_modules_builds_the_exact_step_deep_link() {
        let details = serde_json::json!({
            "job": {
                "testresults": [
                    {
                        "name": "mount08",
                        "result": "softfailed",
                        "details": [
                            {"num": 1, "title": "wait_serial", "result": "ok"},
                            {"num": 2, "title": "wait_serial", "result": "fail"},
                            // `unk` is a needle-less screenshot os-autoinst
                            // never scored, not a failure — must be dropped.
                            {"num": 3, "title": "", "result": "unk"},
                        ]
                    },
                    {"name": "boot", "result": "passed", "details": []}
                ]
            }
        });
        let base = reqwest::Url::parse("https://openqa.suse.de/").unwrap();
        let modules = failed_modules(&details, 23_222_647, &base);
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].module, "mount08");
        assert_eq!(modules[0].steps.len(), 1);
        assert_eq!(
            modules[0].steps[0].url,
            "https://openqa.suse.de/tests/23222647#step/mount08/2"
        );
    }

    #[test]
    fn failed_modules_caps_at_digest_max_modules() {
        let testresults: Vec<_> = (0..DIGEST_MAX_MODULES + 5)
            .map(
                |i| serde_json::json!({"name": format!("m{i}"), "result": "failed", "details": []}),
            )
            .collect();
        let details = serde_json::json!({"job": {"testresults": testresults}});
        let base = reqwest::Url::parse("https://openqa.suse.de/").unwrap();
        let modules = failed_modules(&details, 1, &base);
        assert_eq!(modules.len(), DIGEST_MAX_MODULES);
    }

    #[test]
    fn has_log_checks_logs_and_ulogs_under_the_job_wrapper() {
        let details = serde_json::json!({
            "job": {"logs": ["autoinst-log.txt", "serial_terminal.txt"], "ulogs": []}
        });
        assert!(has_log(&details, "serial_terminal.txt"));
        assert!(!has_log(&details, "video.webm"));
    }
}
