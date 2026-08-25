//! Normalizes schemars' nullable-union rendering of `Option<T>` fields
//! (`{"type": [T, "null"]}`) into a bare `{"type": T}`. Vertex's
//! `FunctionDeclaration` validator rejects `anyOf` with sibling keys, which is
//! what opencode's Gemini path (and `@ai-sdk/google`) turns that array `type`
//! into; optionality already survives via omission from `required`.

use rmcp::model::JsonObject;
use serde_json::Value;

/// Recursively collapses a `["T", "null"]` (or `["null", "T"]`) `"type"` into
/// `"T"`, dropping an accompanying `"default": null` (it would otherwise
/// contradict the collapsed scalar type). Walks into `properties`, `items`
/// and `$defs`; a non-null union, or any other field, is left untouched.
pub(crate) fn degrade_nullable_unions(schema: &mut JsonObject) {
    if let Some(Value::Array(types)) = schema.get("type") {
        let mut non_null = types.iter().filter(|t| *t != "null");
        let scalar = non_null.next().cloned();
        let single = non_null.next().is_none();
        let has_null = types.iter().any(|t| t == "null");
        if has_null
            && single
            && let Some(scalar) = scalar
        {
            schema.insert("type".to_string(), scalar);
            if schema.get("default") == Some(&Value::Null) {
                schema.remove("default");
            }
        }
    }

    for key in ["properties", "$defs"] {
        if let Some(Value::Object(properties)) = schema.get_mut(key) {
            for value in properties.values_mut() {
                if let Value::Object(nested) = value {
                    degrade_nullable_unions(nested);
                }
            }
        }
    }

    if let Some(Value::Object(items)) = schema.get_mut("items") {
        degrade_nullable_unions(items);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{JsonObject, degrade_nullable_unions};

    fn object(value: Value) -> JsonObject {
        let Value::Object(map) = value else {
            panic!("expected a JSON object")
        };
        map
    }

    #[test]
    fn collapses_a_top_level_nullable_scalar() {
        let mut schema = object(json!({
            "type": ["number", "null"],
            "format": "double",
            "default": null,
        }));
        degrade_nullable_unions(&mut schema);
        assert_eq!(
            Value::Object(schema),
            json!({ "type": "number", "format": "double" })
        );
    }

    #[test]
    fn collapses_nested_object_properties() {
        let mut schema = object(json!({
            "type": "object",
            "properties": {
                "arch": { "type": ["string", "null"], "default": null },
            },
        }));
        degrade_nullable_unions(&mut schema);
        assert_eq!(
            Value::Object(schema),
            json!({
                "type": "object",
                "properties": { "arch": { "type": "string" } },
            })
        );
    }

    #[test]
    fn collapses_array_items() {
        let mut schema = object(json!({
            "type": ["array", "null"],
            "default": null,
            "items": { "type": ["integer", "null"], "default": null },
        }));
        degrade_nullable_unions(&mut schema);
        assert_eq!(
            Value::Object(schema),
            json!({
                "type": "array",
                "items": { "type": "integer" },
            })
        );
    }

    #[test]
    fn leaves_a_non_null_union_alone() {
        let mut schema = object(json!({ "type": ["string", "integer"] }));
        let before = Value::Object(schema.clone());
        degrade_nullable_unions(&mut schema);
        assert_eq!(Value::Object(schema), before);
    }

    #[test]
    fn preserves_a_non_null_default() {
        let mut schema = object(json!({ "type": ["integer", "null"], "default": 0 }));
        degrade_nullable_unions(&mut schema);
        assert_eq!(
            Value::Object(schema),
            json!({ "type": "integer", "default": 0 })
        );
    }
}
