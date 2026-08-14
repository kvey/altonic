//! Datetime generators (SPEC §6): shift, truncation, random.

use super::{require_type, CellCtx, Generator, Opts};
use crate::rng::Rng;
use crate::schema::Column;
use crate::value::{Timestamp, Value};
use crate::{verr, Result};

fn parse_ts(input: &Value) -> Result<Option<(Timestamp, String)>> {
    match input {
        Value::Null => Ok(None),
        Value::Text(s) => match Timestamp::parse(s) {
            Some(ts) => Ok(Some((ts, s.clone()))),
            None => Err(verr(format!("cannot parse {s:?} as a date/timestamp"))),
        },
        other => Err(verr(format!("expected a date/timestamp string, got {other:?}"))),
    }
}

// ---- timestamp_shift ----

struct TimestampShift {
    max_days: i64,
}

impl Generator for TimestampShift {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        let (ts, _) = match parse_ts(input)? {
            None => return Ok(Value::Null),
            Some(t) => t,
        };
        // Shift by ±max_days at second resolution (whole days for date-only).
        let out = if ts.had_time {
            let span = self.max_days * 86400;
            let delta = rng.range_i64(-span, span);
            Timestamp::from_epoch_seconds(ts.to_epoch_seconds() + delta, &ts)
        } else {
            let delta_days = rng.range_i64(-self.max_days, self.max_days);
            Timestamp::from_epoch_seconds(ts.to_epoch_seconds() + delta_days * 86400, &ts)
        };
        Ok(Value::Text(out.render()))
    }
}

pub fn timestamp_shift(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&["days"])?;
    require_type("timestamp_shift", column, |t| t.is_temporal() || t.is_textual(), "date/timestamp")?;
    let max_days = opts.i64("days")?.unwrap_or(30);
    if max_days <= 0 {
        return Err(verr("generator \"timestamp_shift\": days must be positive"));
    }
    Ok(Box::new(TimestampShift { max_days }))
}

// ---- date_truncation ----

#[derive(Clone, Copy)]
enum TruncateTo {
    Year,
    Month,
    Day,
    Hour,
}

struct DateTruncation {
    to: TruncateTo,
}

impl Generator for DateTruncation {
    fn generate(&self, input: &Value, _ctx: &CellCtx, _rng: &mut Rng) -> Result<Value> {
        let (mut ts, _) = match parse_ts(input)? {
            None => return Ok(Value::Null),
            Some(t) => t,
        };
        match self.to {
            TruncateTo::Year => {
                ts.month = 1;
                ts.day = 1;
                ts.hour = 0;
                ts.minute = 0;
                ts.second = 0;
            }
            TruncateTo::Month => {
                ts.day = 1;
                ts.hour = 0;
                ts.minute = 0;
                ts.second = 0;
            }
            TruncateTo::Day => {
                ts.hour = 0;
                ts.minute = 0;
                ts.second = 0;
            }
            TruncateTo::Hour => {
                ts.minute = 0;
                ts.second = 0;
            }
        }
        if ts.frac.is_some() {
            ts.frac = Some(0);
        }
        Ok(Value::Text(ts.render()))
    }
}

pub fn date_truncation(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&["to"])?;
    require_type("date_truncation", column, |t| t.is_temporal() || t.is_textual(), "date/timestamp")?;
    let to = match opts.str_or("to", "year")? {
        "year" => TruncateTo::Year,
        "month" => TruncateTo::Month,
        "day" => TruncateTo::Day,
        "hour" => TruncateTo::Hour,
        other => {
            return Err(verr(format!(
                "generator \"date_truncation\": to must be year|month|day|hour, got {other:?}"
            )))
        }
    };
    Ok(Box::new(DateTruncation { to }))
}

// ---- random_timestamp ----

struct RandomTimestamp {
    min: Timestamp,
    max: Timestamp,
}

impl Generator for RandomTimestamp {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        // Shape (date vs datetime) follows the input when parseable, else min.
        let like = match input {
            Value::Null => return Ok(Value::Null),
            Value::Text(s) => Timestamp::parse(s).unwrap_or(self.min),
            _ => self.min,
        };
        let lo = self.min.to_epoch_seconds();
        let hi = self.max.to_epoch_seconds();
        let secs = rng.range_i64(lo, hi);
        let secs = if like.had_time { secs } else { secs - secs.rem_euclid(86400) };
        Ok(Value::Text(Timestamp::from_epoch_seconds(secs, &like).render()))
    }

    fn is_data_free(&self) -> bool {
        true
    }
}

pub fn random_timestamp(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&["min", "max"])?;
    require_type("random_timestamp", column, |t| t.is_temporal() || t.is_textual(), "date/timestamp")?;
    let min_s = opts.str_or("min", "2000-01-01")?;
    let max_s = opts.str_or("max", "2030-01-01")?;
    let min = Timestamp::parse(min_s)
        .ok_or_else(|| verr(format!("generator \"random_timestamp\": bad min {min_s:?}")))?;
    let max = Timestamp::parse(max_s)
        .ok_or_else(|| verr(format!("generator \"random_timestamp\": bad max {max_s:?}")))?;
    if min.to_epoch_seconds() >= max.to_epoch_seconds() {
        return Err(verr("generator \"random_timestamp\": min must be before max"));
    }
    Ok(Box::new(RandomTimestamp { min, max }))
}

#[cfg(test)]
mod tests {
    use super::super::test_util::*;
    use crate::value::Timestamp;

    #[test]
    fn shift_stays_in_window_and_keeps_shape() {
        let v = quick("timestamp_shift", r#"{"days": 10}"#, "2024-06-15 12:00:00");
        let s = v.as_text().unwrap();
        let ts = Timestamp::parse(s).unwrap();
        let orig = Timestamp::parse("2024-06-15 12:00:00").unwrap();
        let delta = (ts.to_epoch_seconds() - orig.to_epoch_seconds()).abs();
        assert!(delta <= 10 * 86400);
        assert!(ts.had_time && !ts.sep_t);

        // Date-only input stays date-only.
        let v = quick("timestamp_shift", r#"{"days": 5}"#, "2024-06-15");
        assert_eq!(v.as_text().unwrap().len(), 10);
    }

    #[test]
    fn truncation() {
        assert_eq!(
            quick("date_truncation", r#"{"to": "month"}"#, "2024-06-15 12:34:56").as_text().unwrap(),
            "2024-06-01 00:00:00"
        );
        assert_eq!(
            quick("date_truncation", r#"{"to": "year"}"#, "2024-06-15").as_text().unwrap(),
            "2024-01-01"
        );
    }

    #[test]
    fn random_in_range() {
        let v = quick(
            "random_timestamp",
            r#"{"min": "2020-01-01", "max": "2021-01-01"}"#,
            "1999-12-31 08:00:00",
        );
        let ts = Timestamp::parse(v.as_text().unwrap()).unwrap();
        assert_eq!(ts.year, 2020);
        assert!(ts.had_time); // shape follows input
    }

    #[test]
    fn garbage_input_errors() {
        let table = test_table();
        let c = col(&table, "d");
        let opts = crate::json::Json::parse("{}").unwrap();
        let b = super::super::build("timestamp_shift", &opts, &table, &c).unwrap();
        let ctx_row = vec![crate::value::Value::Null; 7];
        let ctx = super::super::CellCtx {
            table: &table,
            row: &ctx_row,
            row_ordinal: 0,
            attempt: 0,
            secret: b"k",
        };
        let mut rng = crate::rng::Rng::from_seed(1);
        assert!(b
            .gen
            .generate(&crate::value::Value::Text("garbage".into()), &ctx, &mut rng)
            .is_err());
    }
}
