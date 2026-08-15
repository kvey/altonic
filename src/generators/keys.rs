//! Key generators (SPEC §6): format-preserving key masking plus sequential
//! and random key synthesis. All support uniqueness via engine retry.

use super::{require_type, CellCtx, Generator, Opts};
use crate::rng::Rng;
use crate::schema::Column;
use crate::value::{ColumnType, Value};
use crate::{verr, Result};

// ---- integer_key ----

struct IntegerKey {
    min: i64,
    max: i64,
}

impl Generator for IntegerKey {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        if input.is_null() {
            return Ok(Value::Null);
        }
        Ok(Value::Int(rng.range_i64(self.min, self.max)))
    }

    fn supports_uniqueness(&self) -> bool {
        true
    }
}

pub fn integer_key(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&["min", "max"])?;
    require_type("integer_key", column, |t| t == ColumnType::Integer, "integer")?;
    let min = opts.i64("min")?.unwrap_or(1);
    let max = opts.i64("max")?.unwrap_or(i64::MAX - 1);
    if min >= max {
        return Err(verr("generator \"integer_key\": min must be < max"));
    }
    Ok(Box::new(IntegerKey { min, max }))
}

// ---- uuid_key / random_uuid share the format ----

pub(crate) fn format_uuid(rng: &mut Rng) -> String {
    // v4-format: 122 random bits, version and variant nibbles fixed.
    let a = rng.next_u64();
    let b = rng.next_u64();
    let bytes: Vec<u8> = a.to_be_bytes().iter().chain(b.to_be_bytes().iter()).copied().collect();
    let mut hex = String::with_capacity(36);
    for (i, byte) in bytes.iter().enumerate() {
        let byte = match i {
            6 => (byte & 0x0f) | 0x40, // version 4
            8 => (byte & 0x3f) | 0x80, // RFC 4122 variant
            _ => *byte,
        };
        if matches!(i, 4 | 6 | 8 | 10) {
            hex.push('-');
        }
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

struct UuidKey;

impl Generator for UuidKey {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        if input.is_null() {
            return Ok(Value::Null);
        }
        Ok(Value::Text(format_uuid(rng)))
    }

    fn supports_uniqueness(&self) -> bool {
        true
    }
}

pub fn uuid_key(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&[])?;
    require_type("uuid_key", column, |t| matches!(t, ColumnType::Uuid | ColumnType::Text), "uuid or text")?;
    Ok(Box::new(UuidKey))
}

// ---- alphanumeric_key: format-preserving ----

struct AlphanumericKey;

impl Generator for AlphanumericKey {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        let s = match input {
            Value::Null => return Ok(Value::Null),
            Value::Text(s) => s.clone(),
            other => other.to_string(),
        };
        let out: String = s
            .chars()
            .map(|c| {
                if c.is_ascii_digit() {
                    char::from(b'0' + rng.below(10) as u8)
                } else if c.is_ascii_lowercase() {
                    char::from(b'a' + rng.below(26) as u8)
                } else if c.is_ascii_uppercase() {
                    char::from(b'A' + rng.below(26) as u8)
                } else {
                    c
                }
            })
            .collect();
        Ok(Value::Text(out))
    }

    fn supports_uniqueness(&self) -> bool {
        true
    }
}

pub fn alphanumeric_key(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&[])?;
    require_type("alphanumeric_key", column, |t| t.is_textual(), "text")?;
    Ok(Box::new(AlphanumericKey))
}

// ---- numeric_string_key: digits remapped, everything else kept ----

struct NumericStringKey;

impl Generator for NumericStringKey {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        let s = match input {
            Value::Null => return Ok(Value::Null),
            Value::Text(s) => s.clone(),
            other => other.to_string(),
        };
        let out: String = s
            .chars()
            .map(|c| if c.is_ascii_digit() { char::from(b'0' + rng.below(10) as u8) } else { c })
            .collect();
        Ok(Value::Text(out))
    }

    fn supports_uniqueness(&self) -> bool {
        true
    }
}

pub fn numeric_string_key(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&[])?;
    require_type("numeric_string_key", column, |t| t.is_textual(), "text")?;
    Ok(Box::new(NumericStringKey))
}

// ---- sequential_integer ----

struct SequentialInteger {
    start: i64,
    step: i64,
}

impl Generator for SequentialInteger {
    fn generate(&self, _input: &Value, ctx: &CellCtx, _rng: &mut Rng) -> Result<Value> {
        Ok(Value::Int(self.start.wrapping_add(self.step.wrapping_mul(ctx.row_ordinal as i64))))
    }

    fn supports_uniqueness(&self) -> bool {
        true
    }

    fn is_data_free(&self) -> bool {
        true
    }
}

pub fn sequential_integer(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&["start", "step"])?;
    require_type("sequential_integer", column, |t| t == ColumnType::Integer, "integer")?;
    let start = opts.i64("start")?.unwrap_or(1);
    let step = opts.i64("step")?.unwrap_or(1);
    if step == 0 {
        return Err(verr("generator \"sequential_integer\": step must not be 0"));
    }
    Ok(Box::new(SequentialInteger { start, step }))
}

#[cfg(test)]
mod tests {
    use super::super::test_util::*;
    use crate::value::Value;

    #[test]
    fn integer_key_deterministic_and_bounded() {
        let table = test_table();
        let c = col(&table, "n");
        let opts = crate::json::Json::parse(r#"{"consistency": "value", "min": 100, "max": 999}"#).unwrap();
        let b = super::super::build("integer_key", &opts, &table, &c).unwrap();
        let a1 = run_gen(&b, Value::Int(42), &table, vec![Value::Null; 7]);
        let a2 = run_gen(&b, Value::Int(42), &table, vec![Value::Null; 7]);
        let d = run_gen(&b, Value::Int(43), &table, vec![Value::Null; 7]);
        assert_eq!(a1, a2); // consistent
        assert_ne!(a1, d);
        match a1 {
            Value::Int(i) => assert!((100..=999).contains(&i)),
            other => panic!("expected int, got {other:?}"),
        }
    }

    #[test]
    fn uuid_shape() {
        let v = quick("uuid_key", "{}", "abc");
        let s = v.as_text().unwrap();
        assert_eq!(s.len(), 36);
        assert_eq!(s.chars().nth(14), Some('4')); // version nibble
        assert!(matches!(s.chars().nth(19), Some('8' | '9' | 'a' | 'b')));
    }

    #[test]
    fn format_preserving_keys() {
        let v = quick("alphanumeric_key", "{}", "AB-12_cd");
        let s = v.as_text().unwrap();
        assert_eq!(s.len(), 8);
        assert_eq!(s.chars().nth(2), Some('-'));
        assert_eq!(s.chars().nth(5), Some('_'));
        assert!(s.chars().next().unwrap().is_ascii_uppercase());
        assert!(s.chars().nth(3).unwrap().is_ascii_digit());
        assert!(s.chars().nth(6).unwrap().is_ascii_lowercase());

        let v = quick("numeric_string_key", "{}", "123-45-6789");
        let s = v.as_text().unwrap();
        assert_eq!(s.len(), 11);
        assert_eq!(s.chars().nth(3), Some('-'));
        assert!(s.chars().next().unwrap().is_ascii_digit());
    }

    #[test]
    fn null_passthrough() {
        let table = test_table();
        let c = col(&table, "n");
        let opts = crate::json::Json::parse("{}").unwrap();
        let b = super::super::build("integer_key", &opts, &table, &c).unwrap();
        assert_eq!(run_gen(&b, Value::Null, &table, vec![Value::Null; 7]), Value::Null);
    }

    #[test]
    fn type_mismatch_rejected() {
        let table = test_table();
        let c = col(&table, "c"); // text column
        let opts = crate::json::Json::parse("{}").unwrap();
        assert!(super::super::build("integer_key", &opts, &table, &c).is_err());
    }

    #[test]
    fn unknown_option_rejected() {
        let table = test_table();
        let c = col(&table, "n");
        let opts = crate::json::Json::parse(r#"{"bogus": 1}"#).unwrap();
        assert!(super::super::build("integer_key", &opts, &table, &c).is_err());
    }
}
