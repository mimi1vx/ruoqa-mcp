//! Minimal length-delimited protobuf decoder so integration tests can assert
//! on decoded structure rather than opaque bytes. Deliberately dumb: never
//! panics on malformed input, rejects deprecated groups (wire types 3, 4)
//! and the unused wire types 6 and 7.
//!
//! Compiled into every test target that declares `mod common;`, so a helper
//! a given target does not call would warn `dead_code` under `-D warnings`
//! without this.
#![allow(dead_code)]

const WIRE_VARINT: u32 = 0;
const WIRE_FIXED64: u32 = 1;
const WIRE_LEN: u32 = 2;
const WIRE_FIXED32: u32 = 5;

#[derive(Debug, Clone, PartialEq)]
pub enum Field {
    Varint(u64),
    Fixed64(u64),
    Fixed32(u32),
    Len(Vec<u8>),
}

#[derive(Debug)]
pub struct Message(Vec<(u32, Field)>);

fn read_varint(bytes: &[u8], pos: &mut usize) -> Result<u64, String> {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    loop {
        let Some(&byte) = bytes.get(*pos) else {
            return Err("truncated varint".to_string());
        };
        if shift >= 64 {
            return Err("varint longer than 10 bytes".to_string());
        }
        *pos += 1;
        result |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
    }
}

fn read_fixed<'a>(bytes: &'a [u8], pos: &mut usize, width: usize) -> Result<&'a [u8], String> {
    let end = pos
        .checked_add(width)
        .ok_or_else(|| "length overflow".to_string())?;
    if end > bytes.len() {
        return Err("truncated fixed-width field".to_string());
    }
    let slice = &bytes[*pos..end];
    *pos = end;
    Ok(slice)
}

impl Message {
    /// Parses a flat sequence of protobuf fields. Never panics: truncated
    /// input, a `LEN` length running past the buffer, and wire types 3, 4
    /// (deprecated groups) or 6, 7 (unused) are all `Err`.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let mut fields = Vec::new();
        let mut pos = 0usize;
        while pos < bytes.len() {
            let tag = read_varint(bytes, &mut pos)?;
            let field = u32::try_from(tag >> 3).map_err(|_| "field number overflow".to_string())?;
            let wire = u32::try_from(tag & 0x7).expect("3-bit mask fits in u32");
            let value = if wire == WIRE_VARINT {
                Field::Varint(read_varint(bytes, &mut pos)?)
            } else if wire == WIRE_FIXED64 {
                let raw: [u8; 8] = read_fixed(bytes, &mut pos, 8)?
                    .try_into()
                    .expect("read_fixed returns exactly `width` bytes");
                Field::Fixed64(u64::from_le_bytes(raw))
            } else if wire == WIRE_FIXED32 {
                let raw: [u8; 4] = read_fixed(bytes, &mut pos, 4)?
                    .try_into()
                    .expect("read_fixed returns exactly `width` bytes");
                Field::Fixed32(u32::from_le_bytes(raw))
            } else if wire == WIRE_LEN {
                let len = read_varint(bytes, &mut pos)?;
                let len = usize::try_from(len).map_err(|_| "length overflow".to_string())?;
                let end = pos
                    .checked_add(len)
                    .ok_or_else(|| "length overflow".to_string())?;
                if end > bytes.len() {
                    return Err("length runs past buffer".to_string());
                }
                let data = bytes[pos..end].to_vec();
                pos = end;
                Field::Len(data)
            } else {
                return Err(format!("unsupported wire type {wire}"));
            };
            fields.push((field, value));
        }
        Ok(Message(fields))
    }

    /// The first occurrence of `field`, in encounter order.
    pub fn get(&self, field: u32) -> Option<&Field> {
        self.0.iter().find(|(f, _)| *f == field).map(|(_, v)| v)
    }

    /// Every occurrence of `field`, in encounter order.
    pub fn all(&self, field: u32) -> Vec<&Field> {
        self.0
            .iter()
            .filter(|(f, _)| *f == field)
            .map(|(_, v)| v)
            .collect()
    }

    /// Parses `field`'s `Len` payload as a nested message.
    pub fn msg(&self, field: u32) -> Option<Message> {
        match self.get(field) {
            Some(Field::Len(bytes)) => Message::parse(bytes).ok(),
            _ => None,
        }
    }

    /// `field`'s `Len` payload as a UTF-8 string.
    pub fn str(&self, field: u32) -> Option<String> {
        match self.get(field) {
            Some(Field::Len(bytes)) => String::from_utf8(bytes.clone()).ok(),
            _ => None,
        }
    }

    /// `field`'s value as `u64`, from whichever numeric wire type it used.
    pub fn u64(&self, field: u32) -> Option<u64> {
        match self.get(field) {
            Some(Field::Varint(v) | Field::Fixed64(v)) => Some(*v),
            Some(Field::Fixed32(v)) => Some(u64::from(*v)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_varint() {
        // field=1, wire=VARINT, value=300 -> tag=0x08, varint=[0xac,0x02]
        let bytes = [0x08, 0xac, 0x02];
        let msg = Message::parse(&bytes).unwrap();
        assert_eq!(msg.get(1), Some(&Field::Varint(300)));
    }

    #[test]
    fn round_trips_negative_int64() {
        // field=1, wire=VARINT, value=-1 as u64 (ten 0xff-pattern bytes)
        let bytes = [
            0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01,
        ];
        let msg = Message::parse(&bytes).unwrap();
        assert_eq!(msg.get(1), Some(&Field::Varint(u64::MAX)));
    }

    #[test]
    fn round_trips_fixed32() {
        let bytes = [0x0d, 0x04, 0x03, 0x02, 0x01];
        let msg = Message::parse(&bytes).unwrap();
        assert_eq!(msg.get(1), Some(&Field::Fixed32(0x0102_0304)));
    }

    #[test]
    fn round_trips_fixed64() {
        let bytes = [0x09, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01];
        let msg = Message::parse(&bytes).unwrap();
        assert_eq!(msg.get(1), Some(&Field::Fixed64(0x0102_0304_0506_0708)));
    }

    #[test]
    fn round_trips_double() {
        let mut bytes = vec![0x09];
        bytes.extend_from_slice(&1.5f64.to_bits().to_le_bytes());
        let msg = Message::parse(&bytes).unwrap();
        assert_eq!(msg.get(1), Some(&Field::Fixed64(1.5f64.to_bits())));
    }

    #[test]
    fn round_trips_string() {
        let bytes = [0x0a, 0x02, b'h', b'i'];
        let msg = Message::parse(&bytes).unwrap();
        assert_eq!(msg.str(1), Some("hi".to_string()));
    }

    #[test]
    fn round_trips_bytes() {
        let bytes = [0x0a, 0x02, 0xde, 0xad];
        let msg = Message::parse(&bytes).unwrap();
        assert_eq!(msg.get(1), Some(&Field::Len(vec![0xde, 0xad])));
    }

    #[test]
    fn round_trips_packed_fixed64() {
        let mut bytes = vec![0x1a, 0x18]; // tag(field=3, LEN), length=24
        for v in [1u64, 2, 3] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        let msg = Message::parse(&bytes).unwrap();
        let Some(Field::Len(packed)) = msg.get(3) else {
            panic!("expected a Len field");
        };
        let (chunks, _) = packed.as_chunks::<8>();
        let values: Vec<u64> = chunks.iter().copied().map(u64::from_le_bytes).collect();
        assert_eq!(values, vec![1, 2, 3]);
    }

    #[test]
    fn round_trips_nested_message_with_multi_byte_length() {
        // field=1, LEN body: nested field=1 string of 200 'a's (from proto.rs's
        // `message_with_long_body_has_two_byte_length_and_unshifted_body`).
        let body_str = "a".repeat(200);
        let mut inner = vec![0x0a];
        inner.push(0x80 | u8::try_from(body_str.len() % 128).unwrap());
        inner.push(u8::try_from(body_str.len() / 128).unwrap());
        inner.extend_from_slice(body_str.as_bytes());

        let mut outer = vec![0x0a];
        // outer length = inner.len(), which is > 127, so two-byte varint too.
        let len = inner.len() as u64;
        outer.push(0x80 | u8::try_from(len % 128).unwrap());
        outer.push(u8::try_from(len / 128).unwrap());
        outer.extend_from_slice(&inner);

        let msg = Message::parse(&outer).unwrap();
        let nested = msg.msg(1).expect("nested message");
        assert_eq!(nested.str(1), Some(body_str));
    }

    #[test]
    fn round_trips_any_value_variants() {
        // Each literal is a full KeyValue{key="k", value=AnyValue{...}}
        // wrapped as field 1 (matching `write_key_value(&mut buf, 1, ...)`),
        // so unwrap that envelope with `.msg(1)` before reading key/value.

        // KeyValue{key="k", value=AnyValue{string_value="v"}}
        let str_kv = [0x0a, 0x08, 0x0a, 0x01, b'k', 0x12, 0x03, 0x0a, 0x01, b'v'];
        let kv = Message::parse(&str_kv).unwrap().msg(1).unwrap();
        assert_eq!(kv.str(1), Some("k".to_string()));
        let value = kv.msg(2).unwrap();
        assert_eq!(value.str(1), Some("v".to_string()));

        // KeyValue{key="k", value=AnyValue{bool_value=true}}
        let bool_kv = [0x0a, 0x07, 0x0a, 0x01, b'k', 0x12, 0x02, 0x10, 0x01];
        let kv = Message::parse(&bool_kv).unwrap().msg(1).unwrap();
        let value = kv.msg(2).unwrap();
        assert_eq!(value.get(2), Some(&Field::Varint(1)));

        // KeyValue{key="k", value=AnyValue{int_value=-1}}
        let int_kv = [
            0x0a, 0x10, 0x0a, 0x01, b'k', 0x12, 0x0b, 0x18, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0x01,
        ];
        let kv = Message::parse(&int_kv).unwrap().msg(1).unwrap();
        let value = kv.msg(2).unwrap();
        assert_eq!(value.u64(3), Some(u64::MAX));

        // KeyValue{key="k", value=AnyValue{double_value=1.5}}
        let mut double_kv = vec![0x0a, 0x0e, 0x0a, 0x01, b'k', 0x12, 0x09, 0x21];
        double_kv.extend_from_slice(&1.5f64.to_bits().to_le_bytes());
        let kv = Message::parse(&double_kv).unwrap().msg(1).unwrap();
        let value = kv.msg(2).unwrap();
        assert_eq!(value.u64(4), Some(1.5f64.to_bits()));
    }

    #[test]
    fn round_trips_resource_two_attributes() {
        // Resource{attributes=[KeyValue{"service.name","ruoqa-mcp"},
        //                      KeyValue{"service.version","1.2.3"}]}
        // wrapped as field 1, matching `write_resource(&mut buf, 1, ...)`.
        fn key_value_str(field: u32, key: &str, value: &str) -> Vec<u8> {
            let mut inner = vec![];
            let mut key_bytes = vec![0x0a, u8::try_from(key.len()).unwrap()];
            key_bytes.extend_from_slice(key.as_bytes());
            let mut value_bytes = vec![0x0a, u8::try_from(value.len()).unwrap()];
            value_bytes.extend_from_slice(value.as_bytes());
            inner.extend_from_slice(&key_bytes);
            inner.push(0x12);
            inner.push(u8::try_from(value_bytes.len()).unwrap());
            inner.extend_from_slice(&value_bytes);

            let mut kv = vec![u8::try_from((field << 3) | 2).unwrap()];
            kv.push(u8::try_from(inner.len()).unwrap());
            kv.extend_from_slice(&inner);
            kv
        }

        let attr1 = key_value_str(1, "service.name", "ruoqa-mcp");
        let attr2 = key_value_str(1, "service.version", "1.2.3");
        let mut body = attr1.clone();
        body.extend_from_slice(&attr2);

        let mut resource = vec![0x0a];
        resource.push(u8::try_from(body.len()).unwrap());
        resource.extend_from_slice(&body);

        let res = Message::parse(&resource).unwrap().msg(1).unwrap();
        let attributes = res.all(1);
        assert_eq!(attributes.len(), 2);
        let kv1 = match attributes[0] {
            Field::Len(bytes) => Message::parse(bytes).unwrap(),
            _ => panic!("expected Len field"),
        };
        let kv2 = match attributes[1] {
            Field::Len(bytes) => Message::parse(bytes).unwrap(),
            _ => panic!("expected Len field"),
        };
        assert_eq!(kv1.str(1), Some("service.name".to_string()));
        assert_eq!(kv1.msg(2).unwrap().str(1), Some("ruoqa-mcp".to_string()));
        assert_eq!(kv2.str(1), Some("service.version".to_string()));
        assert_eq!(kv2.msg(2).unwrap().str(1), Some("1.2.3".to_string()));
    }

    #[test]
    fn round_trips_scope_with_and_without_version() {
        let with_version = [
            0x0a, 0x12, 0x0a, 0x09, b'r', b'u', b'o', b'q', b'a', b'-', b'm', b'c', b'p', 0x12,
            0x05, b'1', b'.', b'2', b'.', b'3',
        ];
        let scope = Message::parse(&with_version).unwrap().msg(1).unwrap();
        assert_eq!(scope.str(1), Some("ruoqa-mcp".to_string()));
        assert_eq!(scope.str(2), Some("1.2.3".to_string()));

        let without_version = [
            0x0a, 0x0b, 0x0a, 0x09, b'r', b'u', b'o', b'q', b'a', b'-', b'm', b'c', b'p',
        ];
        let scope = Message::parse(&without_version).unwrap().msg(1).unwrap();
        assert_eq!(scope.str(1), Some("ruoqa-mcp".to_string()));
        assert_eq!(scope.str(2), None);
    }

    #[test]
    fn rejects_truncated_varint() {
        assert!(Message::parse(&[0x80]).is_err());
    }

    #[test]
    fn rejects_truncated_len_field() {
        // tag says LEN of 5 bytes but only 2 are present.
        assert!(Message::parse(&[0x0a, 0x05, b'h', b'i']).is_err());
    }

    #[test]
    fn rejects_overlong_length() {
        assert!(Message::parse(&[0x0a, 0xff, 0xff, 0xff, 0xff, 0x0f]).is_err());
    }

    #[test]
    fn rejects_deprecated_group_wire_types() {
        // wire type 3 (SGROUP) and 4 (EGROUP) on field=1.
        assert!(Message::parse(&[0x0b]).is_err());
        assert!(Message::parse(&[0x0c]).is_err());
    }

    #[test]
    fn rejects_unused_wire_types() {
        // wire type 6 and 7 on field=1.
        assert!(Message::parse(&[0x0e]).is_err());
        assert!(Message::parse(&[0x0f]).is_err());
    }

    #[test]
    fn does_not_panic_on_empty_input() {
        assert_eq!(Message::parse(&[]).unwrap().get(1), None);
    }
}
