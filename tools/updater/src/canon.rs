//! Canonical JSON — an RFC 8785 (JCS) subset sufficient for signing G006
//! complete-set manifests: UTF-8 output, object keys sorted
//! lexicographically by their UTF-16 code unit sequence (equivalent to a
//! plain byte-wise sort for the ASCII-only keys this manifest ever uses),
//! no insignificant whitespace, and minimal string escaping (only the
//! characters JSON requires an escape for — everything else, including
//! non-ASCII, is emitted as literal UTF-8).
//!
//! Deliberately narrow: the manifest never contains floating-point
//! numbers, so [`canonicalize`] only promises correct behavior for
//! integers, strings, bools, null, arrays and objects built from those
//! — and *enforces* that promise by rejecting (never silently
//! reformatting) any JSON number that isn't exactly representable as a
//! `u64`/`i64`. Ryu's shortest-round-trip float formatting is not JCS
//! canonical, so emitting it here would quietly produce bytes a
//! spec-compliant JCS verifier could disagree with; a signing/
//! verification path must fail loudly instead.
//! `serde_json`'s default (non-`preserve_order`) `Map` already happens to
//! be `BTreeMap`-backed and its `to_string()` already omits whitespace,
//! but this module does not rely on that Cargo feature default staying
//! put — key sorting and whitespace are both enforced explicitly here.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CanonError {
    #[error("failed to serialize value to JSON: {0}")]
    Serialize(String),
    #[error(
        "JSON number {0} is not exactly representable as a u64/i64 — canonical JSON here never emits floats"
    )]
    NonIntegerNumber(String),
}

/// Serialize `value` to its canonical-JSON byte representation.
/// Fails (rather than falling back to a non-canonical representation)
/// when `value` contains a JSON number that isn't exactly an integer.
pub fn canonicalize(value: &Value) -> Result<String, CanonError> {
    let mut out = String::new();
    write_canonical(value, &mut out)?;
    Ok(out)
}

/// Convenience: canonicalize the JSON serialization of any `Serialize`
/// value.
pub fn canonicalize_serialize<T: serde::Serialize>(value: &T) -> Result<String, CanonError> {
    let json = serde_json::to_value(value).map_err(|e| CanonError::Serialize(e.to_string()))?;
    canonicalize(&json)
}

fn write_canonical(value: &Value, out: &mut String) -> Result<(), CanonError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            // Manifests only ever carry non-negative integers (sizes,
            // versions); reject anything else loudly instead of silently
            // emitting a non-canonical float representation.
            if let Some(u) = n.as_u64() {
                out.push_str(&u.to_string());
            } else if let Some(i) = n.as_i64() {
                out.push_str(&i.to_string());
            } else {
                return Err(CanonError::NonIntegerNumber(n.to_string()));
            }
        }
        Value::String(s) => write_canonical_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_unstable();
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical_string(key, out);
                out.push(':');
                write_canonical(map.get(*key).expect("key from map.keys()"), out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

fn write_canonical_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sorts_object_keys_lexicographically() {
        let value = json!({"b": 1, "a": 2, "c": 3});
        assert_eq!(canonicalize(&value).expect("integers only"), r#"{"a":2,"b":1,"c":3}"#);
    }

    #[test]
    fn nested_objects_and_arrays_sort_recursively() {
        let value = json!({
            "z": [ {"y": 1, "x": 2}, 3 ],
            "a": {"d": 1, "b": 2},
        });
        assert_eq!(
            canonicalize(&value).expect("integers only"),
            r#"{"a":{"b":2,"d":1},"z":[{"x":2,"y":1},3]}"#
        );
    }

    #[test]
    fn no_insignificant_whitespace() {
        let value = json!({"a": [1, 2, 3], "b": "x"});
        let out = canonicalize(&value).expect("integers only");
        assert!(!out.contains(' '));
        assert!(!out.contains('\n'));
    }

    #[test]
    fn escapes_control_and_quote_characters_minimally() {
        let value = json!({"k": "a\"b\\c\nd\te\u{01}f"});
        assert_eq!(
            canonicalize(&value).expect("integers only"),
            r#"{"k":"a\"b\\c\nd\te\u0001f"}"#
        );
    }

    #[test]
    fn non_ascii_is_kept_literal_utf8_not_escaped() {
        let value = json!({"name": "héllo-世界"});
        let out = canonicalize(&value).expect("integers only");
        assert_eq!(out, "{\"name\":\"héllo-世界\"}");
    }

    #[test]
    fn bools_and_null_round_trip() {
        assert_eq!(canonicalize(&json!(true)).expect("bool"), "true");
        assert_eq!(canonicalize(&json!(false)).expect("bool"), "false");
        assert_eq!(canonicalize(&json!(null)).expect("null"), "null");
    }

    #[test]
    fn is_deterministic_regardless_of_input_key_order() {
        let a = json!({"a": 1, "b": 2});
        let b = json!({"b": 2, "a": 1});
        assert_eq!(
            canonicalize(&a).expect("integers only"),
            canonicalize(&b).expect("integers only")
        );
    }

    #[test]
    fn integers_are_accepted_at_top_level_and_nested() {
        assert!(canonicalize(&json!(42)).is_ok());
        assert!(canonicalize(&json!({"size": 1024, "list": [1, 2, 3]})).is_ok());
    }

    #[test]
    fn a_float_anywhere_in_the_value_is_loudly_rejected() {
        let top_level = canonicalize(&json!(1.5));
        assert_eq!(
            top_level,
            Err(CanonError::NonIntegerNumber("1.5".to_string()))
        );

        let nested_in_object = canonicalize(&json!({"size": 1024.25}));
        assert!(matches!(nested_in_object, Err(CanonError::NonIntegerNumber(_))));

        let nested_in_array = canonicalize(&json!([1, 2, 3.0001]));
        assert!(matches!(nested_in_array, Err(CanonError::NonIntegerNumber(_))));
    }

    #[test]
    fn the_rejection_message_names_the_offending_number() {
        let err = canonicalize(&json!(2.75)).expect_err("float must be rejected");
        assert!(err.to_string().contains("2.75"));
        assert!(err.to_string().to_lowercase().contains("not exactly representable"));
    }
}
