//! Composite generators (SPEC §6): regex_mask, json_mask, conditional.
//! These descend into structured cell values and delegate fragments to
//! sub-generators.
//!
//! Sub-generator seeding: a consistent sub-generator is seeded from the
//! fragment it is rewriting — HMAC(secret, family ‖ fragment) — so equal
//! fragments map equally everywhere, including outside composites. A
//! non-consistent sub-generator derives its stream from the parent cell's RNG.

use super::{BuiltGenerator, CellCtx, Generator, Opts};
use crate::crypto;
use crate::expr::Expr;
use crate::json::Json;
use crate::regex::Regex;
use crate::rng::Rng;
use crate::schema::{Column, Table};
use crate::value::Value;
use crate::{verr, Result};

fn run_sub(
    sub: &BuiltGenerator,
    fragment: &Value,
    ctx: &CellCtx,
    parent_rng: &mut Rng,
) -> Result<Value> {
    let mut rng = if sub.consistent {
        let digest = crypto::hmac_parts(
            ctx.secret,
            &[sub.family.as_bytes(), fragment.canonical().as_bytes()],
        );
        Rng::from_digest(&digest)
    } else {
        Rng::from_seed(parent_rng.next_u64())
    };
    sub.gen.generate(fragment, ctx, &mut rng)
}

fn check_sub(sub: &BuiltGenerator, parent: &str) -> Result<()> {
    if sub.gen.needs_stats() {
        return Err(verr(format!(
            "generator {parent:?}: sub-generator {:?} needs an analysis pass and cannot be nested",
            sub.family
        )));
    }
    Ok(())
}

// ---- regex_mask ----

struct RegexMask {
    regex: Regex,
    /// (capture group index, sub-generator)
    groups: Vec<(usize, BuiltGenerator)>,
}

impl Generator for RegexMask {
    fn generate(&self, input: &Value, ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        let s = match input {
            Value::Null => return Ok(Value::Null),
            Value::Text(s) => s.clone(),
            other => other.to_string(),
        };
        let chars: Vec<char> = s.chars().collect();
        let matches = self.regex.find_iter_chars(&chars);
        if matches.is_empty() {
            return Ok(Value::Text(s));
        }
        // Collect replacement spans (start, end, new text), then splice.
        let mut spans: Vec<(usize, usize, String)> = Vec::new();
        for m in &matches {
            let mut claimed: Vec<(usize, usize)> = Vec::new();
            for (gi, sub) in &self.groups {
                if let Some(Some((gs, ge))) = m.groups.get(*gi) {
                    // Skip zero-width and overlapping (nested) group spans.
                    if gs >= ge || claimed.iter().any(|(cs, ce)| gs < ce && cs < ge) {
                        continue;
                    }
                    let fragment: String = chars[*gs..*ge].iter().collect();
                    let out = run_sub(sub, &Value::Text(fragment), ctx, rng)?;
                    spans.push((*gs, *ge, out.to_string()));
                    claimed.push((*gs, *ge));
                }
            }
        }
        spans.sort_by_key(|(s, _, _)| *s);
        let mut out = String::new();
        let mut pos = 0;
        for (gs, ge, text) in spans {
            out.extend(chars[pos..gs].iter());
            out.push_str(&text);
            pos = ge;
        }
        out.extend(chars[pos..].iter());
        Ok(Value::Text(out))
    }
}

pub fn regex_mask(opts: &Opts, table: &Table, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&["pattern", "groups"])?;
    let pattern = opts
        .str("pattern")?
        .ok_or_else(|| verr("generator \"regex_mask\": option \"pattern\" is required"))?;
    let regex = Regex::new(pattern)?;
    if regex.group_count == 0 {
        return Err(verr("generator \"regex_mask\": pattern must have at least one capture group"));
    }
    let groups_json = opts
        .get("groups")
        .and_then(|g| g.as_object())
        .ok_or_else(|| verr("generator \"regex_mask\": option \"groups\" (object) is required"))?;
    let mut groups = Vec::new();
    for (k, spec) in groups_json {
        let gi: usize = k
            .parse()
            .map_err(|_| verr(format!("generator \"regex_mask\": group key {k:?} must be a number")))?;
        if gi == 0 || gi > regex.group_count {
            return Err(verr(format!(
                "generator \"regex_mask\": group {gi} out of range (pattern has {} groups)",
                regex.group_count
            )));
        }
        let sub = super::build_sub(spec, table, column)?;
        check_sub(&sub, "regex_mask")?;
        groups.push((gi, sub));
    }
    if groups.is_empty() {
        return Err(verr("generator \"regex_mask\": \"groups\" must configure at least one group"));
    }
    Ok(Box::new(RegexMask { regex, groups }))
}

// ---- json_mask ----

#[derive(Debug, Clone, PartialEq)]
enum PathSeg {
    Key(String),
    Index(usize),
    AnyIndex,
}

fn parse_path(path: &str) -> Result<Vec<PathSeg>> {
    let mut segs = Vec::new();
    for part in path.split('.') {
        if part.is_empty() {
            return Err(verr(format!("json_mask: empty path segment in {path:?}")));
        }
        // key, key[*], key[3], possibly chained: a[0][1]
        let mut rest = part;
        let key_end = rest.find('[').unwrap_or(rest.len());
        let key = &rest[..key_end];
        if !key.is_empty() {
            segs.push(PathSeg::Key(key.to_string()));
        }
        rest = &rest[key_end..];
        while let Some(stripped) = rest.strip_prefix('[') {
            let close = stripped
                .find(']')
                .ok_or_else(|| verr(format!("json_mask: unclosed '[' in path {path:?}")))?;
            let idx = &stripped[..close];
            if idx == "*" {
                segs.push(PathSeg::AnyIndex);
            } else {
                segs.push(PathSeg::Index(idx.parse().map_err(|_| {
                    verr(format!("json_mask: bad array index {idx:?} in path {path:?}"))
                })?));
            }
            rest = &stripped[close + 1..];
        }
        if !rest.is_empty() {
            return Err(verr(format!("json_mask: malformed path segment {part:?}")));
        }
    }
    if segs.is_empty() {
        return Err(verr("json_mask: empty path"));
    }
    Ok(segs)
}

struct JsonMask {
    paths: Vec<(Vec<PathSeg>, BuiltGenerator)>,
}

fn json_leaf_to_value(j: &Json) -> Value {
    match j {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Int(i) => Value::Int(*i),
        Json::Float(f) => Value::Float(*f),
        Json::Str(s) => Value::Text(s.clone()),
        other => Value::Text(other.to_string_compact()),
    }
}

fn value_to_json_leaf(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(i) => Json::Int(*i),
        Value::Float(f) => Json::Float(*f),
        Value::Text(s) => Json::Str(s.clone()),
    }
}

fn apply_path(
    node: &mut Json,
    segs: &[PathSeg],
    sub: &BuiltGenerator,
    ctx: &CellCtx,
    rng: &mut Rng,
) -> Result<()> {
    if segs.is_empty() {
        let fragment = json_leaf_to_value(node);
        let out = run_sub(sub, &fragment, ctx, rng)?;
        *node = value_to_json_leaf(&out);
        return Ok(());
    }
    match (&segs[0], node) {
        (PathSeg::Key(k), Json::Object(members)) => {
            for (mk, mv) in members.iter_mut() {
                if mk == k {
                    apply_path(mv, &segs[1..], sub, ctx, rng)?;
                }
            }
        }
        (PathSeg::Index(i), Json::Array(items)) => {
            if let Some(item) = items.get_mut(*i) {
                apply_path(item, &segs[1..], sub, ctx, rng)?;
            }
        }
        (PathSeg::AnyIndex, Json::Array(items)) => {
            for item in items.iter_mut() {
                apply_path(item, &segs[1..], sub, ctx, rng)?;
            }
        }
        // Path does not apply to this shape: silently skip (missing keys are
        // not an error, mirroring Tonic's JSON mask behavior).
        _ => {}
    }
    Ok(())
}

impl Generator for JsonMask {
    fn generate(&self, input: &Value, ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        let s = match input {
            Value::Null => return Ok(Value::Null),
            Value::Text(s) => s.clone(),
            other => return Err(verr(format!("json_mask: expected JSON text, got {other:?}"))),
        };
        let mut root = Json::parse(&s).map_err(|e| verr(format!("json_mask: cell is not valid JSON: {e}")))?;
        for (segs, sub) in &self.paths {
            apply_path(&mut root, segs, sub, ctx, rng)?;
        }
        Ok(Value::Text(root.to_string_compact()))
    }
}

pub fn json_mask(opts: &Opts, table: &Table, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&["paths"])?;
    let paths_json = opts
        .get("paths")
        .and_then(|p| p.as_object())
        .ok_or_else(|| verr("generator \"json_mask\": option \"paths\" (object) is required"))?;
    if paths_json.is_empty() {
        return Err(verr("generator \"json_mask\": \"paths\" must not be empty"));
    }
    let mut paths = Vec::new();
    for (path, spec) in paths_json {
        let segs = parse_path(path)?;
        let sub = super::build_sub(spec, table, column)?;
        check_sub(&sub, "json_mask")?;
        paths.push((segs, sub));
    }
    Ok(Box::new(JsonMask { paths }))
}

// ---- conditional ----

struct Conditional {
    cases: Vec<(Expr, BuiltGenerator)>,
    default: Option<BuiltGenerator>,
}

impl Generator for Conditional {
    fn generate(&self, input: &Value, ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        for (when, sub) in &self.cases {
            if when.eval_bool(&|name| ctx.col_value(name))? {
                return run_sub(sub, input, ctx, rng);
            }
        }
        match &self.default {
            Some(sub) => run_sub(sub, input, ctx, rng),
            None => Ok(input.clone()),
        }
    }
}

pub fn conditional(opts: &Opts, table: &Table, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&["cases", "default"])?;
    let cases_json = opts
        .get("cases")
        .and_then(|c| c.as_array())
        .ok_or_else(|| verr("generator \"conditional\": option \"cases\" (array) is required"))?;
    if cases_json.is_empty() {
        return Err(verr("generator \"conditional\": \"cases\" must not be empty"));
    }
    let mut cases = Vec::new();
    for case in cases_json {
        let when_src = case
            .get("when")
            .and_then(|w| w.as_str())
            .ok_or_else(|| verr("generator \"conditional\": each case needs a \"when\" expression"))?;
        let when = Expr::parse(when_src)?;
        let mut cols = Vec::new();
        when.columns(&mut cols);
        for c in &cols {
            if table.column(c).is_none() {
                return Err(verr(format!(
                    "generator \"conditional\": when-expression references unknown column {c:?}"
                )));
            }
        }
        let sub = super::build_sub(case, table, column)?;
        check_sub(&sub, "conditional")?;
        cases.push((when, sub));
    }
    let default = match opts.get("default") {
        None => None,
        Some(spec) => {
            let sub = super::build_sub(spec, table, column)?;
            check_sub(&sub, "conditional")?;
            Some(sub)
        }
    };
    Ok(Box::new(Conditional { cases, default }))
}

#[cfg(test)]
mod tests {
    use super::super::test_util::*;
    use crate::value::Value;

    #[test]
    fn regex_mask_replaces_groups_only() {
        let v = quick(
            "regex_mask",
            r#"{
              "pattern": "user=(\\w+); pass=(\\w+)",
              "groups": {
                "1": {"generator": "character_scramble", "options": {"consistency": "value"}},
                "2": {"generator": "constant", "options": {"value": "XXX"}}
              }
            }"#,
            "user=alice; pass=hunter2 (untouched)",
        );
        let s = v.as_text().unwrap();
        assert!(s.starts_with("user="));
        assert!(s.contains("; pass=XXX (untouched)"), "{s}");
        assert!(!s.contains("alice"));
        // Fragment consistency: same input again → same masked user.
        let v2 = quick(
            "regex_mask",
            r#"{
              "pattern": "user=(\\w+); pass=(\\w+)",
              "groups": {
                "1": {"generator": "character_scramble", "options": {"consistency": "value"}},
                "2": {"generator": "constant", "options": {"value": "XXX"}}
              }
            }"#,
            "user=alice; pass=hunter2 (untouched)",
        );
        assert_eq!(v, v2);
    }

    #[test]
    fn regex_mask_multiple_matches() {
        let v = quick(
            "regex_mask",
            r#"{"pattern": "(\\d{3})-\\d{4}", "groups": {"1": {"generator": "constant", "options": {"value": "000"}}}}"#,
            "call 555-1234 or 866-9999",
        );
        assert_eq!(v.as_text().unwrap(), "call 000-1234 or 000-9999");
    }

    #[test]
    fn json_mask_paths() {
        let v = quick(
            "json_mask",
            r#"{"paths": {
                "user.email": {"generator": "constant", "options": {"value": "masked@example.com"}},
                "tags[*]": {"generator": "constant", "options": {"value": "tag"}},
                "items[0].price": {"generator": "constant", "options": {"value": 0}}
            }}"#,
            r#"{"user": {"email": "real@corp.com", "name": "keep"}, "tags": ["a", "b"], "items": [{"price": 42}, {"price": 7}]}"#,
        );
        let s = v.as_text().unwrap();
        let parsed = crate::json::Json::parse(s).unwrap();
        assert_eq!(
            parsed.get("user").unwrap().get("email").unwrap().as_str(),
            Some("masked@example.com")
        );
        assert_eq!(parsed.get("user").unwrap().get("name").unwrap().as_str(), Some("keep"));
        let tags = parsed.get("tags").unwrap().as_array().unwrap();
        assert!(tags.iter().all(|t| t.as_str() == Some("tag")));
        let items = parsed.get("items").unwrap().as_array().unwrap();
        assert_eq!(items[0].get("price").unwrap().as_i64(), Some(0));
        assert_eq!(items[1].get("price").unwrap().as_i64(), Some(7));
    }

    #[test]
    fn json_mask_missing_path_is_noop() {
        let v = quick(
            "json_mask",
            r#"{"paths": {"missing.key": {"generator": "constant", "options": {"value": 1}}}}"#,
            r#"{"present": true}"#,
        );
        assert_eq!(v.as_text().unwrap(), r#"{"present":true}"#);
    }

    #[test]
    fn conditional_picks_case() {
        let table = test_table();
        let c = col(&table, "c");
        let opts = crate::json::Json::parse(
            r#"{
              "cases": [
                {"when": "n > 10", "generator": "constant", "options": {"value": "BIG"}},
                {"when": "n > 0", "generator": "constant", "options": {"value": "SMALL"}}
              ],
              "default": {"generator": "constant", "options": {"value": "NONE"}}
            }"#,
        )
        .unwrap();
        let b = super::super::build("conditional", &opts, &table, &c).unwrap();
        let mut row = vec![Value::Null; 7];
        row[1] = Value::Int(50);
        assert_eq!(run_gen(&b, Value::Text("x".into()), &table, row.clone()), Value::Text("BIG".into()));
        row[1] = Value::Int(5);
        assert_eq!(run_gen(&b, Value::Text("x".into()), &table, row.clone()), Value::Text("SMALL".into()));
        row[1] = Value::Int(-1);
        assert_eq!(run_gen(&b, Value::Text("x".into()), &table, row), Value::Text("NONE".into()));
    }

    #[test]
    fn validation_errors() {
        let table = test_table();
        let c = col(&table, "c");
        // No capture group
        let opts = crate::json::Json::parse(
            r#"{"pattern": "abc", "groups": {"1": {"generator": "null"}}}"#,
        )
        .unwrap();
        assert!(super::super::build("regex_mask", &opts, &table, &c).is_err());
        // Group out of range
        let opts = crate::json::Json::parse(
            r#"{"pattern": "(a)", "groups": {"2": {"generator": "null"}}}"#,
        )
        .unwrap();
        assert!(super::super::build("regex_mask", &opts, &table, &c).is_err());
        // Stats-needing sub-generator
        let opts = crate::json::Json::parse(
            r#"{"pattern": "(a)", "groups": {"1": {"generator": "categorical"}}}"#,
        )
        .unwrap();
        assert!(super::super::build("regex_mask", &opts, &table, &c).is_err());
        // Conditional referencing unknown column
        let opts = crate::json::Json::parse(
            r#"{"cases": [{"when": "ghost > 1", "generator": "null"}]}"#,
        )
        .unwrap();
        assert!(super::super::build("conditional", &opts, &table, &c).is_err());
    }
}
