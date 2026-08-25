//! `Span` / `ExportTraceServiceRequest` encoding, span/trace id generation,
//! and W3C `traceparent` parsing.

use super::env::Sampler;
use super::proto::{self, Value};

/// `Status.code`'s `STATUS_CODE_ERROR` value; `STATUS_CODE_OK`/`_UNSET` have
/// no caller here — a success span omits `Status` entirely (proto3's
/// zero-omission rule already makes `STATUS_CODE_UNSET` and absence mean the
/// same thing on the wire).
const STATUS_CODE_ERROR: u32 = 2;

/// `SpanKind`. `Unspecified`/`Producer`/`Consumer` have no caller in this
/// crate, so are omitted rather than left unconstructible.
#[derive(Debug, Clone, Copy)]
pub(crate) enum SpanKind {
    Internal = 1,
    Server = 2,
    Client = 3,
}

/// One span's identity: a 16-byte trace id, an 8-byte span id, and the W3C
/// `sampled` bit. Every `SpanCtx` this crate constructs is already a
/// decision to record a span, so `sampled` is `true` in practice; the field
/// stays explicit because it is what [`encode_span`] writes to `flags`, and
/// [`from_raw`](SpanCtx::from_raw) reconstructs one from stored hex without
/// re-deciding anything.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SpanCtx {
    trace_id: [u8; 16],
    span_id: [u8; 8],
    sampled: bool,
}

/// Draws `N` random, non-all-zero bytes. `getrandom::fill` failing, or two
/// consecutive all-zero draws (astronomically unlikely; the spec defines an
/// all-zero id as invalid), both return `None` rather than panicking or
/// shipping an invalid id — the caller then emits no span for that call.
fn random_nonzero<const N: usize>() -> Option<[u8; N]> {
    let mut buf = [0u8; N];
    for _ in 0..2 {
        if getrandom::fill(&mut buf).is_err() {
            return None;
        }
        if buf != [0u8; N] {
            return Some(buf);
        }
    }
    None
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Decodes a lowercase (or uppercase) hex string of exactly `2*N` characters
/// into `N` bytes. Shared by [`parse_traceparent`] and the audit OTLP
/// bridge, which re-decodes a `Record`'s already-hex-encoded `trace`/`span`
/// fields to fill `LogRecord.trace_id`/`span_id`.
pub(crate) fn decode_hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for i in 0..N {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

impl SpanCtx {
    /// A fresh root context: random trace id, random span id, sampled.
    pub(crate) fn new_root() -> Option<Self> {
        Some(Self {
            trace_id: random_nonzero::<16>()?,
            span_id: random_nonzero::<8>()?,
            sampled: true,
        })
    }

    /// A fresh span id under an existing (possibly remote) `trace_id`.
    pub(crate) fn with_trace_id(trace_id: [u8; 16], sampled: bool) -> Option<Self> {
        Some(Self {
            trace_id,
            span_id: random_nonzero::<8>()?,
            sampled,
        })
    }

    /// Reconstructs a context from already-decided bytes — no randomness,
    /// no sampling decision. Used to rebuild a `SpanCtx` from an audit
    /// `Record`'s stored `trace`/`span` hex fields.
    pub(crate) fn from_raw(trace_id: [u8; 16], span_id: [u8; 8], sampled: bool) -> Self {
        Self {
            trace_id,
            span_id,
            sampled,
        }
    }

    /// A child span in the same trace: same `trace_id`, fresh `span_id`.
    pub(crate) fn child(&self) -> Option<Self> {
        Self::with_trace_id(self.trace_id, self.sampled)
    }

    pub(crate) fn trace_id(&self) -> [u8; 16] {
        self.trace_id
    }

    pub(crate) fn span_id(&self) -> [u8; 8] {
        self.span_id
    }

    pub(crate) fn sampled(&self) -> bool {
        self.sampled
    }

    pub(crate) fn trace_hex(&self) -> String {
        hex_encode(&self.trace_id)
    }

    pub(crate) fn span_hex(&self) -> String {
        hex_encode(&self.span_id)
    }
}

/// A parsed, validated inbound W3C `traceparent`: the remote trace and span
/// id to adopt, and the sampled bit from the trace-flags byte.
pub(crate) struct TraceParent {
    pub(crate) trace_id: [u8; 16],
    pub(crate) span_id: [u8; 8],
    pub(crate) sampled: bool,
}

/// Parses `00-<32 hex>-<16 hex>-<2 hex>` strictly: version `00` only, and an
/// all-zero trace or span id is rejected (the spec defines both as
/// invalid). Anything else — wrong shape, wrong version, bad hex, all-zero
/// ids — is `None`: a malformed value is caller-supplied text, never logged,
/// and simply leaves no parent.
pub(crate) fn parse_traceparent(value: &str) -> Option<TraceParent> {
    let mut parts = value.split('-');
    let version = parts.next()?;
    let trace_id_hex = parts.next()?;
    let span_id_hex = parts.next()?;
    let flags_hex = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if version != "00" || flags_hex.len() != 2 {
        return None;
    }
    let trace_id = decode_hex::<16>(trace_id_hex)?;
    let span_id = decode_hex::<8>(span_id_hex)?;
    let flags = decode_hex::<1>(flags_hex)?[0];
    if trace_id == [0u8; 16] || span_id == [0u8; 8] {
        return None;
    }
    Some(TraceParent {
        trace_id,
        span_id,
        sampled: flags & 0x01 != 0,
    })
}

fn should_sample(sampler: Sampler, parent: &TraceParent) -> bool {
    match sampler {
        Sampler::AlwaysOn => true,
        Sampler::ParentBasedAlwaysOn => parent.sampled,
    }
}

/// The span context (and, for an adopted trace, the remote parent span id)
/// for one tool call, per `sampler` and an optional inbound `traceparent`.
/// `None` means the call must not be sampled: it still runs, but no span is
/// encoded and no `CURRENT_SPAN` scope is entered. A root call (no
/// `traceparent`, or a malformed one) is always sampled under both
/// supported samplers — "parent-based" only ever defers to an actual
/// parent.
pub(crate) fn start_call_span(
    sampler: Sampler,
    traceparent: Option<&str>,
) -> Option<(SpanCtx, Option<[u8; 8]>)> {
    match traceparent.and_then(parse_traceparent) {
        Some(parent) => {
            if !should_sample(sampler, &parent) {
                return None;
            }
            let ctx = SpanCtx::with_trace_id(parent.trace_id, true)?;
            Some((ctx, Some(parent.span_id)))
        }
        None => SpanCtx::new_root().map(|ctx| (ctx, None)),
    }
}

/// Encodes one `Span`'s own fields — **not** wrapped in an outer LEN tag,
/// matching `logs::encode_record`'s convention: the pipeline splices this
/// straight into a `ScopeSpans.spans` field via `proto::write_bytes`.
///
/// `error_message` is `Some(kind)` for a failed call (`Status` `ERROR`,
/// `message` = the failure's `kind`) and `None` on success — proto3's
/// zero-omission already makes an absent `Status` mean `STATUS_CODE_UNSET`,
/// so a success span writes no `Status` at all rather than an explicit
/// unset one.
#[allow(clippy::too_many_arguments, reason = "mirrors the Span wire shape")]
pub(crate) fn encode_span(
    ctx: SpanCtx,
    parent_span_id: Option<[u8; 8]>,
    name: &str,
    kind: SpanKind,
    start_nanos: u64,
    end_nanos: u64,
    attrs: &[(&str, Value<'_>)],
    error_message: Option<&str>,
) -> Vec<u8> {
    let mut buf = Vec::new();
    proto::write_bytes(&mut buf, 1, &ctx.trace_id);
    proto::write_bytes(&mut buf, 2, &ctx.span_id);
    if let Some(parent) = parent_span_id {
        proto::write_bytes(&mut buf, 4, &parent);
    }
    proto::write_string(&mut buf, 5, name);
    proto::write_uint32(&mut buf, 6, kind as u32);
    proto::write_fixed64(&mut buf, 7, start_nanos);
    proto::write_fixed64(&mut buf, 8, end_nanos);
    proto::write_attributes(&mut buf, 9, attrs);
    if let Some(message) = error_message {
        proto::write_message(&mut buf, 15, |b| {
            proto::write_string(b, 2, message);
            proto::write_uint32(b, 3, STATUS_CODE_ERROR);
        });
    }
    // Field 16: added to the spec after `status` (15), out of declaration
    // order — the one field number in this module most likely to be
    // guessed wrong.
    proto::write_fixed32(&mut buf, 16, u32::from(ctx.sampled));
    buf
}

/// Wraps a batch of already-[`encode_span`]d bodies into a full
/// `ExportTraceServiceRequest`, one `Resource` and one
/// `InstrumentationScope`. Field numbers are identical to
/// `logs::encode_request`'s — `resource_spans`/`resource`/`scope_spans` line
/// up with `resource_logs`/`resource`/`scope_logs` one for one — but that is
/// a coincidence of the two message trees, not a contract, which is why this
/// stays its own function rather than a shared generic one.
pub(crate) fn encode_request(
    resource: &[(&str, Value<'_>)],
    scope: (&str, &str),
    spans: &[Vec<u8>],
) -> Vec<u8> {
    let mut buf = Vec::new();
    proto::write_message(&mut buf, 1, |resource_spans| {
        proto::write_resource(resource_spans, 1, resource);
        proto::write_message(resource_spans, 2, |scope_spans| {
            proto::write_scope(scope_spans, 1, scope.0, scope.1);
            for span in spans {
                proto::write_bytes(scope_spans, 2, span);
            }
        });
    });
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_root_yields_distinct_nonzero_ids() {
        let a = SpanCtx::new_root().unwrap();
        let b = SpanCtx::new_root().unwrap();
        assert_ne!(a.trace_id, [0u8; 16]);
        assert_ne!(a.span_id, [0u8; 8]);
        assert_ne!(
            a.trace_id, b.trace_id,
            "two roots must not share a trace id"
        );
    }

    #[test]
    fn child_keeps_trace_id_and_changes_span_id() {
        let root = SpanCtx::new_root().unwrap();
        let child = root.child().unwrap();
        assert_eq!(child.trace_id, root.trace_id);
        assert_ne!(child.span_id, root.span_id);
    }

    #[test]
    fn hex_round_trips_through_decode_hex() {
        let ctx = SpanCtx::new_root().unwrap();
        let trace_id = decode_hex::<16>(&ctx.trace_hex()).unwrap();
        let span_id = decode_hex::<8>(&ctx.span_hex()).unwrap();
        assert_eq!(trace_id, ctx.trace_id);
        assert_eq!(span_id, ctx.span_id);
    }

    #[test]
    fn decode_hex_rejects_wrong_length_and_bad_digits() {
        assert_eq!(decode_hex::<8>("00"), None);
        assert_eq!(decode_hex::<8>("zzzzzzzzzzzzzzzz"), None);
    }

    const VALID: &str = "00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba902b7-01";

    #[test]
    fn parse_traceparent_accepts_a_valid_sampled_value() {
        let parent = parse_traceparent(VALID).unwrap();
        assert_eq!(
            parent.trace_id,
            decode_hex::<16>("0af7651916cd43dd8448eb211c80319c").unwrap()
        );
        assert_eq!(parent.span_id, decode_hex::<8>("00f067aa0ba902b7").unwrap());
        assert!(parent.sampled);
    }

    #[test]
    fn parse_traceparent_reads_the_unsampled_flag() {
        let value = "00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba902b7-00";
        assert!(!parse_traceparent(value).unwrap().sampled);
    }

    #[test]
    fn parse_traceparent_rejects_non_00_version() {
        let value = "01-0af7651916cd43dd8448eb211c80319c-00f067aa0ba902b7-01";
        assert!(parse_traceparent(value).is_none());
    }

    #[test]
    fn parse_traceparent_rejects_all_zero_trace_id() {
        let value = "00-00000000000000000000000000000000-00f067aa0ba902b7-01";
        assert!(parse_traceparent(value).is_none());
    }

    #[test]
    fn parse_traceparent_rejects_all_zero_span_id() {
        let value = "00-0af7651916cd43dd8448eb211c80319c-0000000000000000-01";
        assert!(parse_traceparent(value).is_none());
    }

    #[test]
    fn parse_traceparent_rejects_malformed_shapes() {
        for bad in [
            "",
            "not-a-traceparent",
            "00-tooshort-00f067aa0ba902b7-01",
            "00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba902b7",
            "00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba902b7-01-extra",
        ] {
            assert!(parse_traceparent(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn start_call_span_roots_when_no_traceparent() {
        let (ctx, parent) = start_call_span(Sampler::AlwaysOn, None).unwrap();
        assert!(parent.is_none());
        assert!(ctx.sampled());
    }

    #[test]
    fn start_call_span_adopts_a_valid_traceparent() {
        let (ctx, parent) = start_call_span(Sampler::AlwaysOn, Some(VALID)).unwrap();
        assert_eq!(
            ctx.trace_hex(),
            "0af7651916cd43dd8448eb211c80319c",
            "must adopt the remote trace id"
        );
        assert_eq!(parent, Some(decode_hex::<8>("00f067aa0ba902b7").unwrap()));
    }

    #[test]
    fn start_call_span_always_on_samples_even_an_unsampled_parent() {
        let value = "00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba902b7-00";
        assert!(start_call_span(Sampler::AlwaysOn, Some(value)).is_some());
    }

    #[test]
    fn start_call_span_parentbased_respects_an_unsampled_parent() {
        let value = "00-0af7651916cd43dd8448eb211c80319c-00f067aa0ba902b7-00";
        assert!(start_call_span(Sampler::ParentBasedAlwaysOn, Some(value)).is_none());
    }

    #[test]
    fn start_call_span_parentbased_still_roots_with_no_parent() {
        assert!(start_call_span(Sampler::ParentBasedAlwaysOn, None).is_some());
    }

    #[test]
    fn start_call_span_malformed_traceparent_falls_back_to_root() {
        let (ctx, parent) = start_call_span(Sampler::AlwaysOn, Some("garbage")).unwrap();
        assert!(parent.is_none());
        assert!(ctx.sampled());
    }

    #[test]
    fn encode_span_root_has_no_parent_field() {
        let ctx = SpanCtx::from_raw([1u8; 16], [2u8; 8], true);
        let bytes = encode_span(
            ctx,
            None,
            "mcp.tool/get_job",
            SpanKind::Server,
            10,
            20,
            &[],
            None,
        );

        let mut expected = Vec::new();
        proto::write_bytes(&mut expected, 1, &[1u8; 16]);
        proto::write_bytes(&mut expected, 2, &[2u8; 8]);
        proto::write_string(&mut expected, 5, "mcp.tool/get_job");
        proto::write_uint32(&mut expected, 6, SpanKind::Server as u32);
        proto::write_fixed64(&mut expected, 7, 10);
        proto::write_fixed64(&mut expected, 8, 20);
        proto::write_fixed32(&mut expected, 16, 1);

        assert_eq!(bytes, expected);
    }

    #[test]
    fn encode_span_child_carries_parent_span_id() {
        let ctx = SpanCtx::from_raw([1u8; 16], [2u8; 8], true);
        let bytes = encode_span(
            ctx,
            Some([9u8; 8]),
            "openqa.request",
            SpanKind::Client,
            0,
            0,
            &[],
            None,
        );
        let mut expected = Vec::new();
        proto::write_bytes(&mut expected, 1, &[1u8; 16]);
        proto::write_bytes(&mut expected, 2, &[2u8; 8]);
        proto::write_bytes(&mut expected, 4, &[9u8; 8]);
        proto::write_string(&mut expected, 5, "openqa.request");
        proto::write_uint32(&mut expected, 6, SpanKind::Client as u32);
        proto::write_fixed32(&mut expected, 16, 1);
        assert_eq!(bytes, expected);
    }

    #[test]
    fn encode_span_success_writes_no_status() {
        let ctx = SpanCtx::from_raw([1u8; 16], [2u8; 8], true);
        let bytes = encode_span(ctx, None, "n", SpanKind::Internal, 0, 0, &[], None);
        // Field 15 (LEN) tag byte is `(15 << 3) | 2 = 122 = 0x7a`.
        assert!(!bytes.contains(&0x7a), "success span must write no Status");
    }

    #[test]
    fn encode_span_error_writes_status_error() {
        let ctx = SpanCtx::from_raw([1u8; 16], [2u8; 8], true);
        let bytes = encode_span(
            ctx,
            None,
            "n",
            SpanKind::Internal,
            0,
            0,
            &[],
            Some("not_found"),
        );
        let mut expected_status = Vec::new();
        proto::write_string(&mut expected_status, 2, "not_found");
        proto::write_uint32(&mut expected_status, 3, STATUS_CODE_ERROR);
        let mut expected = Vec::new();
        proto::write_message(&mut expected, 15, |b| b.extend_from_slice(&expected_status));
        // The Status submessage bytes must appear verbatim in the encoded span.
        assert!(
            bytes
                .windows(expected.len())
                .any(|w| w == expected.as_slice())
        );
    }

    #[test]
    fn encode_span_unsampled_writes_no_flags() {
        let unsampled = encode_span(
            SpanCtx::from_raw([1u8; 16], [2u8; 8], false),
            None,
            "n",
            SpanKind::Internal,
            0,
            0,
            &[],
            None,
        );
        let sampled = encode_span(
            SpanCtx::from_raw([1u8; 16], [2u8; 8], true),
            None,
            "n",
            SpanKind::Internal,
            0,
            0,
            &[],
            None,
        );
        // Field 16 (FIXED32) tag is `(16 << 3) | 5 = 133`, a two-byte
        // varint, so the flags field is 6 bytes (tag + 4-byte value) when
        // present and absent entirely when not.
        assert_eq!(sampled.len(), unsampled.len() + 6);
    }

    #[test]
    fn encode_request_matches_hand_built_bytes() {
        let ctx = SpanCtx::from_raw([1u8; 16], [2u8; 8], true);
        let span = encode_span(ctx, None, "n", SpanKind::Internal, 1, 2, &[], None);
        let resource: Vec<(&str, Value<'_>)> = vec![("service.name", Value::Str("ruoqa-mcp"))];
        let bytes = encode_request(
            &resource,
            ("ruoqa-mcp", "1.2.3"),
            std::slice::from_ref(&span),
        );

        let mut expected = Vec::new();
        proto::write_message(&mut expected, 1, |resource_spans| {
            proto::write_resource(resource_spans, 1, &resource);
            proto::write_message(resource_spans, 2, |scope_spans| {
                proto::write_scope(scope_spans, 1, "ruoqa-mcp", "1.2.3");
                proto::write_bytes(scope_spans, 2, &span);
            });
        });
        assert_eq!(bytes, expected);
    }
}
