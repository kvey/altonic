//! `altonic check`: evaluate a cleaning config against the detected source
//! schema (file dataset or live database) and report everything at once —
//! schema drift, invalid generator settings, referential-integrity
//! conflicts, per-column coverage, unprotected PII, and subset validity.
//!
//! Unlike `generate` (which fails fast on the first problem), the checker
//! collects every issue so a config can be reviewed and fixed in one pass.

use crate::config::{Config, TableMode};
use crate::connector::SourceConnector;
use crate::engine::{fk_parent_map, mapped_root_set, resolve_fk_root};
use crate::generators;
use crate::json::Json;
use crate::scan;
use crate::subset;
use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// `generate` would refuse to run.
    Error,
    /// Runs, but probably not what you want (e.g. PII passing through).
    Warning,
}

#[derive(Debug, Clone)]
pub struct Issue {
    pub severity: Severity,
    /// Stable machine-readable code, e.g. "drift.table_missing".
    pub code: &'static str,
    pub table: String,
    pub column: Option<String>,
    pub message: String,
}

/// How a column will be treated by `generate` with this config.
#[derive(Debug, Clone, PartialEq)]
pub enum Coverage {
    /// Explicit generator applies.
    Masked { generator: String },
    /// FK column inheriting the key mapping of a masked referenced column.
    InheritedKey { root: String },
    /// Copied verbatim.
    Passthrough,
    /// Configured but invalid — see the matching Issue.
    Invalid,
    /// Table mode makes column config irrelevant (truncate / preserve).
    NotApplicable,
}

#[derive(Debug, Clone)]
pub struct ColumnReport {
    pub table: String,
    pub column: String,
    pub coverage: Coverage,
    /// PII detection result for this column, if any.
    pub pii: Option<(String, &'static str)>, // (type, confidence name)
}

pub struct CheckReport {
    pub issues: Vec<Issue>,
    pub columns: Vec<ColumnReport>,
    pub sampled_rows: usize,
}

impl CheckReport {
    pub fn errors(&self) -> usize {
        self.issues.iter().filter(|i| i.severity == Severity::Error).count()
    }

    pub fn warnings(&self) -> usize {
        self.issues.iter().filter(|i| i.severity == Severity::Warning).count()
    }

    pub fn to_json(&self) -> Json {
        let issues = self
            .issues
            .iter()
            .map(|i| {
                let mut m = vec![
                    (
                        "severity".to_string(),
                        Json::Str(match i.severity {
                            Severity::Error => "error".to_string(),
                            Severity::Warning => "warning".to_string(),
                        }),
                    ),
                    ("code".to_string(), Json::Str(i.code.to_string())),
                    ("table".to_string(), Json::Str(i.table.clone())),
                ];
                if let Some(c) = &i.column {
                    m.push(("column".to_string(), Json::Str(c.clone())));
                }
                m.push(("message".to_string(), Json::Str(i.message.clone())));
                Json::Object(m)
            })
            .collect();
        let columns = self
            .columns
            .iter()
            .map(|c| {
                let coverage = match &c.coverage {
                    Coverage::Masked { generator } => format!("masked:{generator}"),
                    Coverage::InheritedKey { root } => format!("inherited_key:{root}"),
                    Coverage::Passthrough => "passthrough".to_string(),
                    Coverage::Invalid => "invalid".to_string(),
                    Coverage::NotApplicable => "not_applicable".to_string(),
                };
                let mut m = vec![
                    ("table".to_string(), Json::Str(c.table.clone())),
                    ("column".to_string(), Json::Str(c.column.clone())),
                    ("coverage".to_string(), Json::Str(coverage)),
                ];
                if let Some((ty, conf)) = &c.pii {
                    m.push(("pii_type".to_string(), Json::Str(ty.clone())));
                    m.push(("pii_confidence".to_string(), Json::Str(conf.to_string())));
                }
                Json::Object(m)
            })
            .collect();
        Json::Object(vec![
            ("errors".to_string(), Json::Int(self.errors() as i64)),
            ("warnings".to_string(), Json::Int(self.warnings() as i64)),
            ("sampled_rows".to_string(), Json::Int(self.sampled_rows as i64)),
            ("issues".to_string(), Json::Array(issues)),
            ("columns".to_string(), Json::Array(columns)),
        ])
    }
}

/// Run the evaluation. `sample` bounds PII scanning (0 disables the scan).
pub fn check(src: &dyn SourceConnector, config: &Config, sample: usize) -> Result<CheckReport> {
    let schema = src.schema();
    let mut issues: Vec<Issue> = Vec::new();
    let mut columns: Vec<ColumnReport> = Vec::new();

    let error = |issues: &mut Vec<Issue>, code, table: &str, column: Option<&str>, message: String| {
        issues.push(Issue {
            severity: Severity::Error,
            code,
            table: table.to_string(),
            column: column.map(str::to_string),
            message,
        });
    };
    let warn = |issues: &mut Vec<Issue>, code, table: &str, column: Option<&str>, message: String| {
        issues.push(Issue {
            severity: Severity::Warning,
            code,
            table: table.to_string(),
            column: column.map(str::to_string),
            message,
        });
    };

    // ---- 1. Drift: config entries the database no longer has ----
    for tname in &config.table_order {
        match schema.table(tname) {
            None => error(
                &mut issues,
                "drift.table_missing",
                tname,
                None,
                format!("config table {tname:?} does not exist in the source"),
            ),
            Some(t) => {
                for cname in &config.tables[tname].column_order {
                    if t.column(cname).is_none() {
                        error(
                            &mut issues,
                            "drift.column_missing",
                            tname,
                            Some(cname),
                            format!("config column {tname:?}.{cname:?} does not exist in the source"),
                        );
                    }
                }
            }
        }
    }

    // ---- 2. FK analysis over valid entries ----
    let fk_parent = fk_parent_map(schema);
    let mapped_roots = match mapped_root_set(schema, config, &fk_parent) {
        Ok(r) => r,
        Err(e) => {
            // FK cycle: report and continue with an empty root set.
            error(&mut issues, "fk.cycle", "", None, e.to_string());
            Default::default()
        }
    };

    // ---- 3. PII scan (against source data) ----
    let scan_report = if sample > 0 {
        Some(scan::scan(src, Some(config), sample)?)
    } else {
        None
    };
    let pii_for = |table: &str, column: &str| -> Option<(String, &'static str)> {
        scan_report.as_ref().and_then(|r| {
            r.findings
                .iter()
                .find(|f| f.table == table && f.column == column)
                .map(|f| (f.pii_type.clone(), f.confidence.name()))
        })
    };

    // ---- 4. Per-column coverage over the DETECTED schema ----
    for t in &schema.tables {
        let mode = config.mode_for(&t.name);
        let table_config = config.table(&t.name);

        if mode != TableMode::DeIdentify {
            // Column configs under truncate/preserve are dead weight.
            if let Some(tc) = table_config {
                if !tc.columns.is_empty() {
                    warn(
                        &mut issues,
                        "mode.ignored_columns",
                        &t.name,
                        None,
                        format!(
                            "table {:?} has mode {:?}; its {} column generator(s) are ignored",
                            t.name,
                            mode.name(),
                            tc.columns.len()
                        ),
                    );
                }
            }
            for col in &t.columns {
                columns.push(ColumnReport {
                    table: t.name.clone(),
                    column: col.name.clone(),
                    coverage: Coverage::NotApplicable,
                    pii: pii_for(&t.name, &col.name),
                });
            }
            continue;
        }

        for col in &t.columns {
            let key = (t.name.clone(), col.name.clone());
            let explicit = config.column_config(&t.name, &col.name);
            let pii = pii_for(&t.name, &col.name);
            let is_fk_child = fk_parent.contains_key(&key);

            let coverage = if config.preserve_referential_integrity && is_fk_child {
                let root = resolve_fk_root(&fk_parent, fk_parent[&key].clone())
                    .unwrap_or_else(|_| key.clone());
                let root_name = format!("{}.{}", root.0, root.1);
                if mapped_roots.contains(&root) {
                    if explicit.is_some() {
                        error(
                            &mut issues,
                            "fk.conflicting_generator",
                            &t.name,
                            Some(&col.name),
                            format!(
                                "{}.{} is a foreign key to masked {root_name}; remove its generator — it inherits the key mapping automatically",
                                t.name, col.name
                            ),
                        );
                        Coverage::Invalid
                    } else {
                        Coverage::InheritedKey { root: root_name }
                    }
                } else if explicit.is_some() {
                    error(
                        &mut issues,
                        "fk.would_break_join",
                        &t.name,
                        Some(&col.name),
                        format!(
                            "{}.{} is a foreign key to unmasked {root_name}; configure the generator on {root_name} instead",
                            t.name, col.name
                        ),
                    );
                    Coverage::Invalid
                } else {
                    Coverage::Passthrough
                }
            } else {
                match explicit {
                    None => Coverage::Passthrough,
                    Some(cc) => match generators::build(&cc.generator, &cc.options, t, col) {
                        Err(e) => {
                            error(
                                &mut issues,
                                "generator.invalid",
                                &t.name,
                                Some(&col.name),
                                e.to_string().replace("validation error: ", ""),
                            );
                            Coverage::Invalid
                        }
                        Ok(built) => {
                            // Key-column restriction is the more specific
                            // diagnosis, so it wins over uniqueness.
                            if mapped_roots.contains(&key) && built.gen.needs_stats() {
                                error(
                                    &mut issues,
                                    "generator.stats_on_key",
                                    &t.name,
                                    Some(&col.name),
                                    format!(
                                        "generator {:?} needs an analysis pass and cannot be used on FK-referenced column {}.{}",
                                        cc.generator, t.name, col.name
                                    ),
                                );
                                Coverage::Invalid
                            } else if col.unique
                                && cc.generator != "passthrough"
                                && !built.gen.supports_uniqueness()
                            {
                                error(
                                    &mut issues,
                                    "generator.uniqueness",
                                    &t.name,
                                    Some(&col.name),
                                    format!(
                                        "column {}.{} is unique but generator {:?} cannot guarantee unique output",
                                        t.name, col.name, cc.generator
                                    ),
                                );
                                Coverage::Invalid
                            } else {
                                Coverage::Masked { generator: cc.generator.clone() }
                            }
                        }
                    },
                }
            };

            // Unprotected PII: detected, but nothing masks it.
            if matches!(coverage, Coverage::Passthrough) {
                if let Some((ty, conf)) = &pii {
                    warn(
                        &mut issues,
                        "pii.unprotected",
                        &t.name,
                        Some(&col.name),
                        format!(
                            "{}.{} looks like {ty} ({conf} confidence) but passes through unmasked",
                            t.name, col.name
                        ),
                    );
                }
            }

            columns.push(ColumnReport {
                table: t.name.clone(),
                column: col.name.clone(),
                coverage,
                pii,
            });
        }
    }

    // ---- 5. Subset section vs schema ----
    if let Some(sc) = &config.subset {
        if let Err(e) = subset::validate(sc, schema) {
            error(
                &mut issues,
                "subset.invalid",
                "",
                None,
                e.to_string().replace("validation error: ", ""),
            );
        } else {
            for target in &sc.targets {
                if config.mode_for(&target.table) != TableMode::DeIdentify {
                    warn(
                        &mut issues,
                        "subset.target_mode",
                        &target.table,
                        None,
                        format!(
                            "subset target {:?} has mode {:?}; its rows will not be copied anyway",
                            target.table,
                            config.mode_for(&target.table).name()
                        ),
                    );
                }
            }
        }
    }

    Ok(CheckReport {
        issues,
        columns,
        sampled_rows: scan_report.map(|r| r.sampled_rows).unwrap_or(0),
    })
}

/// Summarize coverage per table: (masked, inherited, passthrough, invalid, n/a, total).
pub fn table_summary(report: &CheckReport, table: &str) -> (usize, usize, usize, usize, usize, usize) {
    let mut m = (0, 0, 0, 0, 0, 0);
    for c in report.columns.iter().filter(|c| c.table == table) {
        m.5 += 1;
        match c.coverage {
            Coverage::Masked { .. } => m.0 += 1,
            Coverage::InheritedKey { .. } => m.1 += 1,
            Coverage::Passthrough => m.2 += 1,
            Coverage::Invalid => m.3 += 1,
            Coverage::NotApplicable => m.4 += 1,
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::DirDataset;
    use std::path::PathBuf;

    fn demo(dirname: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("altonic-check-{dirname}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("schema.json"),
            r#"{
              "tables": [
                {"name": "customers", "columns": [
                  {"name": "id", "type": "integer"},
                  {"name": "email", "type": "text", "unique": true},
                  {"name": "notes", "type": "text"}],
                 "primary_key": ["id"]},
                {"name": "orders", "columns": [
                  {"name": "id", "type": "integer"},
                  {"name": "customer_id", "type": "integer", "nullable": false}],
                 "primary_key": ["id"],
                 "foreign_keys": [
                   {"columns": ["customer_id"], "references_table": "customers", "references_columns": ["id"]}]}
              ]
            }"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("customers.csv"),
            "id,email,notes\n1,a@x.com,hi\n2,b@y.com,yo\n3,c@z.com,ok\n",
        )
        .unwrap();
        std::fs::write(dir.join("orders.csv"), "id,customer_id\n10,1\n").unwrap();
        dir
    }

    #[test]
    fn clean_config_checks_out() {
        let dir = demo("clean");
        let ds = DirDataset::open(&dir).unwrap();
        let config = Config::parse(
            r#"
secret = "s"
[tables.customers.columns.id]
generator = "integer_key"
options = { consistency = "value" }
[tables.customers.columns.email]
generator = "email"
options = { consistency = "value" }
"#,
        )
        .unwrap();
        let r = check(&ds, &config, 100).unwrap();
        assert_eq!(r.errors(), 0, "{:?}", r.issues);
        assert_eq!(r.warnings(), 0, "{:?}", r.issues);
        // customers: id + email masked, notes passthrough.
        assert_eq!(table_summary(&r, "customers"), (2, 0, 1, 0, 0, 3));
        // orders: customer_id inherits key map, id passthrough.
        assert_eq!(table_summary(&r, "orders"), (0, 1, 1, 0, 0, 2));
        let cid = r
            .columns
            .iter()
            .find(|c| c.table == "orders" && c.column == "customer_id")
            .unwrap();
        assert_eq!(cid.coverage, Coverage::InheritedKey { root: "customers.id".into() });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collects_all_problems_at_once() {
        let dir = demo("problems");
        let ds = DirDataset::open(&dir).unwrap();
        let config = Config::parse(
            r#"
secret = "s"
[tables.ghost_table.columns.x]
generator = "null"
[tables.customers.columns.ghost_column]
generator = "null"
[tables.customers.columns.email]
generator = "categorical"       # unique column, categorical can't do unique
[tables.customers.columns.id]
generator = "categorical"       # FK-referenced key + needs stats
[tables.orders.columns.customer_id]
generator = "integer_key"       # conflicts with inherited mapping
"#,
        )
        .unwrap();
        let r = check(&ds, &config, 100).unwrap();
        let codes: Vec<&str> = r.issues.iter().map(|i| i.code).collect();
        assert!(codes.contains(&"drift.table_missing"), "{codes:?}");
        assert!(codes.contains(&"drift.column_missing"), "{codes:?}");
        assert!(codes.contains(&"generator.uniqueness"), "{codes:?}");
        assert!(codes.contains(&"generator.stats_on_key"), "{codes:?}");
        assert!(codes.contains(&"fk.conflicting_generator"), "{codes:?}");
        assert!(r.errors() >= 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flags_unprotected_pii() {
        let dir = demo("pii");
        let ds = DirDataset::open(&dir).unwrap();
        let config = Config::parse(r#"secret = "s""#).unwrap();
        let r = check(&ds, &config, 100).unwrap();
        assert!(r
            .issues
            .iter()
            .any(|i| i.code == "pii.unprotected" && i.column.as_deref() == Some("email")));
        // Masking it clears the warning.
        let config2 = Config::parse(
            r#"
secret = "s"
[tables.customers.columns.email]
generator = "email"
"#,
        )
        .unwrap();
        let r2 = check(&ds, &config2, 100).unwrap();
        assert!(!r2.issues.iter().any(|i| i.code == "pii.unprotected"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mode_and_subset_warnings() {
        let dir = demo("modes");
        let ds = DirDataset::open(&dir).unwrap();
        let config = Config::parse(
            r#"
secret = "s"
[tables.orders]
mode = "truncate"
[tables.orders.columns.id]
generator = "integer_key"

[subset]
enabled = true
[[subset.targets]]
table = "orders"
percent = 10.0
"#,
        )
        .unwrap();
        let r = check(&ds, &config, 0).unwrap();
        let codes: Vec<&str> = r.issues.iter().map(|i| i.code).collect();
        assert!(codes.contains(&"mode.ignored_columns"), "{codes:?}");
        assert!(codes.contains(&"subset.target_mode"), "{codes:?}");
        // sample=0 disables scanning.
        assert_eq!(r.sampled_rows, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subset_drift_detected() {
        let dir = demo("subsetdrift");
        let ds = DirDataset::open(&dir).unwrap();
        let config = Config::parse(
            r#"
secret = "s"
[subset]
enabled = true
[[subset.targets]]
table = "orders"
where = "no_such_column > 5"
"#,
        )
        .unwrap();
        let r = check(&ds, &config, 0).unwrap();
        assert!(r.issues.iter().any(|i| i.code == "subset.invalid"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
