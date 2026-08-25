//! `LogRecord` / `ExportLogsServiceRequest` encoding, the diagnostics
//! `tracing` `Layer`, and the field-capture `Visit` impl behind it.

use super::proto::{self, Value};

/// `SeverityNumber`. Five values, not the full 24: `tracing` has exactly
/// five levels, and the intra-level `*2`/`*3`/`*4` gradations the full OTLP
/// table defines have no source in this crate — an enum with 19
/// unconstructible variants would be worse than one that matches reality.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Severity {
    Trace = 1,
    Debug = 5,
    Info = 9,
    Warn = 13,
    Error = 17,
}

impl Severity {
    fn text(self) -> &'static str {
        match self {
            Severity::Trace => "TRACE",
            Severity::Debug => "DEBUG",
            Severity::Info => "INFO",
            Severity::Warn => "WARN",
            Severity::Error => "ERROR",
        }
    }

    /// Maps a `tracing::Level` onto its `SeverityNumber`.
    pub(crate) fn from_level(level: tracing::Level) -> Self {
        match level {
            tracing::Level::TRACE => Severity::Trace,
            tracing::Level::DEBUG => Severity::Debug,
            tracing::Level::INFO => Severity::Info,
            tracing::Level::WARN => Severity::Warn,
            tracing::Level::ERROR => Severity::Error,
        }
    }
}

/// Encodes one `LogRecord`'s own fields — **not** wrapped in an outer LEN
/// tag. The caller (the export pipeline) splices this straight into a
/// `ScopeLogs.log_records` field via `proto::write_bytes`, which already
/// produces the exact LEN-framing a nested message needs.
///
/// `observed_time_unix_nano` is set to `ts_unix_nanos` too: there is no
/// separate observation step in this crate, the record is built and
/// observed in the same call.
pub(crate) fn encode_record(
    ts_unix_nanos: u64,
    severity: Severity,
    body: &str,
    attrs: &[(&str, Value<'_>)],
) -> Vec<u8> {
    let mut buf = Vec::new();
    proto::write_fixed64(&mut buf, 1, ts_unix_nanos);
    proto::write_uint32(&mut buf, 2, severity as u32);
    proto::write_string(&mut buf, 3, severity.text());
    proto::write_any_value(&mut buf, 5, &Value::Str(body));
    proto::write_attributes(&mut buf, 6, attrs);
    proto::write_fixed64(&mut buf, 11, ts_unix_nanos);
    buf
}

/// Wraps a batch of already-[`encode_record`]d bodies into a full
/// `ExportLogsServiceRequest` carrying exactly one `Resource` and one
/// `InstrumentationScope`, matching this crate's one-resource-per-process
/// model.
pub(crate) fn encode_request(
    resource: &[(&str, Value<'_>)],
    scope: (&str, &str),
    records: &[Vec<u8>],
) -> Vec<u8> {
    let mut buf = Vec::new();
    // ExportLogsServiceRequest.resource_logs (field 1) -> ResourceLogs
    proto::write_message(&mut buf, 1, |resource_logs| {
        proto::write_resource(resource_logs, 1, resource);
        // ResourceLogs.scope_logs (field 2) -> ScopeLogs
        proto::write_message(resource_logs, 2, |scope_logs| {
            proto::write_scope(scope_logs, 1, scope.0, scope.1);
            for record in records {
                proto::write_bytes(scope_logs, 2, record);
            }
        });
    });
    buf
}

/// The current wall-clock time as OTLP's `time_unix_nano`. `0` on a clock
/// error rather than a panic: a bad timestamp is not worth losing the record
/// over.
pub(crate) fn now_unix_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// Longest an attribute's key/value, or the record body, may be after
/// truncation. Reused from `error.rs`'s tool-error body cap: one truncation
/// rule for every byte this crate ever puts on the wire to something else.
const MAX_ATTR_BYTES: usize = 1024;

/// Most attributes a single event may carry, not counting the two
/// unconditionally-added ones (`ruoqa.stream`, `code.target`). Without a
/// cap, one `tracing::debug!` carrying dozens of fields — or a 32 MiB log
/// body via `get_job_log` — would be enqueued whole.
const MAX_ATTRIBUTES: usize = 32;

/// An attribute value captured off a `tracing` event, before it borrows back
/// into a [`Value`] for encoding. Owned because the visitor outlives the
/// borrows `tracing::field::Visit`'s `&str`/`&dyn Debug` parameters give it.
enum AttrValue {
    Str(String),
    Int(i64),
    Bool(bool),
    Double(f64),
}

impl AttrValue {
    fn as_proto(&self) -> Value<'_> {
        match self {
            AttrValue::Str(s) => Value::Str(s),
            AttrValue::Int(i) => Value::Int(*i),
            AttrValue::Bool(b) => Value::Bool(*b),
            AttrValue::Double(d) => Value::Double(*d),
        }
    }

    fn into_body_string(self) -> String {
        match self {
            AttrValue::Str(s) => s,
            AttrValue::Int(i) => i.to_string(),
            AttrValue::Bool(b) => b.to_string(),
            AttrValue::Double(d) => d.to_string(),
        }
    }
}

/// Collects one event's fields: `message` becomes the record body, every
/// other field an attribute, both capped at [`MAX_ATTR_BYTES`] and the
/// attribute count at [`MAX_ATTRIBUTES`].
#[derive(Default)]
struct AttributeVisitor {
    body: Option<String>,
    attrs: Vec<(String, AttrValue)>,
}

impl AttributeVisitor {
    fn capture(&mut self, field: &tracing::field::Field, value: AttrValue) {
        if field.name() == "message" {
            let text = value.into_body_string();
            self.body = Some(crate::error::truncate(&text, MAX_ATTR_BYTES).to_string());
            return;
        }
        if self.attrs.len() >= MAX_ATTRIBUTES {
            return;
        }
        let key = crate::error::truncate(field.name(), MAX_ATTR_BYTES).to_string();
        let value = match value {
            AttrValue::Str(s) => {
                AttrValue::Str(crate::error::truncate(&s, MAX_ATTR_BYTES).to_string())
            }
            other => other,
        };
        self.attrs.push((key, value));
    }
}

impl tracing::field::Visit for AttributeVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.capture(field, AttrValue::Str(value.to_string()));
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.capture(field, AttrValue::Bool(value));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.capture(field, AttrValue::Int(value));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.capture(
            field,
            AttrValue::Int(i64::try_from(value).unwrap_or(i64::MAX)),
        );
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.capture(field, AttrValue::Double(value));
    }

    fn record_error(
        &mut self,
        field: &tracing::field::Field,
        value: &(dyn std::error::Error + 'static),
    ) {
        self.capture(field, AttrValue::Str(format!("{value:?}")));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.capture(field, AttrValue::Str(format!("{value:?}")));
    }
}

/// The diagnostics `tracing` `Layer`: encodes every non-excluded event into a
/// `LogRecord` and hands it to the pipeline's [`Producer`](super::pipeline::Producer).
///
/// `on_event` only — **no span support.** Spans are a later phase's; a layer
/// that half-recorded them would be worse than one that clearly does not.
pub(crate) struct DiagnosticsLayer {
    producer: super::pipeline::Producer,
}

impl DiagnosticsLayer {
    pub(crate) fn new(producer: super::pipeline::Producer) -> Self {
        Self { producer }
    }
}

impl<S> tracing_subscriber::Layer<S> for DiagnosticsLayer
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        // Belt and braces: the layer is only ever installed behind the same
        // target filter at construction, but a future refactor that drops
        // that filter must not silently reopen the self-feeding export loop.
        let meta = event.metadata();
        if super::pipeline::excluded(meta.target()) {
            return;
        }

        let mut visitor = AttributeVisitor::default();
        event.record(&mut visitor);

        let mut attrs: Vec<(&str, Value<'_>)> = visitor
            .attrs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_proto()))
            .collect();
        attrs.push(("ruoqa.stream", Value::Str("diagnostics")));
        attrs.push(("code.target", Value::Str(meta.target())));

        let body = visitor.body.unwrap_or_default();
        let record = encode_record(
            now_unix_nanos(),
            Severity::from_level(*meta.level()),
            &body,
            &attrs,
        );
        self.producer.enqueue(record);
    }
}

// Golden-byte tests here re-derive the expected output from the same
// low-level `proto::write_*` primitives `encode_record`/`encode_request`
// call, matching `proto.rs`'s own test style. The stronger check — decoding
// through the independent `tests/common` reader — lives in
// `tests/otel_pipeline.rs`, which is the one place both sides of the wire
// format meet.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::otel::proto;

    #[test]
    fn encode_record_matches_hand_built_bytes_per_severity() {
        for (severity, number, text) in [
            (Severity::Trace, 1u32, "TRACE"),
            (Severity::Debug, 5, "DEBUG"),
            (Severity::Info, 9, "INFO"),
            (Severity::Warn, 13, "WARN"),
            (Severity::Error, 17, "ERROR"),
        ] {
            let bytes = encode_record(42, severity, "hello", &[("k", Value::Str("v"))]);

            let mut expected = Vec::new();
            proto::write_fixed64(&mut expected, 1, 42);
            proto::write_uint32(&mut expected, 2, number);
            proto::write_string(&mut expected, 3, text);
            proto::write_any_value(&mut expected, 5, &Value::Str("hello"));
            proto::write_key_value(&mut expected, 6, "k", &Value::Str("v"));
            proto::write_fixed64(&mut expected, 11, 42);

            assert_eq!(bytes, expected, "{severity:?}");
        }
    }

    #[test]
    fn from_level_maps_all_five_tracing_levels() {
        for (level, expected) in [
            (tracing::Level::TRACE, Severity::Trace),
            (tracing::Level::DEBUG, Severity::Debug),
            (tracing::Level::INFO, Severity::Info),
            (tracing::Level::WARN, Severity::Warn),
            (tracing::Level::ERROR, Severity::Error),
        ] {
            assert_eq!(Severity::from_level(level), expected, "{level:?}");
            assert_eq!(Severity::from_level(level).text(), expected.text());
        }
    }

    #[test]
    fn encode_request_matches_hand_built_bytes() {
        let r1 = encode_record(1, Severity::Info, "one", &[]);
        let r2 = encode_record(2, Severity::Error, "two", &[]);
        let resource: Vec<(&str, Value<'_>)> = vec![("service.name", Value::Str("ruoqa-mcp"))];
        let bytes = encode_request(&resource, ("ruoqa-mcp", "1.2.3"), &[r1.clone(), r2.clone()]);

        let mut expected = Vec::new();
        proto::write_message(&mut expected, 1, |resource_logs| {
            proto::write_resource(resource_logs, 1, &resource);
            proto::write_message(resource_logs, 2, |scope_logs| {
                proto::write_scope(scope_logs, 1, "ruoqa-mcp", "1.2.3");
                proto::write_bytes(scope_logs, 2, &r1);
                proto::write_bytes(scope_logs, 2, &r2);
            });
        });

        assert_eq!(bytes, expected);
    }
}
