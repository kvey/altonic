//! COPY text-format codec, SQL quoting, and Value ↔ Postgres-text mapping.

use crate::value::{ColumnType, Value};
use crate::{rerr, Result};

/// Quote an identifier for SQL (always quoted; internal quotes doubled).
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Quote a string literal.
pub fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Encode one field for COPY text format (backslash escapes per the COPY
/// docs). None encodes as \N at the call site, not here.
pub fn copy_escape(s: &str, out: &mut Vec<u8>) {
    for b in s.bytes() {
        match b {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            0x0b => out.extend_from_slice(b"\\v"),
            b => out.push(b),
        }
    }
}

/// Split one COPY text line into decoded fields. Raw tab bytes never occur
/// inside field data (the writer escapes them as `\t`), so fields are the
/// tab-separated raw chunks; a raw chunk of exactly `\N` means NULL, and
/// everything else is backslash-unescaped.
pub fn copy_split(line: &[u8]) -> Result<Vec<Option<String>>> {
    line.split(|&b| b == b'\t')
        .map(|raw| {
            if raw == b"\\N" {
                return Ok(None);
            }
            let bytes = copy_unescape(raw)?;
            String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| rerr("postgres: COPY field is not valid UTF-8"))
        })
        .collect()
}

fn copy_unescape(raw: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] != b'\\' {
            out.push(raw[i]);
            i += 1;
            continue;
        }
        let next = *raw
            .get(i + 1)
            .ok_or_else(|| rerr("postgres: dangling backslash in COPY data"))?;
        i += 2;
        match next {
            b'\\' => out.push(b'\\'),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'v' => out.push(0x0b),
            b'x' => {
                // \xH or \xHH
                let mut val: u8 = 0;
                let mut used = 0;
                while used < 2 {
                    match raw.get(i).and_then(|c| (*c as char).to_digit(16)) {
                        Some(d) => {
                            val = val.wrapping_mul(16).wrapping_add(d as u8);
                            used += 1;
                            i += 1;
                        }
                        None => break,
                    }
                }
                if used == 0 {
                    out.push(b'x'); // not an escape after all
                } else {
                    out.push(val);
                }
            }
            b'0'..=b'7' => {
                // Octal \o, \oo, \ooo (first digit already consumed)
                let mut val = (next - b'0') as u32;
                let mut used = 1;
                while used < 3 {
                    match raw.get(i) {
                        Some(&c) if (b'0'..=b'7').contains(&c) => {
                            val = val * 8 + (c - b'0') as u32;
                            used += 1;
                            i += 1;
                        }
                        _ => break,
                    }
                }
                out.push(val as u8);
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

/// Postgres text → typed Value. `t`/`f` booleans, NaN/Infinity floats, and
/// our timestamp shapes are all covered by Value::from_csv semantics plus
/// the special cases here.
pub fn text_to_value(text: Option<String>, ty: ColumnType, col: &str) -> Result<Value> {
    let Some(s) = text else { return Ok(Value::Null) };
    match ty {
        ColumnType::Integer => s
            .parse::<i64>()
            .map(Value::Int)
            .map_err(|_| rerr(format!("column {col:?}: invalid integer {s:?}"))),
        ColumnType::Float => s
            .parse::<f64>()
            .map(Value::Float)
            .map_err(|_| rerr(format!("column {col:?}: invalid float {s:?}"))),
        ColumnType::Boolean => match s.as_str() {
            "t" | "true" => Ok(Value::Bool(true)),
            "f" | "false" => Ok(Value::Bool(false)),
            _ => Err(rerr(format!("column {col:?}: invalid boolean {s:?}"))),
        },
        _ => Ok(Value::Text(s)),
    }
}

/// Typed Value → Postgres text (None = NULL).
pub fn value_to_text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::Bool(true) => Some("true".to_string()),
        Value::Bool(false) => Some("false".to_string()),
        Value::Int(i) => Some(i.to_string()),
        Value::Float(f) => Some(if f.is_nan() {
            "NaN".to_string()
        } else if f.is_infinite() {
            if *f > 0.0 { "Infinity".to_string() } else { "-Infinity".to_string() }
        } else if f.fract() == 0.0 && f.abs() < 1e15 {
            format!("{:.1}", f)
        } else {
            format!("{f}")
        }),
        Value::Text(s) => Some(s.clone()),
    }
}

/// Map a Postgres type (udt_name) to the portable column model.
pub fn map_pg_type(udt: &str) -> ColumnType {
    match udt {
        "int2" | "int4" | "int8" | "oid" => ColumnType::Integer,
        "float4" | "float8" | "numeric" => ColumnType::Float,
        "bool" => ColumnType::Boolean,
        "uuid" => ColumnType::Uuid,
        "date" => ColumnType::Date,
        "timestamp" | "timestamptz" => ColumnType::Timestamp,
        "json" | "jsonb" => ColumnType::Json,
        _ => ColumnType::Text,
    }
}

/// DDL type for a column when no exact source type is known (portable model
/// → sensible Postgres type).
pub fn default_pg_type(ty: ColumnType) -> &'static str {
    match ty {
        ColumnType::Integer => "bigint",
        ColumnType::Float => "double precision",
        ColumnType::Boolean => "boolean",
        ColumnType::Text => "text",
        ColumnType::Uuid => "uuid",
        ColumnType::Date => "date",
        ColumnType::Timestamp => "timestamp",
        ColumnType::Json => "jsonb",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn escape_str(s: &str) -> Vec<u8> {
        let mut out = Vec::new();
        copy_escape(s, &mut out);
        out
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_ident("weird\"name"), "\"weird\"\"name\"");
        assert_eq!(quote_literal("it's"), "'it''s'");
    }

    #[test]
    fn copy_round_trip() {
        // Build a line the way the writer does, split it back.
        let fields: [Option<&str>; 4] =
            [Some("plain"), None, Some("tab\there\nand\\slash"), Some("")];
        let mut line = Vec::new();
        for (i, f) in fields.iter().enumerate() {
            if i > 0 {
                line.push(b'\t');
            }
            match f {
                None => line.extend_from_slice(b"\\N"),
                Some(s) => line.extend_from_slice(&escape_str(s)),
            }
        }
        let parsed = copy_split(&line).unwrap();
        assert_eq!(parsed.len(), 4);
        assert_eq!(parsed[0].as_deref(), Some("plain"));
        assert_eq!(parsed[1], None);
        assert_eq!(parsed[2].as_deref(), Some("tab\there\nand\\slash"));
        assert_eq!(parsed[3].as_deref(), Some(""));
    }

    #[test]
    fn null_vs_literal_backslash_n() {
        // \N alone = NULL; "x\Ny"-style content is not.
        let parsed = copy_split(b"\\N\tx\\Ny").unwrap();
        assert_eq!(parsed[0], None);
        assert_eq!(parsed[1].as_deref(), Some("xNy"));
    }

    #[test]
    fn octal_and_hex_escapes() {
        let parsed = copy_split(b"a\\101b\t\\x41").unwrap();
        assert_eq!(parsed[0].as_deref(), Some("aAb"));
        assert_eq!(parsed[1].as_deref(), Some("A"));
    }

    #[test]
    fn value_text_round_trip() {
        assert_eq!(value_to_text(&Value::Null), None);
        assert_eq!(value_to_text(&Value::Bool(true)).as_deref(), Some("true"));
        assert_eq!(value_to_text(&Value::Float(2.5)).as_deref(), Some("2.5"));
        assert_eq!(value_to_text(&Value::Float(3.0)).as_deref(), Some("3.0"));
        assert_eq!(value_to_text(&Value::Float(f64::NAN)).as_deref(), Some("NaN"));
        assert_eq!(
            text_to_value(Some("t".into()), ColumnType::Boolean, "c").unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            text_to_value(Some("NaN".into()), ColumnType::Float, "c").unwrap().as_f64().map(f64::is_nan),
            Some(true)
        );
        assert_eq!(text_to_value(None, ColumnType::Integer, "c").unwrap(), Value::Null);
    }

    #[test]
    fn type_mapping() {
        assert_eq!(map_pg_type("int8"), ColumnType::Integer);
        assert_eq!(map_pg_type("varchar"), ColumnType::Text);
        assert_eq!(map_pg_type("timestamptz"), ColumnType::Timestamp);
        assert_eq!(map_pg_type("some_enum"), ColumnType::Text);
    }
}
