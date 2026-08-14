//! Minimal JSON (RFC 8259) parser and serializer.
//!
//! Objects preserve member insertion order (Vec-backed) so reports and
//! rewritten configs are stable. Numbers are kept as i64 when exactly
//! integral, f64 otherwise.

use std::fmt::Write as _;

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
        let mut p = Parser { chars: input.chars().collect(), pos: 0 };
        p.skip_ws();
        let v = p.value(0)?;
        p.skip_ws();
        if p.pos != p.chars.len() {
            return Err(p.err("trailing content after JSON value"));
        }
        Ok(v)
    }

    pub fn to_string_pretty(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, Some(0));
        out.push('\n');
        out
    }

    pub fn to_string_compact(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, None);
        out
    }

    fn write(&self, out: &mut String, indent: Option<usize>) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Int(i) => {
                let _ = write!(out, "{i}");
            }
            Json::Float(f) => {
                if f.is_finite() {
                    if f.fract() == 0.0 && f.abs() < 1e15 {
                        let _ = write!(out, "{:.1}", f);
                    } else {
                        let _ = write!(out, "{f}");
                    }
                } else {
                    out.push_str("null"); // JSON has no NaN/Inf
                }
            }
            Json::Str(s) => write_escaped(out, s),
            Json::Array(items) => {
                if items.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    if let Some(level) = indent {
                        newline_indent(out, level + 1);
                        item.write(out, Some(level + 1));
                    } else {
                        item.write(out, None);
                    }
                }
                if let Some(level) = indent {
                    newline_indent(out, level);
                }
                out.push(']');
            }
            Json::Object(members) => {
                if members.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push('{');
                for (i, (k, v)) in members.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    if let Some(level) = indent {
                        newline_indent(out, level + 1);
                        write_escaped(out, k);
                        out.push_str(": ");
                        v.write(out, Some(level + 1));
                    } else {
                        write_escaped(out, k);
                        out.push(':');
                        v.write(out, None);
                    }
                }
                if let Some(level) = indent {
                    newline_indent(out, level);
                }
                out.push('}');
            }
        }
    }
}

fn newline_indent(out: &mut String, level: usize) {
    out.push('\n');
    for _ in 0..level {
        out.push_str("  ");
    }
}

fn write_escaped(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

const MAX_DEPTH: usize = 128;

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn err(&self, msg: &str) -> String {
        // Report 1-based line:col of the current position.
        let mut line = 1;
        let mut col = 1;
        for &c in self.chars.iter().take(self.pos) {
            if c == '\n' {
                line += 1;
                col = 1;
            } else {
                col += 1;
            }
        }
        format!("{msg} at line {line}, column {col}")
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, c: char) -> Result<(), String> {
        if self.bump() == Some(c) {
            Ok(())
        } else {
            self.pos = self.pos.saturating_sub(1);
            Err(self.err(&format!("expected '{c}'")))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > MAX_DEPTH {
            return Err(self.err("nesting too deep"));
        }
        match self.peek() {
            Some('{') => self.object(depth),
            Some('[') => self.array(depth),
            Some('"') => Ok(Json::Str(self.string()?)),
            Some('t') => self.literal("true", Json::Bool(true)),
            Some('f') => self.literal("false", Json::Bool(false)),
            Some('n') => self.literal("null", Json::Null),
            Some(c) if c == '-' || c.is_ascii_digit() => self.number(),
            Some(_) => Err(self.err("unexpected character")),
            None => Err(self.err("unexpected end of input")),
        }
    }

    fn literal(&mut self, word: &str, val: Json) -> Result<Json, String> {
        for expected in word.chars() {
            if self.bump() != Some(expected) {
                return Err(self.err(&format!("invalid literal, expected '{word}'")));
            }
        }
        Ok(val)
    }

    fn object(&mut self, depth: usize) -> Result<Json, String> {
        self.expect('{')?;
        let mut members = Vec::new();
        self.skip_ws();
        if self.peek() == Some('}') {
            self.pos += 1;
            return Ok(Json::Object(members));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.expect(':')?;
            self.skip_ws();
            let val = self.value(depth + 1)?;
            members.push((key, val));
            self.skip_ws();
            match self.bump() {
                Some(',') => continue,
                Some('}') => return Ok(Json::Object(members)),
                _ => {
                    self.pos = self.pos.saturating_sub(1);
                    return Err(self.err("expected ',' or '}' in object"));
                }
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, String> {
        self.expect('[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(']') {
            self.pos += 1;
            return Ok(Json::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.bump() {
                Some(',') => continue,
                Some(']') => return Ok(Json::Array(items)),
                _ => {
                    self.pos = self.pos.saturating_sub(1);
                    return Err(self.err("expected ',' or ']' in array"));
                }
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect('"')?;
        let mut s = String::new();
        loop {
            match self.bump() {
                None => return Err(self.err("unterminated string")),
                Some('"') => return Ok(s),
                Some('\\') => match self.bump() {
                    Some('"') => s.push('"'),
                    Some('\\') => s.push('\\'),
                    Some('/') => s.push('/'),
                    Some('b') => s.push('\u{08}'),
                    Some('f') => s.push('\u{0c}'),
                    Some('n') => s.push('\n'),
                    Some('r') => s.push('\r'),
                    Some('t') => s.push('\t'),
                    Some('u') => {
                        let hi = self.hex4()?;
                        let cp = if (0xd800..0xdc00).contains(&hi) {
                            // Surrogate pair
                            if self.bump() != Some('\\') || self.bump() != Some('u') {
                                return Err(self.err("unpaired surrogate"));
                            }
                            let lo = self.hex4()?;
                            if !(0xdc00..0xe000).contains(&lo) {
                                return Err(self.err("invalid low surrogate"));
                            }
                            0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
                        } else if (0xdc00..0xe000).contains(&hi) {
                            return Err(self.err("unpaired low surrogate"));
                        } else {
                            hi
                        };
                        s.push(char::from_u32(cp).ok_or_else(|| self.err("invalid codepoint"))?);
                    }
                    _ => return Err(self.err("invalid escape")),
                },
                Some(c) if (c as u32) < 0x20 => return Err(self.err("control character in string")),
                Some(c) => s.push(c),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let mut v = 0u32;
        for _ in 0..4 {
            let c = self.bump().ok_or_else(|| self.err("truncated \\u escape"))?;
            let d = c.to_digit(16).ok_or_else(|| self.err("invalid hex digit"))?;
            v = v * 16 + d;
        }
        Ok(v)
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        if self.peek() == Some('-') {
            self.pos += 1;
        }
        // int part
        match self.peek() {
            Some('0') => {
                self.pos += 1;
            }
            Some(c) if c.is_ascii_digit() => {
                while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    self.pos += 1;
                }
            }
            _ => return Err(self.err("invalid number")),
        }
        let mut is_float = false;
        if self.peek() == Some('.') {
            is_float = true;
            self.pos += 1;
            if !matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                return Err(self.err("digit required after decimal point"));
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some('+' | '-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                return Err(self.err("digit required in exponent"));
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        if !is_float {
            if let Ok(i) = text.parse::<i64>() {
                return Ok(Json::Int(i));
            }
        }
        text.parse::<f64>().map(Json::Float).map_err(|_| self.err("invalid number"))
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
    }
}
