//! Workspace configuration (SPEC §4): table modes, per-column generator
//! assignments, and the subsetting configuration.

use crate::json::Json;
use crate::{verr, Result};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableMode {
    DeIdentify,
    Truncate,
    PreserveDestination,
}

impl TableMode {
    pub fn parse(s: &str) -> Option<TableMode> {
        Some(match s {
            "de_identify" | "deidentify" => TableMode::DeIdentify,
            "truncate" => TableMode::Truncate,
            "preserve_destination" | "preserve" => TableMode::PreserveDestination,
            _ => return None,
        })
    }

    pub fn name(&self) -> &'static str {
        match self {
            TableMode::DeIdentify => "de_identify",
            TableMode::Truncate => "truncate",
            TableMode::PreserveDestination => "preserve_destination",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ColumnConfig {
    pub generator: String,
    /// Raw generator options; interpreted by the generator factory.
    pub options: Json,
}

#[derive(Debug, Clone)]
pub struct TableConfig {
    pub mode: TableMode,
    pub columns: HashMap<String, ColumnConfig>,
    /// Preserves config-file ordering for stable reports.
    pub column_order: Vec<String>,
}

impl Default for TableConfig {
    fn default() -> Self {
        TableConfig {
            mode: TableMode::DeIdentify,
            columns: HashMap::new(),
            column_order: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum TargetFilter {
    Percent(f64),
    Where(String),
}

#[derive(Debug, Clone)]
pub struct SubsetTarget {
    pub table: String,
    pub filter: TargetFilter,
}

#[derive(Debug, Clone)]
pub struct UpstreamRule {
    pub table: String,
    /// Additional predicate; None means "all referencing rows".
    pub where_clause: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutOfSubset {
    Truncate,
    Process,
}

#[derive(Debug, Clone)]
pub struct SubsetConfig {
    pub enabled: bool,
    pub targets: Vec<SubsetTarget>,
    pub lookup_tables: Vec<String>,
    pub upstream: Vec<UpstreamRule>,
    pub out_of_subset: OutOfSubset,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub name: String,
    pub secret: String,
    pub seed: u64,
    pub preserve_referential_integrity: bool,
    pub tables: HashMap<String, TableConfig>,
    pub table_order: Vec<String>,
    pub subset: Option<SubsetConfig>,
}

impl Config {
    pub fn table(&self, name: &str) -> Option<&TableConfig> {
        self.tables.get(name)
    }

    pub fn mode_for(&self, table: &str) -> TableMode {
        self.tables.get(table).map(|t| t.mode).unwrap_or(TableMode::DeIdentify)
    }

    pub fn column_config(&self, table: &str, column: &str) -> Option<&ColumnConfig> {
        self.tables.get(table).and_then(|t| t.columns.get(column))
    }

    pub fn subset_enabled(&self) -> bool {
        self.subset.as_ref().map(|s| s.enabled).unwrap_or(false)
    }

    pub fn parse(text: &str) -> Result<Config> {
        let root = Json::parse(text).map_err(|e| verr(format!("config: {e}")))?;
        Self::from_json(&root)
    }

    pub fn from_json(root: &Json) -> Result<Config> {
        let name = root.get("name").and_then(|n| n.as_str()).unwrap_or("workspace").to_string();
        let secret = root
            .get("secret")
            .and_then(|s| s.as_str())
            .ok_or_else(|| verr("config: missing \"secret\" (workspace consistency key)"))?
            .to_string();
        if secret.is_empty() {
            return Err(verr("config: \"secret\" must not be empty"));
        }
        let seed = root.get("seed").and_then(|s| s.as_i64()).unwrap_or(0) as u64;
        let preserve_referential_integrity = root
            .get("preserve_referential_integrity")
            .and_then(|p| p.as_bool())
            .unwrap_or(true);

        let mut tables = HashMap::new();
        let mut table_order = Vec::new();
        if let Some(tobj) = root.get("tables") {
            let members = tobj
                .as_object()
                .ok_or_else(|| verr("config: \"tables\" must be an object"))?;
            for (tname, tj) in members {
                let tc = parse_table_config(tname, tj)?;
                table_order.push(tname.clone());
                tables.insert(tname.clone(), tc);
            }
        }

        let subset = match root.get("subset") {
            None => None,
            Some(sj) => Some(parse_subset(sj)?),
        };

        Ok(Config {
            name,
            secret,
            seed,
            preserve_referential_integrity,
            tables,
            table_order,
            subset,
        })
    }

    pub fn to_json(&self) -> Json {
        let mut members = vec![
            ("name".to_string(), Json::Str(self.name.clone())),
            ("secret".to_string(), Json::Str(self.secret.clone())),
            ("seed".to_string(), Json::Int(self.seed as i64)),
            (
                "preserve_referential_integrity".to_string(),
                Json::Bool(self.preserve_referential_integrity),
            ),
        ];
        let mut tables = Vec::new();
        for tname in &self.table_order {
            let tc = &self.tables[tname];
            let mut tm = Vec::new();
            if tc.mode != TableMode::DeIdentify {
                tm.push(("mode".to_string(), Json::Str(tc.mode.name().to_string())));
            }
            if !tc.columns.is_empty() {
                let mut cols = Vec::new();
                for cname in &tc.column_order {
                    let cc = &tc.columns[cname];
                    let mut cm = vec![("generator".to_string(), Json::Str(cc.generator.clone()))];
                    if !matches!(cc.options, Json::Object(ref o) if o.is_empty()) && !cc.options.is_null() {
                        cm.push(("options".to_string(), cc.options.clone()));
                    }
                    cols.push((cname.clone(), Json::Object(cm)));
                }
                tm.push(("columns".to_string(), Json::Object(cols)));
            }
            tables.push((tname.clone(), Json::Object(tm)));
        }
        members.push(("tables".to_string(), Json::Object(tables)));
        if let Some(s) = &self.subset {
            members.push(("subset".to_string(), subset_to_json(s)));
        }
        Json::Object(members)
    }
}

fn parse_table_config(tname: &str, tj: &Json) -> Result<TableConfig> {
    let mode = match tj.get("mode") {
        None => TableMode::DeIdentify,
        Some(m) => {
            let s = m
                .as_str()
                .ok_or_else(|| verr(format!("config: table {tname:?} mode must be a string")))?;
            TableMode::parse(s)
                .ok_or_else(|| verr(format!("config: table {tname:?} unknown mode {s:?}")))?
        }
    };
    let mut columns = HashMap::new();
    let mut column_order = Vec::new();
    if let Some(cj) = tj.get("columns") {
        let members = cj
            .as_object()
            .ok_or_else(|| verr(format!("config: table {tname:?} \"columns\" must be an object")))?;
        for (cname, cfg) in members {
            let generator = cfg
                .get("generator")
                .and_then(|g| g.as_str())
                .ok_or_else(|| {
                    verr(format!("config: column {tname:?}.{cname:?} missing \"generator\""))
                })?
                .to_string();
            let options = cfg.get("options").cloned().unwrap_or(Json::Object(Vec::new()));
            if options.as_object().is_none() {
                return Err(verr(format!(
                    "config: column {tname:?}.{cname:?} \"options\" must be an object"
                )));
            }
            column_order.push(cname.clone());
            columns.insert(cname.clone(), ColumnConfig { generator, options });
        }
    }
    Ok(TableConfig { mode, columns, column_order })
}

fn parse_subset(sj: &Json) -> Result<SubsetConfig> {
    let enabled = sj.get("enabled").and_then(|e| e.as_bool()).unwrap_or(true);
    let mut targets = Vec::new();
    if let Some(tj) = sj.get("targets") {
        for t in tj.as_array().ok_or_else(|| verr("config: subset targets must be an array"))? {
            let table = t
                .get("table")
                .and_then(|x| x.as_str())
                .ok_or_else(|| verr("config: subset target missing \"table\""))?
                .to_string();
            let percent = t.get("percent").and_then(|p| p.as_f64());
            let where_clause = t.get("where").and_then(|w| w.as_str());
            let filter = match (percent, where_clause) {
                (Some(p), None) => {
                    if !(0.0..=100.0).contains(&p) {
                        return Err(verr(format!(
                            "config: subset target {table:?} percent must be in [0, 100]"
                        )));
                    }
                    TargetFilter::Percent(p)
                }
                (None, Some(w)) => TargetFilter::Where(w.to_string()),
                _ => {
                    return Err(verr(format!(
                        "config: subset target {table:?} needs exactly one of \"percent\" or \"where\""
                    )))
                }
            };
            targets.push(SubsetTarget { table, filter });
        }
    }
    if enabled && targets.is_empty() {
        return Err(verr("config: subset enabled but no targets configured"));
    }
    let lookup_tables = match sj.get("lookup_tables") {
        None => Vec::new(),
        Some(l) => l
            .as_array()
            .ok_or_else(|| verr("config: subset lookup_tables must be an array"))?
            .iter()
            .map(|x| {
                x.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| verr("config: subset lookup_tables must be strings"))
            })
            .collect::<Result<Vec<_>>>()?,
    };
    let mut upstream = Vec::new();
    if let Some(uj) = sj.get("upstream") {
        for u in uj.as_array().ok_or_else(|| verr("config: subset upstream must be an array"))? {
            let table = u
                .get("table")
                .and_then(|x| x.as_str())
                .ok_or_else(|| verr("config: subset upstream rule missing \"table\""))?
                .to_string();
            let where_clause = u.get("where").and_then(|w| w.as_str()).map(str::to_string);
            upstream.push(UpstreamRule { table, where_clause });
        }
    }
    let out_of_subset = match sj.get("out_of_subset").and_then(|o| o.as_str()) {
        None | Some("truncate") => OutOfSubset::Truncate,
        Some("process") => OutOfSubset::Process,
        Some(other) => {
            return Err(verr(format!(
                "config: subset out_of_subset must be \"truncate\" or \"process\", got {other:?}"
            )))
        }
    };
    Ok(SubsetConfig { enabled, targets, lookup_tables, upstream, out_of_subset })
}

fn subset_to_json(s: &SubsetConfig) -> Json {
    let mut members = vec![("enabled".to_string(), Json::Bool(s.enabled))];
    members.push((
        "targets".to_string(),
        Json::Array(
            s.targets
                .iter()
                .map(|t| {
                    let mut m = vec![("table".to_string(), Json::Str(t.table.clone()))];
                    match &t.filter {
                        TargetFilter::Percent(p) => m.push(("percent".to_string(), Json::Float(*p))),
                        TargetFilter::Where(w) => m.push(("where".to_string(), Json::Str(w.clone()))),
                    }
                    Json::Object(m)
                })
                .collect(),
        ),
    ));
    if !s.lookup_tables.is_empty() {
        members.push((
            "lookup_tables".to_string(),
            Json::Array(s.lookup_tables.iter().map(|l| Json::Str(l.clone())).collect()),
        ));
    }
    if !s.upstream.is_empty() {
        members.push((
            "upstream".to_string(),
            Json::Array(
                s.upstream
                    .iter()
                    .map(|u| {
                        let mut m = vec![("table".to_string(), Json::Str(u.table.clone()))];
                        if let Some(w) = &u.where_clause {
                            m.push(("where".to_string(), Json::Str(w.clone())));
                        }
                        Json::Object(m)
                    })
                    .collect(),
            ),
        ));
    }
    members.push((
        "out_of_subset".to_string(),
        Json::Str(
            match s.out_of_subset {
                OutOfSubset::Truncate => "truncate",
                OutOfSubset::Process => "process",
            }
            .to_string(),
        ),
    ));
    Json::Object(members)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn demo_config_json() -> &'static str {
        r#"{
          "name": "demo",
          "secret": "test-secret",
          "seed": 7,
          "tables": {
            "customers": {
              "columns": {
                "email": {"generator": "email", "options": {"consistency": "value"}}
              }
            },
            "audit_log": {"mode": "truncate"}
          },
          "subset": {
            "enabled": true,
            "targets": [{"table": "orders", "percent": 10.0}],
            "lookup_tables": ["states"],
            "upstream": [{"table": "orders", "where": "total > 100"}]
          }
        }"#
    }

    #[test]
    fn parses() {
        let c = Config::parse(demo_config_json()).unwrap();
        assert_eq!(c.secret, "test-secret");
        assert_eq!(c.seed, 7);
        assert!(c.preserve_referential_integrity);
        assert_eq!(c.mode_for("audit_log"), TableMode::Truncate);
        assert_eq!(c.mode_for("unknown"), TableMode::DeIdentify);
        assert_eq!(c.column_config("customers", "email").unwrap().generator, "email");
        let s = c.subset.as_ref().unwrap();
        assert!(matches!(s.targets[0].filter, TargetFilter::Percent(p) if p == 10.0));
        assert_eq!(s.out_of_subset, OutOfSubset::Truncate);
    }

    #[test]
    fn round_trips() {
        let c = Config::parse(demo_config_json()).unwrap();
        let j = c.to_json().to_string_pretty();
        let c2 = Config::parse(&j).unwrap();
        assert_eq!(c2.secret, c.secret);
        assert_eq!(c2.table_order, c.table_order);
        assert_eq!(c2.subset.as_ref().unwrap().targets.len(), 1);
    }

    #[test]
    fn rejects_invalid() {
        assert!(Config::parse(r#"{"tables": {}}"#).is_err()); // no secret
        assert!(Config::parse(r#"{"secret": ""}"#).is_err());
        assert!(Config::parse(
            r#"{"secret": "s", "subset": {"targets": [{"table": "t"}]}}"#
        )
        .is_err()); // target without filter
        assert!(Config::parse(
            r#"{"secret": "s", "subset": {"targets": [{"table": "t", "percent": 150}]}}"#
        )
        .is_err());
        assert!(Config::parse(
            r#"{"secret": "s", "tables": {"t": {"mode": "bogus"}}}"#
        )
        .is_err());
    }
}
