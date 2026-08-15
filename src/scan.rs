//! Sensitivity scan (SPEC §9): detect likely PII columns from column names
//! and sampled values, and recommend generators.

use crate::config::Config;
use crate::connector::SourceConnector;
use crate::generators::{luhn_valid, FIRST_NAMES, GENERATOR_NAMES, LAST_NAMES};
use crate::json::Json;
use regex::Regex;
use crate::schema::Table;
use crate::value::{ColumnType, Value};
use crate::Result;
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    Possible,
    Likely,
    High,
}

impl Confidence {
    pub fn name(&self) -> &'static str {
        match self {
            Confidence::Possible => "possible",
            Confidence::Likely => "likely",
            Confidence::High => "high",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub table: String,
    pub column: String,
    pub pii_type: String,
    pub confidence: Confidence,
    pub recommended_generator: String,
    pub recommended_options: Json,
    /// Whether the workspace config already assigns a generator here.
    pub already_configured: bool,
}

pub struct ScanReport {
    pub findings: Vec<Finding>,
    pub sampled_rows: usize,
}

impl ScanReport {
    pub fn to_json(&self) -> Json {
        Json::Object(vec![
            ("sampled_rows".to_string(), Json::Int(self.sampled_rows as i64)),
            (
                "findings".to_string(),
                Json::Array(
                    self.findings
                        .iter()
                        .map(|f| {
                            Json::Object(vec![
                                ("table".to_string(), Json::Str(f.table.clone())),
                                ("column".to_string(), Json::Str(f.column.clone())),
                                ("pii_type".to_string(), Json::Str(f.pii_type.clone())),
                                ("confidence".to_string(), Json::Str(f.confidence.name().to_string())),
                                (
                                    "recommended_generator".to_string(),
                                    Json::Str(f.recommended_generator.clone()),
                                ),
                                ("recommended_options".to_string(), f.recommended_options.clone()),
                                ("already_configured".to_string(), Json::Bool(f.already_configured)),
                            ])
                        })
                        .collect(),
                ),
            ),
        ])
    }
}

/// A recommendation: (pii type, generator, options-json).
type Rec = (&'static str, &'static str, &'static str);

/// Column-name patterns, first match wins. Names are normalized to lowercase
/// alphanumerics before matching (camelCase and kebab-case fold in).
const NAME_RULES: &[(&str, Rec)] = &[
    ("email", ("email", "email", r#"{"consistency": "value"}"#)),
    ("firstname|givenname", ("first_name", "name", r#"{"part": "first", "link": "person"}"#)),
    ("lastname|surname|familyname", ("last_name", "name", r#"{"part": "last", "link": "person"}"#)),
    ("fullname|customername|contactname|username|ownername", ("full_name", "name", r#"{}"#)),
    ("ssn|socialsecurity", ("ssn", "ssn", r#"{"consistency": "value"}"#)),
    ("creditcard|cardnumber|ccnum|pan$", ("credit_card", "credit_card", r#"{"consistency": "value"}"#)),
    ("phone|mobile|fax", ("phone", "phone", r#"{"consistency": "value"}"#)),
    ("dob|dateofbirth|birthdate|birthday", ("date_of_birth", "date_truncation", r#"{"to": "year"}"#)),
    ("password|passwd|secret|apikey|token", ("credential", "random_hash", r#"{}"#)),
    ("ipaddress|ipaddr|^ip$|clientip|remoteip", ("ip_address", "ip_address", r#"{}"#)),
    ("macaddress|^mac$", ("mac_address", "mac_address", r#"{}"#)),
    ("street|address1|addressline|^address", ("street_address", "address", r#"{"part": "street", "link": "addr"}"#)),
    ("city|town", ("city", "address", r#"{"part": "city", "link": "addr"}"#)),
    ("zipcode|zip$|postalcode|postcode", ("zip", "address", r#"{"part": "zip", "link": "addr"}"#)),
    ("latitude|^lat$", ("latitude", "geo", r#"{"part": "lat", "link": "geo"}"#)),
    ("longitude|^lng$|^lon$", ("longitude", "geo", r#"{"part": "lon", "link": "geo"}"#)),
    ("company|employer|businessname|organization", ("business_name", "business_name", r#"{}"#)),
    ("hostname", ("hostname", "hostname", r#"{}"#)),
    ("url|website|homepage", ("url", "url", r#"{}"#)),
    ("filename|filepath|attachment", ("file_name", "file_name", r#"{}"#)),
    ("iban|accountnumber|routingnumber", ("bank_account", "numeric_string_key", r#"{"consistency": "value"}"#)),
    ("salary|income|wage", ("salary", "noise", r#"{"scale": 0.1}"#)),
];

struct ValueDetector {
    pii_type: &'static str,
    generator: &'static str,
    options: &'static str,
    regex: Regex,
    /// Extra semantic validation beyond the regex (e.g. Luhn).
    validate: Option<fn(&str) -> bool>,
}

fn luhn_text(s: &str) -> bool {
    let digits: Vec<u8> = s.chars().filter(|c| c.is_ascii_digit()).map(|c| c as u8 - b'0').collect();
    if !(13..=19).contains(&digits.len()) {
        return false;
    }
    luhn_valid(&digits)
}

fn value_detectors() -> Vec<ValueDetector> {
    let mk = |pii_type, generator, options, pattern: &str, validate| ValueDetector {
        pii_type,
        generator,
        options,
        regex: Regex::new(pattern).expect("built-in detector regex"),
        validate,
    };
    vec![
        mk(
            "email",
            "email",
            r#"{"consistency": "value"}"#,
            r"^[\w.+-]+@[\w-]+(\.[\w-]+)+$",
            None,
        ),
        mk("ssn", "ssn", r#"{"consistency": "value"}"#, r"^\d{3}-\d{2}-\d{4}$", None),
        mk(
            "credit_card",
            "credit_card",
            r#"{"consistency": "value"}"#,
            r"^[\d](?:[\d -]{11,17})[\d]$",
            Some(luhn_text),
        ),
        mk(
            "phone",
            "phone",
            r#"{"consistency": "value"}"#,
            r"^\+?1?[ .-]?(\(\d{3}\)|\d{3})[ .-]?\d{3}[ .-]?\d{4}$",
            None,
        ),
        mk(
            "ip_address",
            "ip_address",
            r#"{}"#,
            r"^(\d{1,3}\.){3}\d{1,3}$|^[0-9a-fA-F:]+::?[0-9a-fA-F:]+$",
            Some(|s| {
                !s.contains('.')
                    || s.split('.').all(|p| p.parse::<u32>().map(|v| v <= 255).unwrap_or(false))
            }),
        ),
        mk(
            "uuid",
            "uuid_key",
            r#"{"consistency": "value"}"#,
            r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$",
            None,
        ),
        mk("mac_address", "mac_address", r#"{}"#, r"^([0-9a-fA-F]{2}[:-]){5}[0-9a-fA-F]{2}$", None),
        mk("url", "url", r#"{}"#, r"^https?://[\w.-]+", None),
    ]
}

fn normalize(name: &str) -> String {
    // Lowercase and strip every separator, so first_name, FirstName,
    // first-name, and firstName all normalize to "firstname" — the form the
    // NAME_RULES patterns are written in.
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn name_match(normalized: &str, pattern: &str) -> bool {
    // Patterns are alternations of substrings, with optional ^/$ anchors.
    pattern.split('|').any(|p| {
        let (p, anchored_start) = match p.strip_prefix('^') {
            Some(rest) => (rest, true),
            None => (p, false),
        };
        let (p, anchored_end) = match p.strip_suffix('$') {
            Some(rest) => (rest, true),
            None => (p, false),
        };
        match (anchored_start, anchored_end) {
            (true, true) => normalized == p,
            (true, false) => normalized.starts_with(p),
            (false, true) => normalized.ends_with(p),
            (false, false) => normalized.contains(p),
        }
    })
}

/// Scan a source dataset. `config` (optional) marks findings already covered.
/// `sample` bounds how many rows per table are inspected for value signals.
pub fn scan(
    src: &dyn SourceConnector,
    config: Option<&Config>,
    sample: usize,
) -> Result<ScanReport> {
    let schema = src.schema();
    let detectors = value_detectors();
    let mut findings = Vec::new();
    let mut sampled_total = 0usize;

    for t in &schema.tables {
        // Value sampling: collect up to `sample` rows once per table.
        let mut samples: Vec<Vec<Value>> = Vec::new();
        let mut rows = src.read_table(&t.name)?;
        while samples.len() < sample {
            match rows.next_row()? {
                Some(r) => samples.push(r),
                None => break,
            }
        }
        sampled_total += samples.len();

        for (ci, col) in t.columns.iter().enumerate() {
            let normalized = normalize(&col.name);
            let mut finding: Option<(String, Confidence, String, Json)> = None;

            // 1. Name heuristics (high confidence).
            for (pattern, (pii, gener, opts)) in NAME_RULES {
                if name_match(&normalized, pattern) {
                    // Type sanity: don't recommend text generators on numbers.
                    if type_compatible(gener, col.ty) {
                        finding = Some((
                            pii.to_string(),
                            Confidence::High,
                            gener.to_string(),
                            Json::parse(opts).expect("built-in options"),
                        ));
                    }
                    break;
                }
            }

            // 2. Value heuristics (likely confidence) when the name said nothing.
            if finding.is_none() && col.ty == ColumnType::Text {
                let values: Vec<&str> = samples
                    .iter()
                    .filter_map(|r| r[ci].as_text())
                    .filter(|s| !s.is_empty())
                    .collect();
                if values.len() >= 3 {
                    for d in &detectors {
                        let hits = values
                            .iter()
                            .filter(|v| {
                                d.regex.is_match(v) && d.validate.map(|f| f(v)).unwrap_or(true)
                            })
                            .count();
                        if hits * 10 >= values.len() * 8 {
                            // >= 80% of sampled values match
                            finding = Some((
                                d.pii_type.to_string(),
                                Confidence::Likely,
                                d.generator.to_string(),
                                Json::parse(d.options).expect("built-in options"),
                            ));
                            break;
                        }
                    }
                    // Person-name dictionary check.
                    if finding.is_none() {
                        let first: HashSet<&str> = FIRST_NAMES.iter().copied().collect();
                        let last: HashSet<&str> = LAST_NAMES.iter().copied().collect();
                        let hits = values
                            .iter()
                            .filter(|v| {
                                let mut parts = v.split_whitespace();
                                match (parts.next(), parts.next(), parts.next()) {
                                    (Some(f), Some(l), None) => {
                                        first.contains(f) && last.contains(l)
                                    }
                                    (Some(single), None, None) => {
                                        first.contains(single) || last.contains(single)
                                    }
                                    _ => false,
                                }
                            })
                            .count();
                        if hits * 10 >= values.len() * 8 {
                            finding = Some((
                                "person_name".to_string(),
                                Confidence::Likely,
                                "name".to_string(),
                                Json::Object(Vec::new()),
                            ));
                        }
                    }
                }
            }

            if let Some((pii_type, confidence, generator, options)) = finding {
                debug_assert!(GENERATOR_NAMES.contains(&generator.as_str()));
                let already_configured = config
                    .map(|c| c.column_config(&t.name, &col.name).is_some())
                    .unwrap_or(false);
                findings.push(Finding {
                    table: t.name.clone(),
                    column: col.name.clone(),
                    pii_type,
                    confidence,
                    recommended_generator: generator,
                    recommended_options: options,
                    already_configured,
                });
            }
        }
    }
    Ok(ScanReport { findings, sampled_rows: sampled_total })
}

fn type_compatible(generator: &str, ty: ColumnType) -> bool {
    match generator {
        "noise" => ty.is_numeric(),
        "geo" => ty.is_numeric() || ty == ColumnType::Text,
        "date_truncation" => ty.is_temporal() || ty.is_textual(),
        "integer_key" => ty == ColumnType::Integer,
        _ => ty == ColumnType::Text || ty.is_textual(),
    }
}

/// Merge scan recommendations into a config: adds generator assignments for
/// findings on columns that are not yet configured. Existing entries are
/// never overwritten.
pub fn apply_to_config(report: &ScanReport, config: &mut Config) -> usize {
    let mut applied = 0;
    for f in &report.findings {
        if config.column_config(&f.table, &f.column).is_some() {
            continue;
        }
        let tc = config.tables.entry(f.table.clone()).or_insert_with(|| {
            config.table_order.push(f.table.clone());
            Default::default()
        });
        tc.column_order.push(f.column.clone());
        tc.columns.insert(
            f.column.clone(),
            crate::config::ColumnConfig {
                generator: f.recommended_generator.clone(),
                options: f.recommended_options.clone(),
            },
        );
        applied += 1;
    }
    applied
}

/// Strict-mode check for CI (SPEC §12): unresolved likely/high findings.
pub fn unresolved(report: &ScanReport) -> Vec<&Finding> {
    report
        .findings
        .iter()
        .filter(|f| !f.already_configured && f.confidence >= Confidence::Likely)
        .collect()
}

/// Scan a table definition without data (used by `altonic scan` on empty
/// tables and by tests).
pub fn scan_names_only(t: &Table) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for col in &t.columns {
        let normalized = normalize(&col.name);
        for (pattern, (pii, _, _)) in NAME_RULES {
            if name_match(&normalized, pattern) {
                out.push((col.name.clone(), pii.to_string()));
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::DirDataset;

    fn demo(dirname: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("altonic-scan-{dirname}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("schema.json"),
            r#"{
              "tables": [
                {"name": "users", "columns": [
                  {"name": "id", "type": "integer"},
                  {"name": "emailAddress", "type": "text"},
                  {"name": "contact", "type": "text"},
                  {"name": "notes", "type": "text"},
                  {"name": "salary", "type": "float"},
                  {"name": "cc", "type": "text"}],
                 "primary_key": ["id"]}
              ]
            }"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("users.csv"),
            "id,emailAddress,contact,notes,salary,cc\n\
             1,a@x.com,555-123-4567,hello world,100.0,4539578763621486\n\
             2,b@y.org,555-987-6543,plain text,90.0,4539578763621486\n\
             3,c@z.net,555-222-3333,more notes,80.0,4539578763621486\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn name_and_value_detection() {
        let dir = demo("basic");
        let ds = DirDataset::open(&dir).unwrap();
        let report = scan(&ds, None, 100).unwrap();
        let by_col: std::collections::HashMap<&str, &Finding> =
            report.findings.iter().map(|f| (f.column.as_str(), f)).collect();

        // Name rule: emailAddress (camelCase folds).
        let email = by_col["emailAddress"];
        assert_eq!(email.pii_type, "email");
        assert_eq!(email.confidence, Confidence::High);

        // Value rule: contact column contains phone-shaped values.
        let contact = by_col["contact"];
        assert_eq!(contact.pii_type, "phone");
        assert_eq!(contact.confidence, Confidence::Likely);

        // Value rule with Luhn: cc column.
        let cc = by_col["cc"];
        assert_eq!(cc.pii_type, "credit_card");

        // Name rule with numeric type: salary → noise.
        assert_eq!(by_col["salary"].recommended_generator, "noise");

        // Free text not flagged.
        assert!(!by_col.contains_key("notes"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_merges_without_overwriting() {
        let dir = demo("apply");
        let ds = DirDataset::open(&dir).unwrap();
        let mut config = Config::parse(
            r#"{"secret": "s", "tables": {"users": {"columns": {
                "emailAddress": {"generator": "null"}
            }}}}"#,
        )
        .unwrap();
        let report = scan(&ds, Some(&config), 100).unwrap();
        let n = apply_to_config(&report, &mut config);
        assert!(n >= 2); // contact, cc, salary at least
        // Existing assignment untouched.
        assert_eq!(config.column_config("users", "emailAddress").unwrap().generator, "null");
        assert_eq!(config.column_config("users", "contact").unwrap().generator, "phone");
        // Round-trips as valid config JSON.
        let text = config.to_json().to_string_pretty();
        Config::parse(&text).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unresolved_for_strict_mode() {
        let dir = demo("strict");
        let ds = DirDataset::open(&dir).unwrap();
        let report = scan(&ds, None, 100).unwrap();
        assert!(!unresolved(&report).is_empty());
        let mut config = Config::parse(r#"{"secret": "s"}"#).unwrap();
        apply_to_config(&report, &mut config);
        let report2 = scan(&ds, Some(&config), 100).unwrap();
        assert!(unresolved(&report2).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn normalization() {
        assert_eq!(normalize("emailAddress"), "emailaddress");
        assert_eq!(normalize("First-Name"), "firstname");
        assert_eq!(normalize("SSN"), "ssn");
        assert!(name_match("emailaddress", "email"));
        assert!(name_match(&normalize("first_name"), "firstname|givenname"));
        assert!(name_match("ip", "^ip$"));
        assert!(!name_match("description", "^ip$"));
        assert!(!name_match("shipment", "^ip$"));
    }
}
