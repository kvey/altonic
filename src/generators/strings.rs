//! String, categorical, and miscellaneous value generators (SPEC §6).

use super::{require_type, CellCtx, ColumnStats, Generator, Opts};
use crate::crypto;
use crate::json::Json;
use crate::regex::{replace_all, Regex};
use crate::rng::Rng;
use crate::schema::Column;
use crate::value::{ColumnType, Value};
use crate::{verr, Result};

// ---- categorical: redraw from the column's own distribution ----

struct Categorical {
    stats: ColumnStats,
    /// Laplace-style noise scale applied to counts (rough DP mechanism hook;
    /// see SPEC §12 caveats). 0 disables.
    dp_noise: f64,
    noised: Vec<(Value, f64)>,
}

impl Categorical {
    fn weights(&self) -> &[(Value, f64)] {
        &self.noised
    }
}

impl Generator for Categorical {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        if input.is_null() {
            return Ok(Value::Null);
        }
        let weights = self.weights();
        if weights.is_empty() {
            return Err(verr("categorical: no values collected for column"));
        }
        let total: f64 = weights.iter().map(|(_, w)| *w).sum();
        let mut pick = rng.f64() * total;
        for (v, w) in weights {
            pick -= *w;
            if pick <= 0.0 {
                return Ok(v.clone());
            }
        }
        Ok(weights.last().unwrap().0.clone())
    }

    fn needs_stats(&self) -> bool {
        true
    }

    fn set_stats(&mut self, stats: ColumnStats) {
        // Apply count noise once, deterministically from the counts themselves
        // (the RNG passed to generate() varies per row; the noised distribution
        // must not).
        let mut seed = 0x9e3779b97f4a7c15u64;
        for (i, (_, c)) in stats.freqs.iter().enumerate() {
            seed = seed.wrapping_mul(0x100000001b3).wrapping_add(*c ^ i as u64);
        }
        let mut rng = Rng::from_seed(seed);
        self.noised = stats
            .freqs
            .iter()
            .map(|(v, c)| {
                let mut w = *c as f64;
                if self.dp_noise > 0.0 {
                    // Laplace(b) via inverse CDF.
                    let u = rng.f64() - 0.5;
                    let lap = -self.dp_noise * u.signum() * (1.0 - 2.0 * u.abs()).ln();
                    w = (w + lap).max(0.0);
                }
                (v.clone(), w)
            })
            .filter(|(_, w)| *w > 0.0)
            .collect();
        if self.noised.is_empty() {
            // All noised away: fall back to raw frequencies.
            self.noised = stats.freqs.iter().map(|(v, c)| (v.clone(), *c as f64)).collect();
        }
        self.stats = stats;
    }
}

pub fn categorical(opts: &Opts, _column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&["dp_noise"])?;
    let dp_noise = opts.f64("dp_noise")?.unwrap_or(0.0);
    if dp_noise < 0.0 {
        return Err(verr("generator \"categorical\": dp_noise must be >= 0"));
    }
    Ok(Box::new(Categorical { stats: ColumnStats::default(), dp_noise, noised: Vec::new() }))
}

// ---- custom_categorical ----

struct CustomCategorical {
    values: Vec<Value>,
}

impl Generator for CustomCategorical {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        if input.is_null() {
            return Ok(Value::Null);
        }
        Ok(rng.choose(&self.values).clone())
    }

    fn is_data_free(&self) -> bool {
        true
    }
}

pub fn custom_categorical(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&["values"])?;
    let arr = opts
        .get("values")
        .and_then(|v| v.as_array())
        .ok_or_else(|| verr("generator \"custom_categorical\": option \"values\" (array) is required"))?;
    if arr.is_empty() {
        return Err(verr("generator \"custom_categorical\": \"values\" must not be empty"));
    }
    let values = arr
        .iter()
        .map(|j| json_to_value(j, column.ty))
        .collect::<Result<Vec<_>>>()?;
    Ok(Box::new(CustomCategorical { values }))
}

fn json_to_value(j: &Json, ty: ColumnType) -> Result<Value> {
    Ok(match j {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Int(i) => {
            if ty == ColumnType::Float {
                Value::Float(*i as f64)
            } else {
                Value::Int(*i)
            }
        }
        Json::Float(f) => Value::Float(*f),
        Json::Str(s) => Value::Text(s.clone()),
        _ => return Err(verr("custom_categorical values must be scalars")),
    })
}

// ---- character_scramble: format-preserving randomization ----

struct CharacterScramble;

impl Generator for CharacterScramble {
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
                } else if c.is_alphabetic() {
                    // Non-ASCII letters scramble within a small latin set to
                    // stay printable.
                    char::from(b'a' + rng.below(26) as u8)
                } else {
                    c
                }
            })
            .collect();
        Ok(Value::Text(out))
    }
}

pub fn character_scramble(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&[])?;
    require_type("character_scramble", column, |t| t.is_textual(), "text")?;
    Ok(Box::new(CharacterScramble))
}

// ---- character_substitution: keyed per-character cipher ----

struct CharacterSubstitution;

impl Generator for CharacterSubstitution {
    fn generate(&self, input: &Value, ctx: &CellCtx, _rng: &mut Rng) -> Result<Value> {
        let s = match input {
            Value::Null => return Ok(Value::Null),
            Value::Text(s) => s.clone(),
            other => other.to_string(),
        };
        // Build substitution alphabets keyed by the workspace secret only, so
        // the mapping is column-independent and stable (a true substitution
        // cipher, same character always maps the same way).
        let digest = crypto::hmac_parts(ctx.secret, &[b"char-subst-alphabet"]);
        let mut rng = Rng::from_digest(&digest);
        let mut lower: Vec<u8> = (b'a'..=b'z').collect();
        let mut upper: Vec<u8> = (b'A'..=b'Z').collect();
        let mut digits: Vec<u8> = (b'0'..=b'9').collect();
        rng.shuffle(&mut lower);
        rng.shuffle(&mut upper);
        rng.shuffle(&mut digits);
        let out: String = s
            .chars()
            .map(|c| {
                if c.is_ascii_lowercase() {
                    lower[(c as u8 - b'a') as usize] as char
                } else if c.is_ascii_uppercase() {
                    upper[(c as u8 - b'A') as usize] as char
                } else if c.is_ascii_digit() {
                    digits[(c as u8 - b'0') as usize] as char
                } else {
                    c
                }
            })
            .collect();
        Ok(Value::Text(out))
    }
}

pub fn character_substitution(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&[])?;
    require_type("character_substitution", column, |t| t.is_textual(), "text")?;
    Ok(Box::new(CharacterSubstitution))
}

// ---- constant ----

struct Constant {
    value: Value,
    apply_to_null: bool,
}

impl Generator for Constant {
    fn generate(&self, input: &Value, _ctx: &CellCtx, _rng: &mut Rng) -> Result<Value> {
        if input.is_null() && !self.apply_to_null {
            return Ok(Value::Null);
        }
        Ok(self.value.clone())
    }

    fn is_data_free(&self) -> bool {
        true
    }
}

pub fn constant(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&["value", "apply_to_null"])?;
    let j = opts
        .get("value")
        .ok_or_else(|| verr("generator \"constant\": option \"value\" is required"))?;
    Ok(Box::new(Constant {
        value: json_to_value(j, column.ty)?,
        apply_to_null: opts.bool_or("apply_to_null", false)?,
    }))
}

// ---- find_and_replace ----

struct FindAndReplace {
    regex: Option<Regex>,
    find: String,
    replace: String,
}

impl Generator for FindAndReplace {
    fn generate(&self, input: &Value, _ctx: &CellCtx, _rng: &mut Rng) -> Result<Value> {
        let s = match input {
            Value::Null => return Ok(Value::Null),
            Value::Text(s) => s.clone(),
            other => other.to_string(),
        };
        Ok(Value::Text(match &self.regex {
            Some(re) => replace_all(re, &s, &self.replace),
            None => s.replace(&self.find, &self.replace),
        }))
    }
}

pub fn find_and_replace(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&["find", "replace", "regex"])?;
    require_type("find_and_replace", column, |t| t.is_textual(), "text")?;
    let find = opts
        .str("find")?
        .ok_or_else(|| verr("generator \"find_and_replace\": option \"find\" is required"))?
        .to_string();
    let replace = opts.str_or("replace", "")?.to_string();
    let regex = if opts.bool_or("regex", false)? { Some(Regex::new(&find)?) } else { None };
    Ok(Box::new(FindAndReplace { regex, find, replace }))
}

// ---- null ----

struct NullGen;

impl Generator for NullGen {
    fn generate(&self, _input: &Value, _ctx: &CellCtx, _rng: &mut Rng) -> Result<Value> {
        Ok(Value::Null)
    }

    fn is_data_free(&self) -> bool {
        true
    }
}

pub fn null_gen(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&[])?;
    if !column.nullable {
        return Err(verr(format!(
            "generator \"null\" cannot be used on non-nullable column {:?}",
            column.name
        )));
    }
    Ok(Box::new(NullGen))
}

// ---- random_boolean ----

struct RandomBoolean {
    true_probability: f64,
}

impl Generator for RandomBoolean {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        if input.is_null() {
            return Ok(Value::Null);
        }
        Ok(Value::Bool(rng.f64() < self.true_probability))
    }

    fn is_data_free(&self) -> bool {
        true
    }
}

pub fn random_boolean(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&["true_probability"])?;
    require_type("random_boolean", column, |t| t == ColumnType::Boolean, "boolean")?;
    let p = opts.f64("true_probability")?.unwrap_or(0.5);
    if !(0.0..=1.0).contains(&p) {
        return Err(verr("generator \"random_boolean\": true_probability must be in [0, 1]"));
    }
    Ok(Box::new(RandomBoolean { true_probability: p }))
}

// ---- random_hash: keyed pseudonymization ----

struct RandomHash {
    length: usize,
}

impl Generator for RandomHash {
    fn generate(&self, input: &Value, ctx: &CellCtx, _rng: &mut Rng) -> Result<Value> {
        if input.is_null() {
            return Ok(Value::Null);
        }
        let digest = crypto::hmac_parts(ctx.secret, &[b"random_hash", input.canonical().as_bytes()]);
        let hex = crypto::to_hex(&digest);
        Ok(Value::Text(hex[..self.length.min(64)].to_string()))
    }

    fn supports_uniqueness(&self) -> bool {
        true
    }
}

pub fn random_hash(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&["length"])?;
    require_type("random_hash", column, |t| t.is_textual(), "text")?;
    let length = opts.i64("length")?.unwrap_or(64);
    if !(8..=64).contains(&length) {
        return Err(verr("generator \"random_hash\": length must be in [8, 64]"));
    }
    Ok(Box::new(RandomHash { length: length as usize }))
}

// ---- random_uuid ----

struct RandomUuid;

impl Generator for RandomUuid {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        if input.is_null() {
            return Ok(Value::Null);
        }
        Ok(Value::Text(super::keys::format_uuid(rng)))
    }

    fn supports_uniqueness(&self) -> bool {
        true
    }

    fn is_data_free(&self) -> bool {
        true
    }
}

pub fn random_uuid(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&[])?;
    require_type("random_uuid", column, |t| matches!(t, ColumnType::Uuid | ColumnType::Text), "uuid or text")?;
    Ok(Box::new(RandomUuid))
}

// ---- passthrough ----

struct Passthrough;

impl Generator for Passthrough {
    fn generate(&self, input: &Value, _ctx: &CellCtx, _rng: &mut Rng) -> Result<Value> {
        Ok(input.clone())
    }
}

pub fn passthrough(opts: &Opts) -> Result<Box<dyn Generator>> {
    opts.check_known(&[])?;
    Ok(Box::new(Passthrough))
}

#[cfg(test)]
mod tests {
    use super::super::test_util::*;
    use super::*;
    use crate::value::Value;

    #[test]
    fn scramble_preserves_shape() {
        let v = quick("character_scramble", "{}", "Acct #12-AB_x");
        let s = v.as_text().unwrap();
        assert_eq!(s.len(), 13);
        assert!(s.chars().next().unwrap().is_ascii_uppercase());
        assert_eq!(&s[4..6], " #");
        assert!(s.chars().nth(6).unwrap().is_ascii_digit());
        assert_eq!(s.chars().nth(8), Some('-'));
        assert_eq!(s.chars().nth(11), Some('_'));
    }

    #[test]
    fn substitution_is_a_consistent_cipher() {
        let a = quick("character_substitution", "{}", "abcabc");
        let s = a.as_text().unwrap();
        // Same input char → same output char.
        assert_eq!(&s[0..3], &s[3..6]);
    }

    #[test]
    fn constant_and_null() {
        assert_eq!(
            quick("constant", r#"{"value": "REDACTED"}"#, "secret"),
            Value::Text("REDACTED".into())
        );
        assert_eq!(quick("null", "{}", "anything"), Value::Null);
    }

    #[test]
    fn find_replace_literal_and_regex() {
        assert_eq!(
            quick("find_and_replace", r#"{"find": "corp", "replace": "acme"}"#, "corp-corp"),
            Value::Text("acme-acme".into())
        );
        assert_eq!(
            quick(
                "find_and_replace",
                r#"{"find": "(\\d+)-(\\d+)", "replace": "$2/$1", "regex": true}"#,
                "call 12-34"
            ),
            Value::Text("call 34/12".into())
        );
    }

    #[test]
    fn custom_categorical_draws_from_list() {
        let v = quick("custom_categorical", r#"{"values": ["a", "b", "c"]}"#, "x");
        assert!(["a", "b", "c"].contains(&v.as_text().unwrap()));
    }

    #[test]
    fn categorical_respects_distribution() {
        let table = test_table();
        let c = col(&table, "c");
        // consistency:value makes run_gen's seed vary with the input, so the
        // 500 draws below actually sample the distribution.
        let opts = crate::json::Json::parse(r#"{"consistency": "value"}"#).unwrap();
        let mut b = super::super::build("categorical", &opts, &table, &c).unwrap();
        b.gen.set_stats(ColumnStats {
            freqs: vec![
                (Value::Text("common".into()), 90),
                (Value::Text("rare".into()), 10),
            ],
            numbers: vec![],
        });
        let mut common = 0;
        for i in 0..500 {
            // Non-consistent path would give varied seeds; emulate via input variety.
            let v = run_gen(&b, Value::Text(format!("in{i}")), &table, vec![Value::Null; 7]);
            if v.as_text() == Some("common") {
                common += 1;
            }
        }
        // Expect roughly 90%; loose bounds to avoid flakiness. Note run_gen
        // uses a fixed non-consistent seed, so vary through consistency.
        assert!(common > 300, "common drawn {common}/500");
    }

    #[test]
    fn random_hash_is_keyed_and_consistent() {
        let a = quick("random_hash", r#"{"length": 16}"#, "alice");
        let b = quick("random_hash", r#"{"length": 16}"#, "alice");
        let c = quick("random_hash", r#"{"length": 16}"#, "bob");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.as_text().unwrap().len(), 16);
    }

    #[test]
    fn passthrough_identity() {
        assert_eq!(quick("passthrough", "{}", "keep me"), Value::Text("keep me".into()));
    }

    #[test]
    fn null_rejected_on_non_nullable() {
        let j = crate::json::Json::parse(
            r#"{"tables": [{"name": "t", "columns": [{"name": "c", "type": "text", "nullable": false}]}]}"#,
        )
        .unwrap();
        let schema = crate::schema::Schema::from_json(&j).unwrap();
        let table = &schema.tables[0];
        let c = table.column("c").unwrap().clone();
        let opts = crate::json::Json::parse("{}").unwrap();
        assert!(super::super::build("null", &opts, table, &c).is_err());
    }
}
