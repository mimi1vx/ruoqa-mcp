//! Hand-rolled protobuf wire writer for the OTLP messages this crate emits.
//!
//! Field numbers are read off `opentelemetry-proto v1.11.0`
//! (<https://github.com/open-telemetry/opentelemetry-proto/tree/v1.11.0>).
//! Later phases must read their field numbers from the same tag.

const WIRE_VARINT: u32 = 0;
const WIRE_FIXED64: u32 = 1;
const WIRE_LEN: u32 = 2;
const WIRE_FIXED32: u32 = 5;

fn write_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(byte);
            break;
        }
        buf.push(byte | 0x80);
    }
}

fn write_tag(buf: &mut Vec<u8>, field: u32, wire: u32) {
    write_varint(buf, u64::from((field << 3) | wire));
}

#[allow(
    dead_code,
    reason = "AnyValue::Bool has no caller before phase F (metrics)"
)]
fn write_bool(buf: &mut Vec<u8>, field: u32, v: bool) {
    if v {
        write_tag(buf, field, WIRE_VARINT);
        write_varint(buf, 1);
    }
}

pub(super) fn write_uint32(buf: &mut Vec<u8>, field: u32, v: u32) {
    if v != 0 {
        write_tag(buf, field, WIRE_VARINT);
        write_varint(buf, u64::from(v));
    }
}

/// `int64` is plain varint, not zigzag — `sint64` is the zigzag variant and
/// OTLP does not use it. A negative value two's-complement-reinterprets as
/// `u64`, producing the correct ten-byte varint.
#[allow(
    dead_code,
    reason = "no integer attribute is emitted before phase D's seq-style attrs"
)]
fn write_int64(buf: &mut Vec<u8>, field: u32, v: i64) {
    if v != 0 {
        write_tag(buf, field, WIRE_VARINT);
        write_varint(buf, v.cast_unsigned());
    }
}

#[allow(dead_code, reason = "LogRecord.flags and span ids arrive in phase E")]
fn write_fixed32(buf: &mut Vec<u8>, field: u32, v: u32) {
    if v != 0 {
        write_tag(buf, field, WIRE_FIXED32);
        buf.extend_from_slice(&v.to_le_bytes());
    }
}

pub(super) fn write_fixed64(buf: &mut Vec<u8>, field: u32, v: u64) {
    if v != 0 {
        write_tag(buf, field, WIRE_FIXED64);
        buf.extend_from_slice(&v.to_le_bytes());
    }
}

#[allow(
    dead_code,
    reason = "Histogram/gauge data points have no caller before phase F"
)]
fn write_double(buf: &mut Vec<u8>, field: u32, v: f64) {
    if v != 0.0 {
        write_tag(buf, field, WIRE_FIXED64);
        buf.extend_from_slice(&v.to_bits().to_le_bytes());
    }
}

pub(super) fn write_string(buf: &mut Vec<u8>, field: u32, v: &str) {
    if !v.is_empty() {
        write_bytes(buf, field, v.as_bytes());
    }
}

pub(super) fn write_bytes(buf: &mut Vec<u8>, field: u32, v: &[u8]) {
    if !v.is_empty() {
        write_tag(buf, field, WIRE_LEN);
        write_varint(buf, v.len() as u64);
        buf.extend_from_slice(v);
    }
}

#[allow(
    dead_code,
    reason = "Histogram bucket counts have no caller before phase F"
)]
fn write_packed_fixed64(buf: &mut Vec<u8>, field: u32, values: &[u64]) {
    if values.is_empty() {
        return;
    }
    write_tag(buf, field, WIRE_LEN);
    write_varint(buf, (values.len() * 8) as u64);
    for v in values {
        buf.extend_from_slice(&v.to_le_bytes());
    }
}

#[allow(
    dead_code,
    reason = "Histogram explicit bounds have no caller before phase F"
)]
fn write_packed_double(buf: &mut Vec<u8>, field: u32, values: &[f64]) {
    if values.is_empty() {
        return;
    }
    write_tag(buf, field, WIRE_LEN);
    write_varint(buf, (values.len() * 8) as u64);
    for v in values {
        buf.extend_from_slice(&v.to_bits().to_le_bytes());
    }
}

/// Writes a length-delimited submessage: tag, then `body`'s output with its
/// length varint back-filled via `splice`. `body` writes directly into `buf`
/// (no scratch allocation); `splice` performs one memmove of the child body
/// to make room for the length prefix. Nesting in this crate is at most four
/// deep (`LogsData -> ResourceLogs -> ScopeLogs -> LogRecord`), so the total
/// shifting stays a small constant factor over the payload — do not
/// "optimise" this into a scratch-`Vec` scheme for that depth.
pub(super) fn write_message(buf: &mut Vec<u8>, field: u32, body: impl FnOnce(&mut Vec<u8>)) {
    write_tag(buf, field, WIRE_LEN);
    let start = buf.len();
    body(buf);
    let len = buf.len() - start;
    let mut len_bytes = Vec::new();
    write_varint(&mut len_bytes, len as u64);
    buf.splice(start..start, len_bytes);
}

/// The `AnyValue` subset this crate emits. Arrays, kvlists and bytes are
/// deliberately absent: every attribute we produce is a scalar.
pub(crate) enum Value<'a> {
    Str(&'a str),
    #[allow(
        dead_code,
        reason = "no integer attribute is emitted before phase D's seq-style attrs"
    )]
    Int(i64),
    #[allow(
        dead_code,
        reason = "no bool attribute has a caller before phase F (metrics)"
    )]
    Bool(bool),
    #[allow(
        dead_code,
        reason = "no double attribute has a caller before phase F (metrics)"
    )]
    Double(f64),
}

/// `AnyValue`: a oneof over field numbers 1 (`string_value`), 2
/// (`bool_value`), 3 (`int_value`) and 4 (`double_value`).
pub(super) fn write_any_value(buf: &mut Vec<u8>, field: u32, v: &Value<'_>) {
    write_message(buf, field, |b| match *v {
        Value::Str(s) => write_string(b, 1, s),
        Value::Bool(bool_v) => write_bool(b, 2, bool_v),
        Value::Int(i) => write_int64(b, 3, i),
        Value::Double(d) => write_double(b, 4, d),
    });
}

/// `KeyValue`: `key` at field 1, `value` at field 2.
pub(super) fn write_key_value(buf: &mut Vec<u8>, field: u32, key: &str, v: &Value<'_>) {
    write_message(buf, field, |b| {
        write_string(b, 1, key);
        write_any_value(b, 2, v);
    });
}

pub(super) fn write_attributes(buf: &mut Vec<u8>, field: u32, attrs: &[(&str, Value<'_>)]) {
    for (key, value) in attrs {
        write_key_value(buf, field, key, value);
    }
}

/// `Resource`: `attributes` at field 1. `dropped_attributes_count` (field 2)
/// is never written — we drop nothing, and proto3 omits zero-valued scalars.
pub(super) fn write_resource(buf: &mut Vec<u8>, field: u32, attrs: &[(&str, Value<'_>)]) {
    write_message(buf, field, |b| write_attributes(b, 1, attrs));
}

/// `InstrumentationScope`: `name` at field 1, `version` at field 2.
pub(super) fn write_scope(buf: &mut Vec<u8>, field: u32, name: &str, version: &str) {
    write_message(buf, field, |b| {
        write_string(b, 1, name);
        write_string(b, 2, version);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_boundaries() {
        let cases: &[(u64, &[u8])] = &[
            (0, &[0x00]),
            (1, &[0x01]),
            (127, &[0x7f]),
            (128, &[0x80, 0x01]),
            (300, &[0xac, 0x02]),
            (
                u64::MAX,
                &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01],
            ),
        ];
        for &(v, expected) in cases {
            let mut buf = Vec::new();
            write_varint(&mut buf, v);
            assert_eq!(buf, expected, "varint({v})");
        }
    }

    #[test]
    fn int64_negative_is_ten_byte_varint() {
        let mut buf = Vec::new();
        write_int64(&mut buf, 1, -1);
        // tag(field=1, VARINT) = 0x08, then ten 0xff-pattern varint bytes for u64::MAX.
        assert_eq!(buf[0], 0x08);
        assert_eq!(buf.len(), 1 + 10);
        assert_eq!(buf[buf.len() - 1], 0x01);
    }

    #[test]
    fn message_with_short_body_has_single_byte_length() {
        let mut buf = Vec::new();
        write_message(&mut buf, 1, |b| write_string(b, 1, "hi"));
        // tag(field=1, LEN)=0x0a, length=4, then inner tag+len+"hi".
        assert_eq!(buf, vec![0x0a, 0x04, 0x0a, 0x02, b'h', b'i']);
    }

    #[test]
    fn message_with_long_body_has_two_byte_length_and_unshifted_body() {
        let body_str = "a".repeat(200);
        let mut expected_inner = Vec::new();
        write_string(&mut expected_inner, 1, &body_str);

        let mut buf = Vec::new();
        write_message(&mut buf, 1, |b| write_string(b, 1, &body_str));

        assert_eq!(buf[0], 0x0a); // tag(field=1, LEN)
        let mut len_bytes = Vec::new();
        write_varint(&mut len_bytes, expected_inner.len() as u64);
        assert_eq!(len_bytes.len(), 2, "expected a two-byte length varint");
        assert_eq!(&buf[1..=len_bytes.len()], len_bytes.as_slice());
        assert_eq!(&buf[1 + len_bytes.len()..], expected_inner.as_slice());
    }

    #[test]
    fn packed_fixed64_three_values() {
        let mut buf = Vec::new();
        write_packed_fixed64(&mut buf, 3, &[1, 2, 3]);
        let mut expected = vec![0x1a, 0x18]; // tag(field=3, LEN), length=24
        for v in [1u64, 2, 3] {
            expected.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(buf, expected);
    }

    #[test]
    fn packed_fixed64_empty_writes_nothing() {
        let mut buf = Vec::new();
        write_packed_fixed64(&mut buf, 3, &[]);
        assert!(buf.is_empty());
    }

    #[test]
    fn fixed32_golden() {
        let mut buf = Vec::new();
        write_fixed32(&mut buf, 1, 0x0102_0304);
        assert_eq!(buf, vec![0x0d, 0x04, 0x03, 0x02, 0x01]);
    }

    #[test]
    fn fixed64_golden() {
        let mut buf = Vec::new();
        write_fixed64(&mut buf, 1, 0x0102_0304_0506_0708);
        assert_eq!(
            buf,
            vec![0x09, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]
        );
    }

    #[test]
    fn double_golden() {
        let mut buf = Vec::new();
        write_double(&mut buf, 1, 1.5);
        let mut expected = vec![0x09];
        expected.extend_from_slice(&1.5f64.to_bits().to_le_bytes());
        assert_eq!(buf, expected);
    }

    #[test]
    fn string_golden() {
        let mut buf = Vec::new();
        write_string(&mut buf, 1, "hi");
        assert_eq!(buf, vec![0x0a, 0x02, b'h', b'i']);
    }

    #[test]
    fn bytes_golden() {
        let mut buf = Vec::new();
        write_bytes(&mut buf, 1, &[0xde, 0xad]);
        assert_eq!(buf, vec![0x0a, 0x02, 0xde, 0xad]);
    }

    #[test]
    fn key_value_str() {
        let mut buf = Vec::new();
        write_key_value(&mut buf, 1, "k", &Value::Str("v"));
        // KeyValue{ key="k", value=AnyValue{string_value="v"} }
        let expected = vec![
            0x0a, 0x08, // tag(field=1, LEN), len=8
            0x0a, 0x01, b'k', // key: tag(1,LEN) len=1 "k"
            0x12, 0x03, // value: tag(field=2, LEN) len=3
            0x0a, 0x01, b'v', // string_value: tag(1,LEN) len=1 "v"
        ];
        assert_eq!(buf, expected);
    }

    #[test]
    fn key_value_bool() {
        let mut buf = Vec::new();
        write_key_value(&mut buf, 1, "k", &Value::Bool(true));
        let expected = vec![
            0x0a, 0x07, // tag(1,LEN) len=7
            0x0a, 0x01, b'k', // key
            0x12, 0x02, // value: tag(2,LEN) len=2
            0x10, 0x01, // bool_value: tag(field=2, VARINT), true
        ];
        assert_eq!(buf, expected);
    }

    #[test]
    fn key_value_int() {
        let mut buf = Vec::new();
        write_key_value(&mut buf, 1, "k", &Value::Int(-1));
        let expected = vec![
            0x0a, 0x10, // tag(1,LEN) len=16
            0x0a, 0x01, b'k', // key
            0x12, 0x0b, // value: tag(2,LEN) len=11
            0x18, // int_value: tag(field=3, VARINT)
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01, // -1 as u64 varint
        ];
        assert_eq!(buf, expected);
    }

    #[test]
    fn key_value_double() {
        let mut buf = Vec::new();
        write_key_value(&mut buf, 1, "k", &Value::Double(1.5));
        let mut expected = vec![
            0x0a, 0x0e, // tag(1,LEN) len=14
            0x0a, 0x01, b'k', // key
            0x12, 0x09, // value: tag(2,LEN) len=9
            0x21, // double_value: tag(field=4, FIXED64)
        ];
        expected.extend_from_slice(&1.5f64.to_bits().to_le_bytes());
        assert_eq!(buf, expected);
    }

    #[test]
    fn resource_two_attributes() {
        let mut buf = Vec::new();
        let attrs: Vec<(&str, Value<'_>)> = vec![
            ("service.name", Value::Str("ruoqa-mcp")),
            ("service.version", Value::Str("1.2.3")),
        ];
        write_resource(&mut buf, 1, &attrs);

        let mut inner = Vec::new();
        write_key_value(&mut inner, 1, "service.name", &Value::Str("ruoqa-mcp"));
        write_key_value(&mut inner, 1, "service.version", &Value::Str("1.2.3"));

        let mut expected = vec![0x0a];
        write_varint(&mut expected, inner.len() as u64);
        expected.extend_from_slice(&inner);
        assert_eq!(buf, expected);
    }

    #[test]
    fn scope_with_version() {
        let mut buf = Vec::new();
        write_scope(&mut buf, 1, "ruoqa-mcp", "1.2.3");
        let expected = vec![
            0x0a, 0x12, // tag(1,LEN) len=18
            0x0a, 0x09, b'r', b'u', b'o', b'q', b'a', b'-', b'm', b'c', b'p', // name
            0x12, 0x05, b'1', b'.', b'2', b'.', b'3', // version
        ];
        assert_eq!(buf, expected);
    }

    #[test]
    fn scope_without_version() {
        let mut buf = Vec::new();
        write_scope(&mut buf, 1, "ruoqa-mcp", "");
        let expected = vec![
            0x0a, 0x0b, // tag(1,LEN) len=11
            0x0a, 0x09, b'r', b'u', b'o', b'q', b'a', b'-', b'm', b'c', b'p', // name
        ];
        assert_eq!(buf, expected);
    }
}
