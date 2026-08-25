//! JSONL audit stream: one line per session lifecycle event and tool call.
//!
//! Disabled unless `--audit-config`/`OPENQA_MCP_AUDIT_CONFIG` names a TOML
//! file. When disabled, [`OpenQaServer`](crate::server::OpenQaServer) holds no
//! [`Auditor`] at all, so a call touches no mutex and writes nothing.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

use crate::LogProducer;
use crate::otel::{logs, proto};

/// Which transport served a session; carried on every record. Also the CLI's
/// `--transport` value type — the two vocabularies must agree, since a
/// mismatch would change flag values and audit records together.
#[derive(Debug, Clone, Copy, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Stdio,
    Http,
}

impl Transport {
    fn as_str(self) -> &'static str {
        match self {
            Transport::Stdio => "stdio",
            Transport::Http => "http",
        }
    }
}

/// A tool's mutating classification, or `none` for a session-level event.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordScope {
    Read,
    Write,
    None,
}

/// What kind of record this line represents.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    SessionOpen,
    ToolCall,
    /// Reserved for the fail-closed gate (a later phase); the schema is
    /// pinned here but nothing in this phase emits it.
    AuditGap,
    Shutdown,
}

impl Event {
    fn as_str(self) -> &'static str {
        match self {
            Event::SessionOpen => "session_open",
            Event::ToolCall => "tool_call",
            Event::AuditGap => "audit_gap",
            Event::Shutdown => "shutdown",
        }
    }
}

/// A tool call's result, reusing `error.rs`'s `kind` vocabulary so an audit
/// record and the caller's own response can never disagree about what
/// happened.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    ToolError {
        kind: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
    },
    ProtocolError {
        code: i32,
    },
}

/// One JSONL line. Field order is declaration order, which is the audit
/// stream's on-disk schema.
#[derive(Debug, Serialize)]
pub struct Record {
    v: u8,
    ts: String,
    seq: u64,
    session: String,
    transport: Transport,
    scope: RecordScope,
    event: Event,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    args: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<Outcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
}

impl Record {
    /// A session-level event with no tool-call fields. `seq`/`ts` are
    /// placeholders, overwritten by [`Auditor::write`] while the sink's lock
    /// is held.
    fn event(session: String, transport: Transport, scope: RecordScope, event: Event) -> Self {
        Self {
            v: 1,
            ts: String::new(),
            seq: 0,
            session,
            transport,
            scope,
            event,
            tool: None,
            server: None,
            args: None,
            outcome: None,
            duration_ms: None,
        }
    }
}

/// Argument names recorded verbatim; everything else becomes `{"_len": n}`.
/// Deny-by-default: a tool gaining a new argument is invisible until someone
/// adds it here, which is the safe direction.
const ARGS_ALLOW: &[&str] = &[
    "server",
    "job_id",
    "job_ids",
    "ids",
    "group_id",
    "groupid",
    "parent_group_id",
    "comment_id",
    "scheduled_product_id",
    "asset_id",
    "bugid",
    "build",
    "distri",
    "version",
    "flavor",
    "arch",
    "machine",
    "test",
    "result",
    "state",
    "group",
    "text",
    "title",
    "prio",
    "force",
    "dup_type_auto",
    "filename",
    "member",
    "tool",
    "tier",
];

/// Allow-listed arrays beyond this many elements are summarized instead of
/// recorded verbatim.
const ARGS_MAX_ARRAY: usize = 20;

/// Apply the argument capture policy to a tool call's arguments: allow-listed
/// scalars and short arrays verbatim, everything else as `{"_len": n}`.
/// `None` for no arguments at all.
#[must_use]
pub fn capture_args(args: Option<&serde_json::Map<String, Value>>) -> Option<Value> {
    let args = args?;
    if args.is_empty() {
        return None;
    }
    let mut out = serde_json::Map::with_capacity(args.len());
    for (key, value) in args {
        let captured = if ARGS_ALLOW.contains(&key.as_str()) {
            capture_allowed(value)
        } else {
            summarize(value)
        };
        out.insert(key.clone(), captured);
    }
    Some(Value::Object(out))
}

fn capture_allowed(value: &Value) -> Value {
    match value {
        Value::Array(items) if items.len() > ARGS_MAX_ARRAY => summarize(value),
        _ => value.clone(),
    }
}

fn summarize(value: &Value) -> Value {
    let len = match value {
        Value::String(s) => s.len(),
        Value::Array(a) => a.len(),
        Value::Object(o) => o.len(),
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
    };
    json!({ "_len": len })
}

/// `open` | `closed_writes` | `closed_all`. Parsed and validated in this
/// phase; gating on it is a later phase's job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailMode {
    Open,
    ClosedWrites,
    ClosedAll,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAuditConfig {
    path: String,
    #[serde(default)]
    fsync: bool,
    #[serde(default = "default_fail_mode")]
    fail_mode: FailMode,
    #[serde(default = "default_rotate_max_bytes")]
    rotate_max_bytes: u64,
    #[serde(default = "default_rotate_keep")]
    rotate_keep: u32,
}

fn default_fail_mode() -> FailMode {
    FailMode::Open
}

fn default_rotate_max_bytes() -> u64 {
    64 * 1024 * 1024
}

fn default_rotate_keep() -> u32 {
    8
}

/// The audit stream's configuration, loaded from a strict TOML file (unknown
/// keys are a startup error).
#[derive(Debug, Clone)]
pub struct AuditConfig {
    /// `None` when `path = "none"`: no file sink at all.
    pub path: Option<PathBuf>,
    pub fsync: bool,
    pub fail_mode: FailMode,
    pub rotate_max_bytes: u64,
    /// Clamped to `1..=10_000`.
    pub rotate_keep: usize,
}

/// Why an audit-stream TOML configuration failed to parse.
#[derive(Debug)]
pub struct ConfigError(toml::de::Error);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid audit configuration: {}", self.0)
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl AuditConfig {
    /// Parse a TOML document into an [`AuditConfig`].
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] naming the offending key for an unknown or
    /// missing field, or for an unrecognized `fail_mode`.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let raw: RawAuditConfig = toml::from_str(text).map_err(ConfigError)?;
        let path = (raw.path != "none").then(|| PathBuf::from(raw.path));
        Ok(Self {
            path,
            fsync: raw.fsync,
            fail_mode: raw.fail_mode,
            rotate_max_bytes: raw.rotate_max_bytes,
            rotate_keep: usize::try_from(raw.rotate_keep.clamp(1, 10_000)).unwrap_or(10_000),
        })
    }
}

/// The append-only file behind the audit stream.
struct Sink {
    path: PathBuf,
    file: File,
    len: u64,
    fsync: bool,
    rotate_max_bytes: u64,
    rotate_keep: usize,
}

impl Sink {
    /// Open (or create) the sink named by `cfg.path`. `Ok(None)` when auditing
    /// has no file sink at all (`path = "none"`).
    fn open(cfg: &AuditConfig) -> io::Result<Option<Self>> {
        let Some(path) = cfg.path.clone() else {
            return Ok(None);
        };
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            create_audit_dir(parent)?;
        }
        let existed = match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(io::Error::other(format!(
                    "audit path {} is a symlink; refusing to open it",
                    path.display()
                )));
            }
            Ok(_) => true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(e),
        };
        let file = open_append(&path).map_err(|e| symlink_or(e, &path))?;
        #[cfg(unix)]
        if existed {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        #[cfg(not(unix))]
        let _ = existed;
        let len = file.metadata()?.len();
        Ok(Some(Self {
            path,
            file,
            len,
            fsync: cfg.fsync,
            rotate_max_bytes: cfg.rotate_max_bytes,
            rotate_keep: cfg.rotate_keep,
        }))
    }

    /// Append one line (a newline is added). Rotates first if the write would
    /// cross `rotate_max_bytes`, unless the live file is already empty (a
    /// single oversized record goes into an empty file as-is).
    fn append(&mut self, line: &str) -> io::Result<()> {
        use std::io::Write;

        let mut buf = Vec::with_capacity(line.len() + 1);
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
        let buf_len = buf.len() as u64;
        if self.rotate_max_bytes > 0 && self.len > 0 && self.len + buf_len > self.rotate_max_bytes {
            self.rotate()?;
        }
        self.file.write_all(&buf)?;
        if self.fsync {
            self.file.sync_data()?;
        }
        self.len += buf_len;
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        let keep = self.rotate_keep;
        ignore_not_found(std::fs::remove_file(numbered(&self.path, keep)))?;
        for i in (1..keep).rev() {
            ignore_not_found(std::fs::rename(
                numbered(&self.path, i),
                numbered(&self.path, i + 1),
            ))?;
        }
        std::fs::rename(&self.path, numbered(&self.path, 1))?;
        self.file = open_append(&self.path)?;
        self.len = 0;
        Ok(())
    }
}

fn create_audit_dir(parent: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(parent)
}

fn open_append(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.append(true).create(true);
    #[cfg(unix)]
    {
        opts.mode(0o600);
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    opts.open(path)
}

/// `ELOOP` from `O_NOFOLLOW` is the race-free symlink signal; turn it into a
/// message that names the word, matching the pre-check above.
#[cfg_attr(
    not(unix),
    allow(unused_variables, unused_mut, clippy::needless_pass_by_value)
)]
fn symlink_or(e: io::Error, path: &Path) -> io::Error {
    #[cfg(unix)]
    if e.raw_os_error() == Some(libc::ELOOP) {
        return io::Error::other(format!("audit path {} is a symlink: {e}", path.display()));
    }
    e
}

fn ignore_not_found(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// `path.1`, `path.2`, … — the full path with a numeric suffix appended.
fn numbered(path: &Path, n: usize) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

/// A per-process id for stdio sessions and for `initialize` (which has no
/// `Mcp-Session-Id` yet): pid plus start time, deliberately not random.
fn process_session_id() -> String {
    let start_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("p{}-{start_ms}", std::process::id())
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// The audit stream's write side: session id, sequence numbering, the
/// (optional) file sink, and the (optional) OTLP bridge.
pub struct Auditor {
    /// Always a real `Mutex`, even with no file sink: the lock is what makes
    /// `seq` order equal emission order, and the OTLP export must inherit
    /// that guarantee too, not just the file.
    sink: Mutex<Option<Sink>>,
    otlp: Option<LogProducer>,
    seq: AtomicU64,
    process_session: String,
}

impl Auditor {
    /// Open the sink named by `cfg`, if any.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the sink's parent directory or file could
    /// not be created, or if the path is a symlink.
    pub fn open(cfg: &AuditConfig) -> io::Result<Self> {
        Ok(Self {
            sink: Mutex::new(Sink::open(cfg)?),
            otlp: None,
            seq: AtomicU64::new(1),
            process_session: process_session_id(),
        })
    }

    /// Bridge every record onto the OTLP logs pipeline too, tagged
    /// `ruoqa.stream = "audit"`. `path = "none"` plus this is what makes a
    /// collector the *only* audit sink.
    #[must_use]
    pub fn with_otlp(mut self, producer: LogProducer) -> Self {
        self.otlp = Some(producer);
        self
    }

    /// The per-process session id used over stdio and at `initialize`.
    #[must_use]
    pub fn process_session(&self) -> &str {
        &self.process_session
    }

    /// Assign `seq`/`ts`, serialise, and append/export, all under the same
    /// lock so file order and OTLP emission order both equal sequence order.
    /// A no-op — no lock contention, no seq spent — only when neither a file
    /// sink nor an OTLP producer is configured.
    fn write(&self, mut record: Record) {
        let mut guard = self
            .sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.is_none() && self.otlp.is_none() {
            return;
        }
        record.seq = self.seq.fetch_add(1, Ordering::SeqCst);
        record.ts = now_rfc3339();
        let line = serde_json::to_string(&record).expect("Record serialization cannot fail");
        if let Some(sink) = guard.as_mut()
            && let Err(e) = sink.append(&line)
        {
            tracing::warn!(error = %e, path = %sink.path.display(), "audit sink append failed");
        }
        if let Some(otlp) = &self.otlp {
            otlp.enqueue(encode_otlp_record(&record, &line));
        }
    }

    pub fn session_open(&self, session: impl Into<String>, transport: Transport) {
        self.write(Record::event(
            session.into(),
            transport,
            RecordScope::None,
            Event::SessionOpen,
        ));
    }

    pub fn shutdown(&self, session: impl Into<String>, transport: Transport) {
        self.write(Record::event(
            session.into(),
            transport,
            RecordScope::None,
            Event::Shutdown,
        ));
    }

    #[allow(clippy::too_many_arguments)]
    pub fn tool_call(
        &self,
        session: impl Into<String>,
        transport: Transport,
        scope: RecordScope,
        tool: impl Into<String>,
        server: Option<String>,
        args: Option<Value>,
        outcome: Outcome,
        duration_ms: u64,
    ) {
        let mut record = Record::event(
            session.into(),
            transport,
            RecordScope::None,
            Event::ToolCall,
        );
        record.scope = scope;
        record.tool = Some(tool.into());
        record.server = server;
        record.args = args;
        record.outcome = Some(outcome);
        record.duration_ms = Some(duration_ms);
        self.write(record);
    }
}

/// A record's OTLP attribute values. Owned, unlike `otel::proto::Value`,
/// because the flattened `outcome`/`error.kind` strings (`"rpc_<code>"`) do
/// not borrow from anything that outlives this function.
#[derive(Debug, PartialEq)]
enum AuditAttr {
    Str(String),
    Int(i64),
}

impl AuditAttr {
    fn as_proto(&self) -> proto::Value<'_> {
        match self {
            AuditAttr::Str(s) => proto::Value::Str(s),
            AuditAttr::Int(i) => proto::Value::Int(*i),
        }
    }
}

/// Computes one [`Record`]'s OTLP severity and attributes, independent of
/// encoding: `outcome` is flattened for querying (the serialised form is a
/// tagged object; an attribute must be a scalar), reusing `error.rs`'s `kind`
/// vocabulary so an audit record, a tool response and a metric attribute
/// never describe the same failure three ways.
fn otlp_attributes(record: &Record) -> (logs::Severity, Vec<(&'static str, AuditAttr)>) {
    let severity = if matches!(record.event, Event::AuditGap) {
        logs::Severity::Error
    } else {
        logs::Severity::Info
    };
    let mut attrs = vec![
        ("ruoqa.stream", AuditAttr::Str("audit".to_string())),
        ("event", AuditAttr::Str(record.event.as_str().to_string())),
        (
            "seq",
            AuditAttr::Int(i64::try_from(record.seq).unwrap_or(i64::MAX)),
        ),
        (
            "transport",
            AuditAttr::Str(record.transport.as_str().to_string()),
        ),
    ];
    if let Some(tool) = &record.tool {
        attrs.push(("tool", AuditAttr::Str(tool.clone())));
    }
    if let Some(server) = &record.server {
        attrs.push(("server", AuditAttr::Str(server.clone())));
    }
    match &record.outcome {
        Some(Outcome::Ok) => attrs.push(("outcome", AuditAttr::Str("ok".to_string()))),
        Some(Outcome::ToolError { kind, .. }) => {
            attrs.push(("outcome", AuditAttr::Str("tool_error".to_string())));
            attrs.push(("error.kind", AuditAttr::Str(kind.clone())));
        }
        Some(Outcome::ProtocolError { code }) => {
            attrs.push(("outcome", AuditAttr::Str("protocol_error".to_string())));
            attrs.push(("error.kind", AuditAttr::Str(format!("rpc_{code}"))));
        }
        None => {}
    }
    (severity, attrs)
}

/// Encodes one audit `Record` as a `LogRecord`. `body` is `line` verbatim —
/// the same JSONL string just appended to the file, minus the trailing
/// `'\n'` — so the exported body and the file line are byte-equal.
fn encode_otlp_record(record: &Record, line: &str) -> Vec<u8> {
    let (severity, attrs) = otlp_attributes(record);
    let attr_refs: Vec<(&str, proto::Value<'_>)> =
        attrs.iter().map(|(k, v)| (*k, v.as_proto())).collect();
    logs::encode_record(logs::now_unix_nanos(), severity, line, &attr_refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(record: &Record) -> String {
        serde_json::to_string(record).unwrap()
    }

    #[test]
    fn session_open_serializes_pinned_shape() {
        let record = Record::event(
            "s1".to_string(),
            Transport::Stdio,
            RecordScope::None,
            Event::SessionOpen,
        );
        assert_eq!(
            line(&record),
            r#"{"v":1,"ts":"","seq":0,"session":"s1","transport":"stdio","scope":"none","event":"session_open"}"#
        );
    }

    #[test]
    fn shutdown_serializes_pinned_shape() {
        let record = Record::event(
            "s1".to_string(),
            Transport::Http,
            RecordScope::None,
            Event::Shutdown,
        );
        assert_eq!(
            line(&record),
            r#"{"v":1,"ts":"","seq":0,"session":"s1","transport":"http","scope":"none","event":"shutdown"}"#
        );
    }

    #[test]
    fn audit_gap_serializes_pinned_shape_though_unused_in_this_phase() {
        let record = Record::event(
            "s1".to_string(),
            Transport::Stdio,
            RecordScope::None,
            Event::AuditGap,
        );
        assert_eq!(
            line(&record),
            r#"{"v":1,"ts":"","seq":0,"session":"s1","transport":"stdio","scope":"none","event":"audit_gap"}"#
        );
    }

    #[test]
    fn tool_call_ok_serializes_pinned_shape() {
        let mut record = Record::event(
            "s1".to_string(),
            Transport::Http,
            RecordScope::Write,
            Event::ToolCall,
        );
        record.tool = Some("add_job_comment".to_string());
        record.server = Some("openqa.suse.de".to_string());
        record.args = Some(json!({"text": "hi"}));
        record.outcome = Some(Outcome::Ok);
        record.duration_ms = Some(12);
        assert_eq!(
            line(&record),
            r#"{"v":1,"ts":"","seq":0,"session":"s1","transport":"http","scope":"write","event":"tool_call","tool":"add_job_comment","server":"openqa.suse.de","args":{"text":"hi"},"outcome":"ok","duration_ms":12}"#
        );
    }

    #[test]
    fn tool_call_tool_error_serializes_pinned_shape() {
        let mut record = Record::event(
            "s1".to_string(),
            Transport::Http,
            RecordScope::Read,
            Event::ToolCall,
        );
        record.tool = Some("get_job".to_string());
        record.outcome = Some(Outcome::ToolError {
            kind: "not_found".to_string(),
            status: Some(404),
        });
        assert_eq!(
            line(&record),
            r#"{"v":1,"ts":"","seq":0,"session":"s1","transport":"http","scope":"read","event":"tool_call","tool":"get_job","outcome":{"tool_error":{"kind":"not_found","status":404}}}"#
        );
    }

    #[test]
    fn tool_call_protocol_error_serializes_pinned_shape() {
        let mut record = Record::event(
            "s1".to_string(),
            Transport::Stdio,
            RecordScope::Write,
            Event::ToolCall,
        );
        record.tool = Some("cancel_job".to_string());
        record.outcome = Some(Outcome::ProtocolError { code: -32602 });
        assert_eq!(
            line(&record),
            r#"{"v":1,"ts":"","seq":0,"session":"s1","transport":"stdio","scope":"write","event":"tool_call","tool":"cancel_job","outcome":{"protocol_error":{"code":-32602}}}"#
        );
    }

    #[test]
    fn capture_args_none_for_no_arguments() {
        assert_eq!(capture_args(None), None);
        assert_eq!(capture_args(Some(&serde_json::Map::new())), None);
    }

    #[test]
    fn capture_args_allow_listed_scalars_verbatim() {
        let args = json!({"job_id": 7, "text": "hi", "server": "osd"});
        let captured = capture_args(args.as_object()).unwrap();
        assert_eq!(captured["job_id"], 7);
        assert_eq!(captured["text"], "hi");
        assert_eq!(captured["server"], "osd");
    }

    #[test]
    fn capture_args_denies_by_default() {
        let args = json!({"extra": {"a": 1, "b": 2}, "markers": ["x", "y", "z"], "q": "boot"});
        let captured = capture_args(args.as_object()).unwrap();
        assert_eq!(captured["extra"], json!({"_len": 2}));
        assert_eq!(captured["markers"], json!({"_len": 3}));
        assert_eq!(captured["q"], json!({"_len": 4}));
    }

    #[test]
    fn capture_args_allow_listed_array_capped_then_summarized() {
        let max = i64::try_from(ARGS_MAX_ARRAY).unwrap();
        let short: Vec<i64> = (0..max).collect();
        let args = json!({"job_ids": short});
        let captured = capture_args(args.as_object()).unwrap();
        assert_eq!(captured["job_ids"], json!(short));

        let long: Vec<i64> = (0..=max).collect();
        let args = json!({"job_ids": long});
        let captured = capture_args(args.as_object()).unwrap();
        assert_eq!(captured["job_ids"], json!({"_len": ARGS_MAX_ARRAY + 1}));
    }

    #[test]
    fn capture_args_text_uncapped() {
        let big_text = "x".repeat(1_000_000);
        let args = json!({"text": big_text});
        let captured = capture_args(args.as_object()).unwrap();
        assert_eq!(captured["text"].as_str().unwrap().len(), 1_000_000);
    }

    #[test]
    fn config_defaults() {
        let cfg = AuditConfig::parse("path = \"/var/log/audit.jsonl\"\n").unwrap();
        assert_eq!(cfg.path, Some(PathBuf::from("/var/log/audit.jsonl")));
        assert!(!cfg.fsync);
        assert_eq!(cfg.fail_mode, FailMode::Open);
        assert_eq!(cfg.rotate_max_bytes, 64 * 1024 * 1024);
        assert_eq!(cfg.rotate_keep, 8);
    }

    #[test]
    fn config_path_none_disables_the_file() {
        let cfg = AuditConfig::parse("path = \"none\"\n").unwrap();
        assert_eq!(cfg.path, None);
    }

    #[test]
    fn config_unknown_key_names_the_key() {
        let err = AuditConfig::parse("path = \"none\"\nfsinc = true\n").unwrap_err();
        assert!(err.to_string().contains("fsinc"), "{err}");
    }

    #[test]
    fn config_invalid_fail_mode_is_an_error() {
        let err = AuditConfig::parse("path = \"none\"\nfail_mode = \"bogus\"\n").unwrap_err();
        assert!(err.to_string().contains("fail_mode") || err.to_string().contains("bogus"));
    }

    #[test]
    fn config_rotate_keep_clamps_low() {
        let cfg = AuditConfig::parse("path = \"none\"\nrotate_keep = 0\n").unwrap();
        assert_eq!(cfg.rotate_keep, 1);
    }

    #[test]
    fn config_rotate_keep_clamps_high() {
        let cfg = AuditConfig::parse("path = \"none\"\nrotate_keep = 99999\n").unwrap();
        assert_eq!(cfg.rotate_keep, 10_000);
    }

    #[test]
    fn config_missing_path_is_an_error() {
        let err = AuditConfig::parse("fsync = true\n").unwrap_err();
        assert!(err.to_string().contains("path"), "{err}");
    }

    #[test]
    fn sink_none_when_path_is_none() {
        let cfg = AuditConfig::parse("path = \"none\"\n").unwrap();
        assert!(Sink::open(&cfg).unwrap().is_none());
    }

    #[test]
    fn sink_writes_records_and_reopens_with_correct_modes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/audit.jsonl");
        let cfg = AuditConfig::parse(&format!("path = {:?}\n", path.to_str().unwrap())).unwrap();
        let auditor = Auditor::open(&cfg).unwrap();
        for i in 0..5 {
            auditor.session_open(format!("s{i}"), Transport::Stdio);
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 5);
        for line in &lines {
            let parsed: Value = serde_json::from_str(line).unwrap();
            assert_eq!(parsed["v"], 1);
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(file_mode, 0o600);
            let dir_mode = std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
        }
    }

    #[cfg(unix)]
    #[test]
    fn sink_open_rejects_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.jsonl");
        std::fs::write(&real, "").unwrap();
        let link = dir.path().join("audit.jsonl");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let cfg = AuditConfig::parse(&format!("path = {:?}\n", link.to_str().unwrap())).unwrap();
        let Err(err) = Auditor::open(&cfg) else {
            panic!("expected Auditor::open to reject a symlink");
        };
        assert!(err.to_string().contains("symlink"), "{err}");
    }

    #[test]
    fn seq_is_gapless_and_file_order_matches_seq_order_under_concurrency() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let cfg = AuditConfig::parse(&format!("path = {:?}\n", path.to_str().unwrap())).unwrap();
        let auditor = std::sync::Arc::new(Auditor::open(&cfg).unwrap());

        std::thread::scope(|scope| {
            for t in 0..8 {
                let auditor = std::sync::Arc::clone(&auditor);
                scope.spawn(move || {
                    for i in 0..50 {
                        auditor.tool_call(
                            "s",
                            Transport::Stdio,
                            RecordScope::Read,
                            format!("t{t}-{i}"),
                            None,
                            None,
                            Outcome::Ok,
                            0,
                        );
                    }
                });
            }
        });

        let text = std::fs::read_to_string(&path).unwrap();
        let seqs: Vec<u64> = text
            .lines()
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["seq"]
                    .as_u64()
                    .unwrap()
            })
            .collect();
        let expected: Vec<u64> = (1..=400).collect();
        assert_eq!(seqs, expected, "file order must equal seq order, gaplessly");
    }

    #[test]
    fn rotation_keeps_the_configured_generations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let cfg = AuditConfig::parse(&format!(
            "path = {:?}\nrotate_max_bytes = 1024\nrotate_keep = 3\n",
            path.to_str().unwrap()
        ))
        .unwrap();
        let auditor = Auditor::open(&cfg).unwrap();
        // Each session_open line is small; enough iterations rotate several times.
        for i in 0..400 {
            auditor.session_open(format!("s{i}"), Transport::Stdio);
        }

        assert!(path.exists());
        assert!(numbered(&path, 1).exists());
        assert!(numbered(&path, 2).exists());
        assert!(numbered(&path, 3).exists());
        assert!(!numbered(&path, 4).exists());
    }

    #[test]
    fn a_record_larger_than_the_rotation_bound_lands_intact_without_looping() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let cfg = AuditConfig::parse(&format!(
            "path = {:?}\nrotate_max_bytes = 1024\nrotate_keep = 3\n",
            path.to_str().unwrap()
        ))
        .unwrap();
        let auditor = Auditor::open(&cfg).unwrap();
        let big_text = "x".repeat(4096);
        auditor.tool_call(
            "s",
            Transport::Stdio,
            RecordScope::Write,
            "add_job_comment",
            None,
            Some(json!({"text": big_text})),
            Outcome::Ok,
            0,
        );

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(!numbered(&path, 1).exists());
    }

    fn stub_producer() -> (LogProducer, tokio::sync::mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        (
            LogProducer(crate::otel::pipeline::Producer::for_test(tx)),
            rx,
        )
    }

    #[test]
    fn path_none_with_otlp_still_numbers_seq_and_creates_no_file() {
        let cfg = AuditConfig::parse("path = \"none\"\n").unwrap();
        let (producer, mut rx) = stub_producer();
        let auditor = Auditor::open(&cfg).unwrap().with_otlp(producer);

        for i in 0..3 {
            auditor.session_open(format!("s{i}"), Transport::Stdio);
        }

        // The defect this restructure fixes: without it, `seq` never
        // advances when there is no file sink, so every exported record
        // would carry `seq: 0`.
        assert_eq!(auditor.seq.load(Ordering::SeqCst), 4);
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err(), "exactly 3 records enqueued");
    }

    #[test]
    fn otlp_and_file_both_receive_every_record_when_both_are_configured() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let cfg = AuditConfig::parse(&format!("path = {:?}\n", path.to_str().unwrap())).unwrap();
        let (producer, mut rx) = stub_producer();
        let auditor = Auditor::open(&cfg).unwrap().with_otlp(producer);

        auditor.session_open("s0", Transport::Stdio);

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn otlp_attributes_ok_outcome() {
        let mut record = Record::event(
            "s".to_string(),
            Transport::Http,
            RecordScope::Write,
            Event::ToolCall,
        );
        record.seq = 7;
        record.tool = Some("add_job_comment".to_string());
        record.server = Some("osd".to_string());
        record.outcome = Some(Outcome::Ok);
        let (severity, attrs) = otlp_attributes(&record);
        assert_eq!(severity, logs::Severity::Info);
        assert_eq!(
            attrs,
            vec![
                ("ruoqa.stream", AuditAttr::Str("audit".to_string())),
                ("event", AuditAttr::Str("tool_call".to_string())),
                ("seq", AuditAttr::Int(7)),
                ("transport", AuditAttr::Str("http".to_string())),
                ("tool", AuditAttr::Str("add_job_comment".to_string())),
                ("server", AuditAttr::Str("osd".to_string())),
                ("outcome", AuditAttr::Str("ok".to_string())),
            ]
        );
    }

    #[test]
    fn otlp_attributes_tool_error_outcome_carries_error_kind() {
        let mut record = Record::event(
            "s".to_string(),
            Transport::Http,
            RecordScope::Read,
            Event::ToolCall,
        );
        record.outcome = Some(Outcome::ToolError {
            kind: "not_found".to_string(),
            status: Some(404),
        });
        let (_, attrs) = otlp_attributes(&record);
        assert!(attrs.contains(&("outcome", AuditAttr::Str("tool_error".to_string()))));
        assert!(attrs.contains(&("error.kind", AuditAttr::Str("not_found".to_string()))));
    }

    #[test]
    fn otlp_attributes_protocol_error_outcome_carries_rpc_error_kind() {
        let mut record = Record::event(
            "s".to_string(),
            Transport::Stdio,
            RecordScope::Write,
            Event::ToolCall,
        );
        record.outcome = Some(Outcome::ProtocolError { code: -32602 });
        let (_, attrs) = otlp_attributes(&record);
        assert!(attrs.contains(&("outcome", AuditAttr::Str("protocol_error".to_string()))));
        assert!(attrs.contains(&("error.kind", AuditAttr::Str("rpc_-32602".to_string()))));
    }

    #[test]
    fn otlp_attributes_audit_gap_is_error_severity() {
        let record = Record::event(
            "s".to_string(),
            Transport::Stdio,
            RecordScope::None,
            Event::AuditGap,
        );
        let (severity, _) = otlp_attributes(&record);
        assert_eq!(severity, logs::Severity::Error);
    }
}
