//! JSON shape of attribute values on the REST surface.
//!
//! The engine stores an attribute value as a tagged
//! [`AttributeValue`], which is what lets the result cache encode a hit
//! with bincode and decode it again. Bincode is not self-describing,
//! so an untagged value could be written but never read back. Over
//! HTTP the same value is a bare JSON scalar, because
//! `{"section": "warnings"}` is the shape a caller expects and
//! `{"section": {"String": "warnings"}}` is not.
//!
//! This module is the translation between the two, in both directions.

use serde_json::{Map, Number, Value};

use firnflow_core::{AttributeInput, AttributeMap, AttributeValue, FirnflowError};

/// Parse the `attributes` object of an upsert row into typed values.
///
/// The mapping is by JSON type, not by the column's declared type,
/// which the row does not know: a JSON string becomes a
/// [`AttributeValue::String`], an integral number an
/// [`AttributeValue::Int`], and so on. Reconciling that against the
/// declaration is the engine's job, and it is the engine that widens
/// an integer written to a `float` column.
///
/// An explicit `null` stores a null, the same as omitting the name,
/// but it is carried through as `None` rather than dropped here so the
/// engine still checks that the name is a declared column. Dropping it
/// would mean a caller who misspells a name while clearing a value
/// gets a `200` for a write that did nothing.
///
/// Arrays and objects are rejected: attributes are scalars, and
/// silently flattening a list would give the caller a column they
/// cannot filter the way they think.
pub fn attributes_from_json(
    row_id: u64,
    raw: Map<String, Value>,
) -> Result<AttributeInput, FirnflowError> {
    let mut out = AttributeInput::new();
    for (name, value) in raw {
        let parsed = match value {
            Value::Null => None,
            Value::String(s) => Some(AttributeValue::String(s)),
            Value::Bool(b) => Some(AttributeValue::Bool(b)),
            // An integer literal becomes an integer, and only a
            // literal that was written as a float becomes one. The
            // order matters for a whole number too large for `i64`:
            // it has an `as_f64` and taking it would silently round
            // the value, and doing that here would slip past the
            // exactness check the engine applies to an integer bound
            // for a `float` column. Refusing it keeps one rule.
            Value::Number(n) => match (n.as_i64(), n.is_f64()) {
                (Some(i), _) => Some(AttributeValue::Int(i)),
                (None, true) => Some(AttributeValue::Float(
                    n.as_f64().expect("is_f64 checked above"),
                )),
                (None, false) => {
                    return Err(FirnflowError::InvalidRequest(format!(
                        "row id {row_id}: attribute {name:?} is a whole number this server \
                         cannot represent ({n}); attribute integers fit in a signed 64-bit \
                         integer"
                    )));
                }
            },
            Value::Array(_) | Value::Object(_) => {
                return Err(FirnflowError::InvalidRequest(format!(
                    "row id {row_id}: attribute {name:?} must be a string, number, or boolean"
                )));
            }
        };
        out.insert(name, parsed);
    }
    Ok(out)
}

/// Render typed attribute values back as bare JSON scalars.
pub fn attributes_to_json(attributes: &AttributeMap) -> Map<String, Value> {
    attributes
        .iter()
        .map(|(name, value)| {
            let rendered = match value {
                AttributeValue::String(s) => Value::String(s.clone()),
                AttributeValue::Int(i) => Value::Number(Number::from(*i)),
                AttributeValue::Bool(b) => Value::Bool(*b),
                // A float that has no JSON representation cannot have
                // arrived over this API in the first place, since JSON
                // has no literal for one. Render it as null rather than
                // fail a whole result set on a value the caller could
                // only have produced through the embedded interface.
                AttributeValue::Float(f) => Number::from_f64(*f)
                    .map(Value::Number)
                    .unwrap_or(Value::Null),
            };
            (name.clone(), rendered)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_object(raw: &str) -> Map<String, Value> {
        match serde_json::from_str(raw).unwrap() {
            Value::Object(map) => map,
            other => panic!("expected an object, got {other:?}"),
        }
    }

    #[test]
    fn scalars_map_by_json_type() {
        let parsed = attributes_from_json(
            1,
            json_object(r#"{"section":"warnings","year":2024,"score":0.5,"archived":false}"#),
        )
        .unwrap();
        assert_eq!(
            parsed.get("section"),
            Some(&Some(AttributeValue::String("warnings".into())))
        );
        assert_eq!(parsed.get("year"), Some(&Some(AttributeValue::Int(2024))));
        assert_eq!(parsed.get("score"), Some(&Some(AttributeValue::Float(0.5))));
        assert_eq!(
            parsed.get("archived"),
            Some(&Some(AttributeValue::Bool(false)))
        );
    }

    /// A null keeps its name so the engine can check it against the
    /// declaration; it just carries no value.
    #[test]
    fn explicit_null_keeps_its_name() {
        let parsed = attributes_from_json(1, json_object(r#"{"section":null}"#)).unwrap();
        assert_eq!(parsed.get("section"), Some(&None));
    }

    /// A whole number past `i64` has an `f64` form, and taking it
    /// would round the value and route it around the exactness check
    /// the engine applies to an integer written to a `float` column.
    #[test]
    fn whole_numbers_too_large_for_an_integer_are_rejected() {
        let err =
            attributes_from_json(3, json_object(r#"{"year":18446744073709551615}"#)).unwrap_err();
        match err {
            FirnflowError::InvalidRequest(msg) => {
                assert!(msg.contains("cannot represent"), "{msg}");
            }
            other => panic!("expected InvalidRequest, got {other:?}"),
        }

        // Written as a float, the same magnitude is a float and is
        // accepted as one.
        let parsed =
            attributes_from_json(3, json_object(r#"{"score":1.8446744073709552e19}"#)).unwrap();
        assert!(matches!(
            parsed.get("score"),
            Some(Some(AttributeValue::Float(_)))
        ));
    }

    #[test]
    fn composite_values_are_rejected() {
        for raw in [r#"{"tags":["a","b"]}"#, r#"{"meta":{"a":1}}"#] {
            let err = attributes_from_json(9, json_object(raw)).unwrap_err();
            match err {
                FirnflowError::InvalidRequest(msg) => {
                    assert!(msg.contains("row id 9"), "{msg}");
                    assert!(msg.contains("string, number, or boolean"), "{msg}");
                }
                other => panic!("expected InvalidRequest, got {other:?}"),
            }
        }
    }

    #[test]
    fn rendering_round_trips_through_json() {
        let parsed = attributes_from_json(
            1,
            json_object(r#"{"section":"warnings","year":2024,"score":0.5,"archived":true}"#),
        )
        .unwrap();
        let read_back: AttributeMap = parsed
            .into_iter()
            .filter_map(|(name, value)| value.map(|v| (name, v)))
            .collect();
        let rendered = Value::Object(attributes_to_json(&read_back));
        assert_eq!(
            rendered,
            serde_json::json!({
                "section": "warnings",
                "year": 2024,
                "score": 0.5,
                "archived": true
            })
        );
    }
}
