//! Canonical JSON, the byte form registry metadata is signed in.
//!
//! The encoding is OLPC canonical JSON as TUF uses it: object keys sorted
//! by their UTF-8 bytes, no insignificant whitespace, integers only (no
//! floating point), and strings with only `"` and `\` escaped. Two
//! documents with the same meaning therefore encode to the same bytes, so a
//! signature over the encoding does not depend on how a file was
//! formatted.

use serde::Serialize;
use serde_json::Value;

/// Why a value has no canonical encoding.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CanonicalError {
    /// The value could not be converted to JSON.
    #[error("cannot convert the value to JSON")]
    Json(#[from] serde_json::Error),
    /// The value holds a floating-point number.
    #[error("canonical JSON has no floating-point numbers")]
    Float,
}

/// Encodes `value` as canonical JSON.
///
/// # Errors
///
/// [`CanonicalError::Json`] if `value` cannot be serialized, and
/// [`CanonicalError::Float`] if it holds a floating-point number.
///
/// # Examples
///
/// ```
/// let bytes = fox_index::canonical::to_vec(&serde_json::json!({"b": 1, "a": "x\"y"}))?;
/// assert_eq!(bytes, br#"{"a":"x\"y","b":1}"#);
/// # Ok::<(), fox_index::canonical::CanonicalError>(())
/// ```
pub fn to_vec(value: &impl Serialize) -> Result<Vec<u8>, CanonicalError> {
    let value = serde_json::to_value(value)?;
    let mut out = Vec::new();
    write(&value, &mut out)?;
    Ok(out)
}

fn write(value: &Value, out: &mut Vec<u8>) -> Result<(), CanonicalError> {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(number) => {
            if !(number.is_i64() || number.is_u64()) {
                return Err(CanonicalError::Float);
            }
            out.extend_from_slice(number.to_string().as_bytes());
        }
        Value::String(text) => write_string(text, out),
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write(item, out)?;
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            out.push(b'{');
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_string(key, out);
                out.push(b':');
                write(item, out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

fn write_string(text: &str, out: &mut Vec<u8>) {
    out.push(b'"');
    for byte in text.bytes() {
        if matches!(byte, b'"' | b'\\') {
            out.push(b'\\');
        }
        out.push(byte);
    }
    out.push(b'"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sorts_keys_and_drops_whitespace() {
        let value = json!({"z": [1, {"b": null, "a": true}], "a": -3, "é": "\\n"});
        assert_eq!(
            to_vec(&value).unwrap(),
            "{\"a\":-3,\"z\":[1,{\"a\":true,\"b\":null}],\"é\":\"\\\\n\"}".as_bytes()
        );
    }

    #[test]
    fn refuses_floats() {
        assert!(matches!(to_vec(&json!(1.5)), Err(CanonicalError::Float)));
    }

    #[test]
    fn control_characters_are_not_escaped() {
        assert_eq!(to_vec(&json!("a\nb")).unwrap(), b"\"a\nb\"");
    }
}
