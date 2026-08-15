//! The value tree configs, schemas and reports are built from.
//!
//! Objects preserve member insertion order (Vec-backed) so reports and
//! rewritten configs are stable — which is why this is its own type rather
//! than `serde_json::Value`. Numbers are kept as i64 when exactly integral,
//! f64 otherwise. Parsing and serialization are `serde`'s; implementing
//! `Serialize`/`Deserialize` here is also what lets the TOML front-end reuse
//! the same tree.

use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        match self {
            Json::Object(members) => members.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Int(i) => Some(*i),
            Json::Float(f) if f.fract() == 0.0 && f.is_finite() => Some(*f as i64),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Int(i) => Some(*i as f64),
            Json::Float(f) => Some(*f),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Object(o) => Some(o),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    /// Set (replace or append) a key on an object. No-op on non-objects.
    pub fn set(&mut self, key: &str, value: Json) {
        if let Json::Object(members) = self {
            for (k, v) in members.iter_mut() {
                if k == key {
                    *v = value;
                    return;
                }
            }
            members.push((key.to_string(), value));
        }
    }

    pub fn parse(input: &str) -> Result<Json, String> {
        serde_json::from_str(input).map_err(|e| e.to_string())
    }

    pub fn to_string_pretty(&self) -> String {
        let mut out = serde_json::to_string_pretty(self).expect("Json always serializes");
        out.push('\n');
        out
    }

    pub fn to_string_compact(&self) -> String {
        serde_json::to_string(self).expect("Json always serializes")
    }
}

impl Serialize for Json {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Json::Null => s.serialize_unit(),
            Json::Bool(b) => s.serialize_bool(*b),
            Json::Int(i) => s.serialize_i64(*i),
            // JSON has no NaN or Infinity.
            Json::Float(f) if !f.is_finite() => s.serialize_unit(),
            Json::Float(f) => s.serialize_f64(*f),
            Json::Str(v) => s.serialize_str(v),
            Json::Array(items) => {
                let mut seq = s.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Json::Object(members) => {
                let mut map = s.serialize_map(Some(members.len()))?;
                for (k, v) in members {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Json, D::Error> {
        d.deserialize_any(JsonVisitor)
    }
}

/// TOML surfaces datetimes to an untyped visitor as a map under this reserved
/// key. altonic has no datetime-typed config fields, and silently turning one
/// into an object would be worse than saying so.
const TOML_DATETIME_KEY: &str = "$__toml_private_datetime";

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Json;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any value")
    }

    fn visit_unit<E>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_none<E>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Json, D::Error> {
        d.deserialize_any(self)
    }

    fn visit_bool<E>(self, v: bool) -> Result<Json, E> {
        Ok(Json::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Json, E> {
        Ok(Json::Int(v))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Json, E> {
        Ok(match i64::try_from(v) {
            Ok(i) => Json::Int(i),
            Err(_) => Json::Float(v as f64),
        })
    }

    fn visit_f64<E>(self, v: f64) -> Result<Json, E> {
        Ok(Json::Float(v))
    }

    fn visit_str<E>(self, v: &str) -> Result<Json, E> {
        Ok(Json::Str(v.to_string()))
    }

    fn visit_string<E>(self, v: String) -> Result<Json, E> {
        Ok(Json::Str(v))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
        let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Json::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
        let mut members: Vec<(String, Json)> = Vec::with_capacity(map.size_hint().unwrap_or(0));
        while let Some(key) = map.next_key::<String>()? {
            if key == TOML_DATETIME_KEY {
                return Err(serde::de::Error::custom(
                    "datetime values are not supported; quote them as strings",
                ));
            }
            members.push((key, map.next_value()?));
        }
        Ok(Json::Object(members))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let src = r#"{"a": 1, "b": [true, null, -2.5, "x\ny"], "c": {"nested": "value"}, "u": "\u00e9\ud83d\ude00"}"#;
        let v = Json::parse(src).unwrap();
        assert_eq!(v.get("a").unwrap().as_i64(), Some(1));
        assert_eq!(v.get("b").unwrap().as_array().unwrap().len(), 4);
        assert_eq!(v.get("u").unwrap().as_str(), Some("é😀"));
        let reparsed = Json::parse(&v.to_string_pretty()).unwrap();
        assert_eq!(v, reparsed);
        let reparsed2 = Json::parse(&v.to_string_compact()).unwrap();
        assert_eq!(v, reparsed2);
    }

    #[test]
    fn preserves_member_order() {
        let v = Json::parse(r#"{"z": 1, "a": 2, "m": 3}"#).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["z", "a", "m"]);
    }

    #[test]
    fn rejects_garbage() {
        for bad in ["{", "[1,", "\"abc", "01", "1.", "{'a':1}", "[1 2]", "nul", "\"\\ud800\""] {
            assert!(Json::parse(bad).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn number_edges() {
        assert_eq!(Json::parse("9223372036854775807").unwrap().as_i64(), Some(i64::MAX));
        assert_eq!(Json::parse("-1e3").unwrap().as_f64(), Some(-1000.0));
        assert_eq!(Json::parse("0.5").unwrap().as_f64(), Some(0.5));
        // Beyond i64 stays representable as a float rather than failing.
        assert!(Json::parse("18446744073709551615").unwrap().as_f64().is_some());
    }

    #[test]
    fn integral_floats_keep_their_point() {
        // Distinguishes a Float from an Int on the way out, so a round trip
        // through a report does not silently retype a column's value.
        assert_eq!(Json::Float(3.0).to_string_compact(), "3.0");
        assert_eq!(Json::Int(3).to_string_compact(), "3");
    }

    #[test]
    fn non_finite_floats_become_null() {
        assert_eq!(Json::Float(f64::NAN).to_string_compact(), "null");
        assert_eq!(Json::Float(f64::INFINITY).to_string_compact(), "null");
    }
}
