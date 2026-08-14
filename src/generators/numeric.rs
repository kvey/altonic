//! Numeric generators (SPEC §6), including the analysis-pass `continuous`
//! generator and the row-expression `algebraic` generator.

use super::{require_type, CellCtx, ColumnStats, Generator, Opts};
use crate::expr::Expr;
use crate::rng::Rng;
use crate::schema::{Column, Table};
use crate::value::{ColumnType, Value};
use crate::{verr, Result};

fn out_value(ty: ColumnType, v: f64) -> Value {
    match ty {
        ColumnType::Integer => Value::Int(v.round() as i64),
        _ => Value::Float(v),
    }
}

// ---- random_integer ----

struct RandomInteger {
    min: i64,
    max: i64,
}

impl Generator for RandomInteger {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        if input.is_null() {
            return Ok(Value::Null);
        }
        Ok(Value::Int(rng.range_i64(self.min, self.max)))
    }

    fn is_data_free(&self) -> bool {
        true
    }
}

pub fn random_integer(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&["min", "max"])?;
    require_type("random_integer", column, |t| t.is_numeric(), "numeric")?;
    let min = opts.i64("min")?.unwrap_or(0);
    let max = opts.i64("max")?.unwrap_or(1_000_000);
    if min > max {
        return Err(verr("generator \"random_integer\": min must be <= max"));
    }
    Ok(Box::new(RandomInteger { min, max }))
}

// ---- random_double ----

struct RandomDouble {
    min: f64,
    max: f64,
    decimals: Option<i64>,
}

impl Generator for RandomDouble {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        if input.is_null() {
            return Ok(Value::Null);
        }
        let mut v = rng.range_f64(self.min, self.max);
        if let Some(d) = self.decimals {
            let m = 10f64.powi(d as i32);
            v = (v * m).round() / m;
        }
        Ok(Value::Float(v))
    }

    fn is_data_free(&self) -> bool {
        true
    }
}

pub fn random_double(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&["min", "max", "decimals"])?;
    require_type("random_double", column, |t| t.is_numeric(), "numeric")?;
    let min = opts.f64("min")?.unwrap_or(0.0);
    let max = opts.f64("max")?.unwrap_or(1.0);
    if min >= max {
        return Err(verr("generator \"random_double\": min must be < max"));
    }
    Ok(Box::new(RandomDouble { min, max, decimals: opts.i64("decimals")? }))
}

// ---- noise ----

struct Noise {
    scale: f64, // relative noise, e.g. 0.1 = ±10%
    absolute: Option<f64>,
    decimals: Option<i64>,
    ty: ColumnType,
}

impl Generator for Noise {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        let x = match input.as_f64() {
            None => {
                return if input.is_null() {
                    Ok(Value::Null)
                } else {
                    Err(verr(format!("noise: non-numeric input {input:?}")))
                }
            }
            Some(x) => x,
        };
        let noise = match self.absolute {
            Some(a) => rng.range_f64(-a, a),
            None => x * self.scale * (rng.f64() * 2.0 - 1.0),
        };
        let mut v = x + noise;
        if let Some(d) = self.decimals {
            let m = 10f64.powi(d as i32);
            v = (v * m).round() / m;
        }
        Ok(out_value(self.ty, v))
    }
}

pub fn noise(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&["scale", "absolute", "decimals"])?;
    require_type("noise", column, |t| t.is_numeric(), "numeric")?;
    let scale = opts.f64("scale")?.unwrap_or(0.1);
    if !(0.0..=10.0).contains(&scale) {
        return Err(verr("generator \"noise\": scale must be in [0, 10]"));
    }
    Ok(Box::new(Noise {
        scale,
        absolute: opts.f64("absolute")?,
        decimals: opts.i64("decimals")?,
        ty: column.ty,
    }))
}

// ---- continuous: empirical-distribution sampling (analysis pass) ----

struct Continuous {
    stats: ColumnStats,
    ty: ColumnType,
}

impl Generator for Continuous {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        if input.is_null() {
            return Ok(Value::Null);
        }
        let n = self.stats.numbers.len();
        if n == 0 {
            return Err(verr("continuous: no numeric data collected for column"));
        }
        if n == 1 {
            return Ok(out_value(self.ty, self.stats.numbers[0]));
        }
        // Inverse-CDF sample of the empirical distribution with linear
        // interpolation between order statistics: preserves the marginal
        // shape without memorizing exact source values.
        let u = rng.f64() * (n - 1) as f64;
        let i = u.floor() as usize;
        let frac = u - i as f64;
        let v = self.stats.numbers[i] + (self.stats.numbers[i + 1] - self.stats.numbers[i]) * frac;
        Ok(out_value(self.ty, v))
    }

    fn needs_stats(&self) -> bool {
        true
    }

    fn set_stats(&mut self, stats: ColumnStats) {
        self.stats = stats;
    }
}

pub fn continuous(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&[])?;
    require_type("continuous", column, |t| t.is_numeric(), "numeric")?;
    Ok(Box::new(Continuous { stats: ColumnStats::default(), ty: column.ty }))
}

// ---- algebraic: expression over the source row ----

struct Algebraic {
    expr: Expr,
    ty: ColumnType,
}

impl Generator for Algebraic {
    fn generate(&self, _input: &Value, ctx: &CellCtx, _rng: &mut Rng) -> Result<Value> {
        let v = self.expr.eval(&|name| ctx.col_value(name))?;
        Ok(match v {
            Value::Null => Value::Null,
            other => match other.as_f64() {
                Some(f) => out_value(self.ty, f),
                None => other,
            },
        })
    }
}

pub fn algebraic(opts: &Opts, table: &Table, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&["expression"])?;
    require_type("algebraic", column, |t| t.is_numeric(), "numeric")?;
    let src = opts
        .str("expression")?
        .ok_or_else(|| verr("generator \"algebraic\": option \"expression\" is required"))?;
    let expr = Expr::parse(src)?;
    let mut cols = Vec::new();
    expr.columns(&mut cols);
    for c in cols {
        if table.column(&c).is_none() {
            return Err(verr(format!(
                "generator \"algebraic\": expression references unknown column {c:?} in table {:?}",
                table.name
            )));
        }
    }
    Ok(Box::new(Algebraic { expr, ty: column.ty }))
}

#[cfg(test)]
mod tests {
    use super::super::test_util::*;
    use super::*;
    use crate::value::Value;

    #[test]
    fn bounded_random() {
        let table = test_table();
        let c = col(&table, "n");
        let opts = crate::json::Json::parse(r#"{"min": 5, "max": 10}"#).unwrap();
        let b = super::super::build("random_integer", &opts, &table, &c).unwrap();
        for i in 0..50 {
            let v = run_gen(&b, Value::Int(i), &table, vec![Value::Null; 7]);
            match v {
                Value::Int(x) => assert!((5..=10).contains(&x)),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn noise_stays_close() {
        let table = test_table();
        let c = col(&table, "f");
        let opts = crate::json::Json::parse(r#"{"scale": 0.1}"#).unwrap();
        let b = super::super::build("noise", &opts, &table, &c).unwrap();
        let v = run_gen(&b, Value::Float(100.0), &table, vec![Value::Null; 7]);
        let x = v.as_f64().unwrap();
        assert!((90.0..=110.0).contains(&x), "{x}");
    }

    #[test]
    fn continuous_within_source_range() {
        let table = test_table();
        let c = col(&table, "f");
        let opts = crate::json::Json::parse("{}").unwrap();
        let mut b = super::super::build("continuous", &opts, &table, &c).unwrap();
        assert!(b.gen.needs_stats());
        b.gen.set_stats(ColumnStats {
            freqs: Vec::new(),
            numbers: vec![10.0, 12.0, 15.0, 40.0, 41.0],
        });
        for i in 0..100 {
            let v = run_gen(&b, Value::Float(i as f64), &table, vec![Value::Null; 7]);
            let x = v.as_f64().unwrap();
            assert!((10.0..=41.0).contains(&x), "{x}");
        }
    }

    #[test]
    fn algebraic_uses_row() {
        let table = test_table();
        let c = col(&table, "f");
        let opts = crate::json::Json::parse(r#"{"expression": "n * 2 + f"}"#).unwrap();
        let b = super::super::build("algebraic", &opts, &table, &c).unwrap();
        let mut row = vec![Value::Null; 7];
        row[1] = Value::Int(10); // n
        row[2] = Value::Float(1.5); // f
        let v = run_gen(&b, Value::Float(0.0), &table, row);
        assert_eq!(v.as_f64().unwrap(), 21.5);
    }

    #[test]
    fn algebraic_rejects_unknown_column() {
        let table = test_table();
        let c = col(&table, "f");
        let opts = crate::json::Json::parse(r#"{"expression": "nope + 1"}"#).unwrap();
        assert!(super::super::build("algebraic", &opts, &table, &c).is_err());
    }
}
