//! `LogRecord` / `ExportLogsServiceRequest` encoding.
//!
//! Record encoding only in this phase: the `tracing` `Layer`, the full
//! severity mapping table, the audit stream and tool-path instrumentation
//! all arrive in phase D. `Severity` here is the three-value subset the
//! startup probe needs.

use super::proto::{self, Value};

/// `SeverityNumber`, the three values this phase needs. The full 24-value
/// table is phase D's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Severity {
    Info = 9,
    #[allow(
        dead_code,
        reason = "phase D's tracing severity mapping constructs this"
    )]
    Warn = 13,
    #[allow(
        dead_code,
        reason = "phase D's tracing severity mapping constructs this"
    )]
    Error = 17,
}

impl Severity {
    fn text(self) -> &'static str {
        match self {
            Severity::Info => "INFO",
            Severity::Warn => "WARN",
            Severity::Error => "ERROR",
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
            (Severity::Info, 9u32, "INFO"),
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
