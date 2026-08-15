//! TOML parser and serializer (the subset of TOML 1.0 a workspace config
//! needs), built from first principles. Parses into the crate's `Json` value
//! tree so config handling is format-agnostic downstream.
//!
//! Supported: bare/quoted/dotted keys, `[table]` and `[[array-of-tables]]`
//! headers, basic and literal strings (single- and multi-line), integers
//! (with `_` separators), floats, booleans, arrays (multi-line, trailing
//! comma), inline tables, comments. Not supported (rejected with clear
//! errors): datetimes, hex/octal/binary integers, and `+inf`/`nan`.

use crate::json::Json;
use crate::{verr, Error, Result};

pub fn parse(input: &str) -> Result<Json> {
    let mut p = Parser { chars: input.chars().collect(), pos: 0, line: 1 };
    let mut root = Json::Object(Vec::new());
    // Paths already used by [table] headers or key-values, to reject
    // redefinition. Array-of-tables paths may repeat by design.
    let mut defined: Vec<Vec<String>> = Vec::new();
    let mut current_path: Vec<PathSeg> = Vec::new();

    loop {
        p.skip_trivia();
        let Some(c) = p.peek() else { break };
        if c == '[' {
            p.bump();
            let array_of_tables = p.peek() == Some('[');
            if array_of_tables {
                p.bump();
            }
            p.skip_ws();
            let key = p.key_path()?;
            p.skip_ws();
            p.expect(']')?;
            if array_of_tables {
                p.expect(']')?;
            }
            p.end_of_line()?;

            let mut path: Vec<PathSeg> =
                key.iter().map(|k| PathSeg::Key(k.clone())).collect();
            if array_of_tables {
                // Append a fresh element to the array at this path.
                push_array_element(&mut root, &path, &p)?;
                path.push(PathSeg::LastElement);
            } else {
                let plain: Vec<String> = key.clone();
                if defined.contains(&plain) {
                    return Err(p.err(&format!("table [{}] defined twice", key.join("."))));
                }
                defined.push(plain);
                // Ensure the table exists (possibly inside array-of-tables parents).
                ensure_object(&mut root, &path, &p)?;
            }
            current_path = path;
        } else {
            let key = p.key_path()?;
            p.skip_ws();
            p.expect('=')?;
            p.skip_ws();
            let value = p.value(0)?;
            p.end_of_line()?;
            let mut path = current_path.clone();
            for k in &key[..key.len() - 1] {
                path.push(PathSeg::Key(k.clone()));
            }
            let obj = ensure_object(&mut root, &path, &p)?;
            let last = key.last().unwrap().clone();
            if obj.get(&last).is_some() {
                return Err(p.err(&format!("duplicate key {last:?}")));
            }
            obj.set(&last, value);
        }
    }
    Ok(root)
}

#[derive(Debug, Clone)]
enum PathSeg {
    Key(String),
    /// Inside the most recent element of an array of tables.
    LastElement,
}

fn ensure_object<'a>(root: &'a mut Json, path: &[PathSeg], p: &Parser) -> Result<&'a mut Json> {
    let mut node = root;
    for seg in path {
        match seg {
            PathSeg::Key(k) => {
                let is_object = {
                    if node.get(k).is_none() {
                        if let Json::Object(members) = node {
                            members.push((k.clone(), Json::Object(Vec::new())));
                        } else {
                            return Err(p.err(&format!("key {k:?} used inside a non-table value")));
                        }
                    }
                    matches!(node.get(k), Some(Json::Object(_)) | Some(Json::Array(_)))
                };
                if !is_object {
                    return Err(p.err(&format!("key {k:?} already holds a non-table value")));
                }
                node = match node {
                    Json::Object(members) => {
                        &mut members.iter_mut().find(|(mk, _)| mk == k).unwrap().1
                    }
                    _ => unreachable!(),
                };
            }
            PathSeg::LastElement => {
                node = match node {
                    Json::Array(items) => items
                        .last_mut()
                        .ok_or_else(|| p.err("internal: empty array of tables"))?,
                    _ => return Err(p.err("expected an array of tables")),
                };
            }
        }
    }
    match node {
        Json::Object(_) => Ok(node),
        _ => Err(p.err("path resolves to a non-table value")),
    }
}

fn push_array_element(root: &mut Json, path: &[PathSeg], p: &Parser) -> Result<()> {
    let (last, parents) = path.split_last().ok_or_else(|| p.err("empty table header"))?;
    let PathSeg::Key(k) = last else { return Err(p.err("bad array-of-tables header")) };
    let parent = ensure_object(root, parents, p)?;
    if parent.get(k).is_none() {
        parent.set(k, Json::Array(Vec::new()));
    }
    match parent.get_mut_inner(k) {
        Some(Json::Array(items)) => {
            items.push(Json::Object(Vec::new()));
            Ok(())
        }
        _ => Err(p.err(&format!("[[{k}]] conflicts with an existing non-array value"))),
    }
}

// Small private extension used by the TOML builder.
impl Json {
    fn get_mut_inner(&mut self, key: &str) -> Option<&mut Json> {
        match self {
            Json::Object(members) => {
                members.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
            }
            _ => None,
        }
    }
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
    line: usize,
}

const MAX_DEPTH: usize = 64;

impl Parser {
    fn err(&self, msg: &str) -> Error {
        verr(format!("toml: {msg} at line {}", self.line))
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c == Some('\n') {
            self.line += 1;
        }
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn expect(&mut self, c: char) -> Result<()> {
        if self.bump() == Some(c) {
            Ok(())
        } else {
            Err(self.err(&format!("expected {c:?}")))
        }
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.pos += 1;
        }
    }

    /// Skip whitespace, comments, and newlines (between top-level items).
    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(' ' | '\t' | '\r') => {
                    self.pos += 1;
                }
                Some('\n') => {
                    self.bump();
                }
                Some('#') => {
                    while !matches!(self.peek(), None | Some('\n')) {
                        self.pos += 1;
                    }
                }
                _ => return,
            }
        }
    }

    /// After a value or header: optional ws + comment, then newline or EOF.
    fn end_of_line(&mut self) -> Result<()> {
        self.skip_ws();
        if self.peek() == Some('#') {
            while !matches!(self.peek(), None | Some('\n')) {
                self.pos += 1;
            }
        }
        match self.peek() {
            None => Ok(()),
            Some('\n') => {
                self.bump();
                Ok(())
            }
            Some('\r') if self.chars.get(self.pos + 1) == Some(&'\n') => {
                self.pos += 1;
                self.bump();
                Ok(())
            }
            Some(c) => Err(self.err(&format!("unexpected {c:?} after value"))),
        }
    }

    fn key_path(&mut self) -> Result<Vec<String>> {
        let mut parts = vec![self.key_part()?];
        loop {
            self.skip_ws();
            if self.peek() == Some('.') {
                self.pos += 1;
                self.skip_ws();
                parts.push(self.key_part()?);
            } else {
                return Ok(parts);
            }
        }
    }

    fn key_part(&mut self) -> Result<String> {
        match self.peek() {
            Some('"') => self.basic_string(false),
            Some('\'') => self.literal_string(false),
            Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '-' => {
                let mut s = String::new();
                while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
                    s.push(self.bump().unwrap());
                }
                Ok(s)
            }
            _ => Err(self.err("expected a key")),
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json> {
        if depth > MAX_DEPTH {
            return Err(self.err("nesting too deep"));
        }
        match self.peek() {
            Some('"') => {
                if self.lookahead("\"\"\"") {
                    self.basic_string(true).map(Json::Str)
                } else {
                    self.basic_string(false).map(Json::Str)
                }
            }
            Some('\'') => {
                if self.lookahead("'''") {
                    self.literal_string(true).map(Json::Str)
                } else {
                    self.literal_string(false).map(Json::Str)
                }
            }
            Some('[') => self.array(depth),
            Some('{') => self.inline_table(depth),
            Some('t') | Some('f') => self.boolean(),
            Some(c) if c == '+' || c == '-' || c.is_ascii_digit() => self.number(),
            Some(c) => Err(self.err(&format!("unexpected {c:?} in value position"))),
            None => Err(self.err("unexpected end of input")),
        }
    }

    fn lookahead(&self, s: &str) -> bool {
        self.chars[self.pos..].starts_with(&s.chars().collect::<Vec<_>>()[..])
    }

    fn boolean(&mut self) -> Result<Json> {
        if self.lookahead("true") {
            self.pos += 4;
            Ok(Json::Bool(true))
        } else if self.lookahead("false") {
            self.pos += 5;
            Ok(Json::Bool(false))
        } else {
            Err(self.err("expected true or false"))
        }
    }

    fn basic_string(&mut self, multiline: bool) -> Result<String> {
        let delim = if multiline { 3 } else { 1 };
        for _ in 0..delim {
            self.expect('"')?;
        }
        let mut s = String::new();
        if multiline && self.peek() == Some('\n') {
            self.bump(); // trim newline right after opening delimiter
        }
        loop {
            if multiline && self.lookahead("\"\"\"") {
                self.pos += 3;
                return Ok(s);
            }
            match self.bump() {
                None => return Err(self.err("unterminated string")),
                Some('"') if !multiline => return Ok(s),
                Some('\n') if !multiline => return Err(self.err("newline in single-line string")),
                Some('\\') => match self.bump() {
                    Some('b') => s.push('\u{08}'),
                    Some('t') => s.push('\t'),
                    Some('n') => s.push('\n'),
                    Some('f') => s.push('\u{0c}'),
                    Some('r') => s.push('\r'),
                    Some('"') => s.push('"'),
                    Some('\\') => s.push('\\'),
                    Some('u') => s.push(self.unicode_escape(4)?),
                    Some('U') => s.push(self.unicode_escape(8)?),
                    Some('\n') if multiline => {
                        // Line-ending backslash: skip following whitespace.
                        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
                            self.bump();
                        }
                    }
                    _ => return Err(self.err("invalid escape")),
                },
                Some(c) => s.push(c),
            }
        }
    }

    fn unicode_escape(&mut self, len: usize) -> Result<char> {
        let mut v: u32 = 0;
        for _ in 0..len {
            let c = self.bump().ok_or_else(|| self.err("truncated unicode escape"))?;
            v = v * 16 + c.to_digit(16).ok_or_else(|| self.err("invalid hex digit"))?;
        }
        char::from_u32(v).ok_or_else(|| self.err("invalid unicode codepoint"))
    }

    fn literal_string(&mut self, multiline: bool) -> Result<String> {
        let delim = if multiline { 3 } else { 1 };
        for _ in 0..delim {
            self.expect('\'')?;
        }
        let mut s = String::new();
        if multiline && self.peek() == Some('\n') {
            self.bump();
        }
        loop {
            if multiline && self.lookahead("'''") {
                self.pos += 3;
                return Ok(s);
            }
            match self.bump() {
                None => return Err(self.err("unterminated string")),
                Some('\'') if !multiline => return Ok(s),
                Some('\n') if !multiline => return Err(self.err("newline in single-line string")),
                Some(c) => s.push(c),
            }
        }
    }

    fn number(&mut self) -> Result<Json> {
        let start = self.pos;
        if matches!(self.peek(), Some('+' | '-')) {
            self.pos += 1;
        }
        let mut is_float = false;
        let mut digits = String::new();
        if matches!(self.peek(), Some('+' | '-')) {
            return Err(self.err("malformed number"));
        }
        while let Some(c) = self.peek() {
            match c {
                '0'..='9' => {
                    digits.push(c);
                    self.pos += 1;
                }
                '_' => {
                    self.pos += 1; // separator; validity checked loosely
                }
                '.' | 'e' | 'E' => {
                    is_float = true;
                    digits.push(c);
                    self.pos += 1;
                    if matches!(self.peek(), Some('+' | '-')) && (c == 'e' || c == 'E') {
                        digits.push(self.bump().unwrap());
                    }
                }
                '-' | ':' => {
                    return Err(self.err(
                        "datetime values are not supported; quote them as strings",
                    ))
                }
                'x' | 'o' | 'b' if self.pos == start + 1 => {
                    return Err(self.err("hex/octal/binary integers are not supported"))
                }
                _ => break,
            }
        }
        if digits.is_empty() {
            return Err(self.err("malformed number"));
        }
        let neg = self.chars[start] == '-';
        let text = format!("{}{digits}", if neg { "-" } else { "" });
        if is_float {
            text.parse::<f64>()
                .map(Json::Float)
                .map_err(|_| self.err(&format!("bad float {text:?}")))
        } else {
            text.parse::<i64>()
                .map(Json::Int)
                .map_err(|_| self.err(&format!("integer out of range: {text:?}")))
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json> {
        self.expect('[')?;
        let mut items = Vec::new();
        loop {
            self.skip_trivia();
            if self.peek() == Some(']') {
                self.pos += 1;
                return Ok(Json::Array(items));
            }
            items.push(self.value(depth + 1)?);
            self.skip_trivia();
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                }
                Some(']') => {}
                _ => return Err(self.err("expected ',' or ']' in array")),
            }
        }
    }

    fn inline_table(&mut self, depth: usize) -> Result<Json> {
        self.expect('{')?;
        let mut obj = Json::Object(Vec::new());
        self.skip_trivia();
        if self.peek() == Some('}') {
            self.pos += 1;
            return Ok(obj);
        }
        loop {
            self.skip_trivia();
            let key = self.key_path()?;
            self.skip_ws();
            self.expect('=')?;
            self.skip_ws();
            let value = self.value(depth + 1)?;
            // Dotted keys inside inline tables create nested objects.
            let mut node = &mut obj;
            for k in &key[..key.len() - 1] {
                if node.get(k).is_none() {
                    node.set(k, Json::Object(Vec::new()));
                }
                node = node.get_mut_inner(k).unwrap();
            }
            let last = key.last().unwrap();
            if node.get(last).is_some() {
                return Err(self.err(&format!("duplicate key {last:?} in inline table")));
            }
            node.set(last, value);
            self.skip_trivia();
            match self.bump() {
                Some(',') => continue,
                Some('}') => return Ok(obj),
                _ => return Err(self.err("expected ',' or '}' in inline table")),
            }
        }
    }
}

// ---------- Serializer ----------

/// Render a `Json` tree (object root) as TOML. Fails on values TOML cannot
/// express (null, arrays mixing tables and scalars).
pub fn to_toml(root: &Json) -> Result<String> {
    let Json::Object(_) = root else {
        return Err(verr("toml: top-level value must be a table"));
    };
    let mut out = String::new();
    write_table(root, &mut Vec::new(), &mut out)?;
    Ok(out)
}

fn is_scalar_ish(v: &Json) -> bool {
    match v {
        Json::Object(_) => false,
        Json::Array(items) => items.iter().all(is_scalar_ish),
        _ => true,
    }
}

fn is_array_of_tables(v: &Json) -> bool {
    matches!(v, Json::Array(items) if !items.is_empty() && items.iter().all(|i| matches!(i, Json::Object(_))))
}

fn write_table(obj: &Json, path: &mut Vec<String>, out: &mut String) -> Result<()> {
    let members = obj.as_object().unwrap();
    // 1. Scalar-ish members belong to the current header.
    for (k, v) in members {
        if is_scalar_ish(v) {
            out.push_str(&format!("{} = {}\n", key_str(k), value_str(v)?));
        }
    }
    // 2. Arrays of tables.
    for (k, v) in members {
        if is_array_of_tables(v) {
            let items = v.as_array().unwrap();
            path.push(k.clone());
            for item in items {
                out.push_str(&format!("\n[[{}]]\n", path_str(path)));
                write_table(item, path, out)?;
            }
            path.pop();
        } else if matches!(v, Json::Array(_)) && !is_scalar_ish(v) {
            return Err(verr(format!(
                "toml: array {k:?} mixes tables and scalars; cannot be expressed"
            )));
        }
    }
    // 3. Sub-tables.
    for (k, v) in members {
        if let Json::Object(_) = v {
            path.push(k.clone());
            out.push_str(&format!("\n[{}]\n", path_str(path)));
            write_table(v, path, out)?;
            path.pop();
        }
    }
    Ok(())
}

fn key_str(k: &str) -> String {
    let bare = !k.is_empty()
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare {
        k.to_string()
    } else {
        value_str(&Json::Str(k.to_string())).unwrap()
    }
}

fn path_str(path: &[String]) -> String {
    path.iter().map(|p| key_str(p)).collect::<Vec<_>>().join(".")
}

fn value_str(v: &Json) -> Result<String> {
    Ok(match v {
        Json::Null => return Err(verr("toml: cannot express null")),
        Json::Bool(b) => b.to_string(),
        Json::Int(i) => i.to_string(),
        Json::Float(f) => {
            if !f.is_finite() {
                return Err(verr("toml: cannot express non-finite float"));
            }
            if f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{:.1}", f)
            } else {
                format!("{f}")
            }
        }
        Json::Str(s) => {
            let mut out = String::with_capacity(s.len() + 2);
            out.push('"');
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
                    c => out.push(c),
                }
            }
            out.push('"');
            out
        }
        Json::Array(items) => {
            let parts: Vec<String> = items.iter().map(value_str).collect::<Result<_>>()?;
            format!("[{}]", parts.join(", "))
        }
        Json::Object(members) => {
            let parts: Vec<String> = members
                .iter()
                .map(|(k, v)| Ok(format!("{} = {}", key_str(k), value_str(v)?)))
                .collect::<Result<_>>()?;
            format!("{{ {} }}", parts.join(", "))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_document() {
        let doc = r#"
# workspace
name = "demo"
seed = 42
ratio = 0.5
flag = true
list = [1, 2, 3]

[tables.customers]
mode = "de_identify"

[tables.customers.columns.email]
generator = "email"
options = { consistency = "value" }

[[subset.targets]]
table = "orders"
percent = 10.0

[[subset.targets]]
table = "customers"
where = "state = 'CA'"
"#;
        let v = parse(doc).unwrap();
        assert_eq!(v.get("name").unwrap().as_str(), Some("demo"));
        assert_eq!(v.get("seed").unwrap().as_i64(), Some(42));
        assert_eq!(v.get("ratio").unwrap().as_f64(), Some(0.5));
        assert_eq!(v.get("list").unwrap().as_array().unwrap().len(), 3);
        let email = v
            .get("tables").unwrap()
            .get("customers").unwrap()
            .get("columns").unwrap()
            .get("email").unwrap();
        assert_eq!(email.get("generator").unwrap().as_str(), Some("email"));
        assert_eq!(
            email.get("options").unwrap().get("consistency").unwrap().as_str(),
            Some("value")
        );
        let targets = v.get("subset").unwrap().get("targets").unwrap().as_array().unwrap();
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[1].get("where").unwrap().as_str(), Some("state = 'CA'"));
    }

    #[test]
    fn strings() {
        let doc = r#"
a = "esc \t \n \" \\ é"
b = 'literal \n kept'
c = """
multi
line"""
d = '''lit '' multi'''
"#;
        let v = parse(doc).unwrap();
        assert_eq!(v.get("a").unwrap().as_str(), Some("esc \t \n \" \\ é"));
        assert_eq!(v.get("b").unwrap().as_str(), Some("literal \\n kept"));
        assert_eq!(v.get("c").unwrap().as_str(), Some("multi\nline"));
        assert_eq!(v.get("d").unwrap().as_str(), Some("lit '' multi"));
    }

    #[test]
    fn quoted_and_dotted_keys() {
        let doc = r#"
[paths]
"user.email" = 1
"tags[*]" = 2
a.b.c = 3
"#;
        let v = parse(doc).unwrap();
        let p = v.get("paths").unwrap();
        assert_eq!(p.get("user.email").unwrap().as_i64(), Some(1));
        assert_eq!(p.get("tags[*]").unwrap().as_i64(), Some(2));
        assert_eq!(
            p.get("a").unwrap().get("b").unwrap().get("c").unwrap().as_i64(),
            Some(3)
        );
    }

    #[test]
    fn errors() {
        for bad in [
            "a = ",
            "a = 1\na = 2",              // duplicate key
            "[t]\n[t]",                  // duplicate table
            "a = 2024-01-01",            // datetime
            "a = 0x10",                  // hex
            "a = \"unterminated",
            "a = [1, {x = 2}]",          // fine to parse actually? mixed array — TOML 1.0 allows; we accept
            "= 3",
        ] {
            let r = parse(bad);
            if bad == "a = [1, {x = 2}]" {
                assert!(r.is_ok());
            } else {
                assert!(r.is_err(), "should reject: {bad}");
            }
        }
    }

    #[test]
    fn round_trip_config_shape() {
        let doc = r#"
name = "demo"
secret = "s"

[tables.customers.columns.email]
generator = "email"
options = { consistency = "value" }

[tables.audit_log]
mode = "truncate"

[subset]
enabled = true
lookup_tables = ["states"]

[[subset.targets]]
table = "orders"
percent = 10.0
"#;
        let v = parse(doc).unwrap();
        let rendered = to_toml(&v).unwrap();
        let v2 = parse(&rendered).unwrap();
        assert_eq!(v, v2, "TOML round-trip changed the tree:\n{rendered}");
    }

    #[test]
    fn serializer_quotes_keys() {
        let v = parse("[m]\n\"user.email\" = { generator = \"null\" }\n").unwrap();
        let rendered = to_toml(&v).unwrap();
        assert!(rendered.contains("\"user.email\""), "{rendered}");
        assert_eq!(parse(&rendered).unwrap(), v);
    }
}
