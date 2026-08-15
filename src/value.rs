//! Cell values and column types (SPEC §3.1).

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColumnType {
    Integer,
    Float,
    Boolean,
    Text,
    Uuid,
    Date,
    Timestamp,
    Json,
}

impl ColumnType {
    pub fn parse(s: &str) -> Option<ColumnType> {
        Some(match s {
            "integer" | "int" | "bigint" => ColumnType::Integer,
            "float" | "double" | "numeric" | "decimal" | "real" => ColumnType::Float,
            "boolean" | "bool" => ColumnType::Boolean,
            "text" | "string" | "varchar" => ColumnType::Text,
            "uuid" => ColumnType::Uuid,
            "date" => ColumnType::Date,
            "timestamp" | "datetime" => ColumnType::Timestamp,
            "json" | "jsonb" => ColumnType::Json,
            _ => return None,
        })
    }

    pub fn name(&self) -> &'static str {
        match self {
            ColumnType::Integer => "integer",
            ColumnType::Float => "float",
            ColumnType::Boolean => "boolean",
            ColumnType::Text => "text",
            ColumnType::Uuid => "uuid",
            ColumnType::Date => "date",
            ColumnType::Timestamp => "timestamp",
            ColumnType::Json => "json",
        }
    }

    pub fn is_numeric(&self) -> bool {
        matches!(self, ColumnType::Integer | ColumnType::Float)
    }

    pub fn is_temporal(&self) -> bool {
        matches!(self, ColumnType::Date | ColumnType::Timestamp)
    }

    pub fn is_textual(&self) -> bool {
        matches!(self, ColumnType::Text | ColumnType::Uuid | ColumnType::Json)
    }
}

impl Value {
    /// Parse a CSV cell into a typed value. `quoted` distinguishes ""
    /// (empty string) from an unquoted empty cell (NULL).
    pub fn from_csv(text: &str, quoted: bool, ty: ColumnType) -> Result<Value, String> {
        if text.is_empty() && !quoted {
            return Ok(Value::Null);
        }
        match ty {
            ColumnType::Integer => text
                .parse::<i64>()
                .map(Value::Int)
                .map_err(|_| format!("invalid integer: {text:?}")),
            ColumnType::Float => text
                .parse::<f64>()
                .map(Value::Float)
                .map_err(|_| format!("invalid float: {text:?}")),
            ColumnType::Boolean => match text {
                "true" | "TRUE" | "t" | "1" => Ok(Value::Bool(true)),
                "false" | "FALSE" | "f" | "0" => Ok(Value::Bool(false)),
                _ => Err(format!("invalid boolean: {text:?}")),
            },
            ColumnType::Text | ColumnType::Uuid | ColumnType::Date | ColumnType::Timestamp | ColumnType::Json => {
                Ok(Value::Text(text.to_string()))
            }
        }
    }

    /// Render for CSV. Returns (text, force_quote): NULL renders as an
    /// unquoted empty cell, an empty string as a quoted one.
    pub fn to_csv(&self) -> (String, bool) {
        match self {
            Value::Null => (String::new(), false),
            Value::Bool(b) => (b.to_string(), false),
            Value::Int(i) => (i.to_string(), false),
            Value::Float(f) => (format_float(*f), false),
            Value::Text(s) => (s.clone(), s.is_empty()),
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// Canonical string form used for consistency seeding and key lookups.
    /// Chosen so that logically equal values (Int 5 vs Float 5.0 in an FK)
    /// normalize identically.
    pub fn canonical(&self) -> String {
        match self {
            Value::Null => "\u{0}null".to_string(),
            Value::Bool(b) => format!("b:{b}"),
            Value::Int(i) => format!("n:{i}"),
            Value::Float(f) => {
                if f.fract() == 0.0 && f.is_finite() && f.abs() < 9.3e18 {
                    format!("n:{}", *f as i64)
                } else {
                    format!("f:{f}")
                }
            }
            Value::Text(s) => format!("t:{s}"),
        }
    }
}

fn format_float(f: f64) -> String {
    if !f.is_finite() {
        return String::new();
    }
    if f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{:.1}", f)
    } else {
        format!("{f}")
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{}", format_float(*x)),
            Value::Text(s) => write!(f, "{s}"),
        }
    }
}

/// A row is a vector of values in the table's column order.
pub type Row = Vec<Value>;

// ---------- Timestamp handling (first principles, proleptic Gregorian) ----------

/// A parsed civil timestamp. Supports `YYYY-MM-DD`, `YYYY-MM-DD HH:MM:SS`,
/// and `YYYY-MM-DDTHH:MM:SS`, with optional fractional seconds and an
/// optional trailing `Z` or numeric UTC offset (`+00`, `-05:30`, `+0000`) as
/// Postgres `timestamptz` emits. The offset is preserved verbatim on render
/// but not applied to the civil fields — Postgres connections are pinned to
/// UTC (offset `+00`), so shift arithmetic stays correct.
#[derive(Debug, Clone, PartialEq)]
pub struct Timestamp {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub frac: Option<u32>, // nanoseconds-ish: raw digits, width preserved
    pub frac_width: usize,
    pub sep_t: bool,
    pub had_time: bool,
    pub had_z: bool,
    /// Verbatim numeric offset suffix (e.g. "+00", "-05:30"), if present.
    pub offset: Option<String>,
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Days since 1970-01-01 (can be negative), via the civil-from-days algorithm.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (y + if m <= 2 { 1 } else { 0 }, m, d)
}

impl Timestamp {
    pub fn parse(s: &str) -> Option<Timestamp> {
        let b: Vec<char> = s.trim().chars().collect();
        let digits = |from: usize, n: usize| -> Option<u32> {
            if from + n > b.len() {
                return None;
            }
            let mut v = 0u32;
            for &c in &b[from..from + n] {
                v = v * 10 + c.to_digit(10)?;
            }
            Some(v)
        };
        if b.len() < 10 {
            return None;
        }
        let year = digits(0, 4)? as i64;
        if b[4] != '-' || b[7] != '-' {
            return None;
        }
        let month = digits(5, 2)?;
        let day = digits(8, 2)?;
        if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
            return None;
        }
        let mut ts = Timestamp {
            year,
            month,
            day,
            hour: 0,
            minute: 0,
            second: 0,
            frac: None,
            frac_width: 0,
            sep_t: false,
            had_time: false,
            had_z: false,
            offset: None,
        };
        if b.len() == 10 {
            return Some(ts);
        }
        let sep = b[10];
        if sep != ' ' && sep != 'T' {
            return None;
        }
        ts.sep_t = sep == 'T';
        ts.had_time = true;
        if b.len() < 19 || b[13] != ':' || b[16] != ':' {
            return None;
        }
        ts.hour = digits(11, 2)?;
        ts.minute = digits(14, 2)?;
        ts.second = digits(17, 2)?;
        if ts.hour > 23 || ts.minute > 59 || ts.second > 59 {
            return None;
        }
        let mut i = 19;
        if i < b.len() && b[i] == '.' {
            i += 1;
            let start = i;
            let mut v: u64 = 0;
            while i < b.len() && b[i].is_ascii_digit() && i - start < 9 {
                v = v * 10 + b[i].to_digit(10).unwrap() as u64;
                i += 1;
            }
            if i == start {
                return None;
            }
            ts.frac = Some(v as u32);
            ts.frac_width = i - start;
        }
        if i < b.len() && (b[i] == 'Z' || b[i] == 'z') {
            ts.had_z = true;
            i += 1;
        } else if i < b.len() && (b[i] == '+' || b[i] == '-') {
            // Numeric UTC offset: +HH, +HH:MM, +HHMM.
            let start = i;
            i += 1;
            let mut digits = 0;
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == ':') {
                if b[i].is_ascii_digit() {
                    digits += 1;
                }
                i += 1;
            }
            if digits != 2 && digits != 4 {
                return None;
            }
            ts.offset = Some(b[start..i].iter().collect());
        }
        if i != b.len() {
            return None;
        }
        Some(ts)
    }

    pub fn to_epoch_seconds(&self) -> i64 {
        days_from_civil(self.year, self.month, self.day) * 86400
            + self.hour as i64 * 3600
            + self.minute as i64 * 60
            + self.second as i64
    }

    pub fn from_epoch_seconds(mut secs: i64, like: &Timestamp) -> Timestamp {
        let mut days = secs.div_euclid(86400);
        secs = secs.rem_euclid(86400);
        if secs < 0 {
            secs += 86400;
            days -= 1;
        }
        let (y, m, d) = civil_from_days(days);
        Timestamp {
            year: y,
            month: m,
            day: d,
            hour: (secs / 3600) as u32,
            minute: ((secs % 3600) / 60) as u32,
            second: (secs % 60) as u32,
            frac: like.frac,
            frac_width: like.frac_width,
            sep_t: like.sep_t,
            had_time: like.had_time,
            had_z: like.had_z,
            offset: like.offset.clone(),
        }
    }

    /// Render in the same shape it was parsed from (date-only stays date-only,
    /// T vs space separator, fraction width, Z suffix).
    pub fn render(&self) -> String {
        let mut s = format!("{:04}-{:02}-{:02}", self.year, self.month, self.day);
        if self.had_time {
            s.push(if self.sep_t { 'T' } else { ' ' });
            s.push_str(&format!("{:02}:{:02}:{:02}", self.hour, self.minute, self.second));
            if let Some(f) = self.frac {
                s.push_str(&format!(".{:0width$}", f, width = self.frac_width));
            }
            if self.had_z {
                s.push('Z');
            } else if let Some(o) = &self.offset {
                s.push_str(o);
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_null_vs_empty() {
        assert_eq!(Value::from_csv("", false, ColumnType::Text).unwrap(), Value::Null);
        assert_eq!(
            Value::from_csv("", true, ColumnType::Text).unwrap(),
            Value::Text(String::new())
        );
        assert_eq!(Value::Null.to_csv(), (String::new(), false));
        assert_eq!(Value::Text(String::new()).to_csv(), (String::new(), true));
    }

    #[test]
    fn typed_parse() {
        assert_eq!(Value::from_csv("42", false, ColumnType::Integer).unwrap(), Value::Int(42));
        assert_eq!(Value::from_csv("2.5", false, ColumnType::Float).unwrap(), Value::Float(2.5));
        assert_eq!(Value::from_csv("true", false, ColumnType::Boolean).unwrap(), Value::Bool(true));
        assert!(Value::from_csv("x", false, ColumnType::Integer).is_err());
    }

    #[test]
    fn canonical_unifies_int_float() {
        assert_eq!(Value::Int(5).canonical(), Value::Float(5.0).canonical());
        assert_ne!(Value::Int(5).canonical(), Value::Text("5".into()).canonical());
    }

    #[test]
    fn timestamp_round_trip() {
        for s in [
            "2024-02-29",
            "2024-01-15 10:30:00",
            "2024-01-15T10:30:00Z",
            "1969-12-31 23:59:59",
            "2024-01-15T10:30:00.123456",
            "2024-01-15 10:30:00+00",
            "2024-01-15 10:30:00.5-05:30",
        ] {
            let ts = Timestamp::parse(s).unwrap();
            assert_eq!(ts.render(), s, "round trip of {s}");
            let back = Timestamp::from_epoch_seconds(ts.to_epoch_seconds(), &ts);
            assert_eq!(back.render(), s, "epoch round trip of {s}");
        }
        assert!(Timestamp::parse("2023-02-29").is_none());
        assert!(Timestamp::parse("2024-13-01").is_none());
        assert!(Timestamp::parse("not a date").is_none());
    }

    #[test]
    fn epoch_known_values() {
        let ts = Timestamp::parse("1970-01-01").unwrap();
        assert_eq!(ts.to_epoch_seconds(), 0);
        let ts = Timestamp::parse("2001-09-09 01:46:40").unwrap();
        assert_eq!(ts.to_epoch_seconds(), 1_000_000_000);
    }
}
