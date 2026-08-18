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
use regex::Regex;
use reqwest::header::{CONTENT_RANGE, ETAG, HeaderMap, HeaderValue, LAST_MODIFIED, RANGE};
use reqwest::{Method, StatusCode};
use rmcp::model::CallToolResult;
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer};
use ruoqa::PreparedRequest;
use serde::Serialize;

use crate::error::{classify, status_kind, tool_error};
use crate::heartbeat::with_heartbeat;

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
}
