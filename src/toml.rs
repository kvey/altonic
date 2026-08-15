//! TOML front-end for the config format.
//!
//! Documents are read into and written from the crate's `Json` tree, so
//! everything downstream of config loading is format-agnostic. The parsing and
//! rendering are the `toml` crate's; what lives here is the mapping to `Json`
//! and the diagnostics for the two shapes TOML cannot express (null, and
//! arrays that mix tables with scalars).

use crate::json::Json;
use crate::{verr, Result};

pub fn parse(input: &str) -> Result<Json> {
    let value: Json =
        ::toml::from_str(input).map_err(|e| verr(format!("toml: {}", first_line(&e.to_string()))))?;
    Ok(value)
}

/// Render a `Json` tree (object root) as TOML. Fails on values TOML cannot
/// express (null, arrays mixing tables and scalars).
pub fn to_toml(root: &Json) -> Result<String> {
    if root.as_object().is_none() {
        return Err(verr("toml: top-level value must be a table"));
    }
    check_expressible(root, &mut Vec::new())?;
    ::toml::to_string_pretty(root).map_err(|e| verr(format!("toml: {e}")))
}

/// Report the two unrepresentable shapes against the path they occur at,
/// which is far more useful than the serializer's own message.
fn check_expressible(value: &Json, path: &mut Vec<String>) -> Result<()> {
    let where_ = || {
        if path.is_empty() {
            "at the top level".to_string()
        } else {
            format!("at {}", path.join("."))
        }
    };
    match value {
        Json::Null => Err(verr(format!("toml: cannot express null ({})", where_()))),
        Json::Float(f) if !f.is_finite() => {
            Err(verr(format!("toml: cannot express non-finite float ({})", where_())))
        }
        Json::Array(items) => {
            let tables = items.iter().filter(|i| matches!(i, Json::Object(_))).count();
            if tables != 0 && tables != items.len() {
                return Err(verr(format!(
                    "toml: array {} mixes tables and scalars; cannot be expressed",
                    where_()
                )));
            }
            for item in items {
                check_expressible(item, path)?;
            }
            Ok(())
        }
        Json::Object(members) => {
            for (k, v) in members {
                path.push(k.clone());
                check_expressible(v, path)?;
                path.pop();
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// `toml` errors carry a multi-line source excerpt; configs report errors on
/// one line, so keep the summary.
fn first_line(message: &str) -> String {
    message.lines().next().unwrap_or(message).trim().to_string()
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
            "a = \"unterminated",
            "= 3",
        ] {
            assert!(parse(bad).is_err(), "should reject: {bad}");
        }
        // Mixed arrays are legal TOML 1.0 and we accept them.
        assert!(parse("a = [1, {x = 2}]").is_ok());
        // Errors stay on one line so config diagnostics read cleanly.
        assert!(!format!("{}", parse("a = ").unwrap_err()).contains('\n'));
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

    #[test]
    fn reports_inexpressible_values_with_a_path() {
        let mut v = parse("[a.b]\nc = 1\n").unwrap();
        v.get_mut("a").unwrap().get_mut("b").unwrap().set("c", Json::Null);
        let msg = format!("{}", to_toml(&v).unwrap_err());
        assert!(msg.contains("null"), "{msg}");
        assert!(msg.contains("a.b.c"), "{msg}");
    }
}
