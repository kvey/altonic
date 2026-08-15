//! Command-line interface (SPEC §11).
//!
//! Subcommands: init, schema, scan, subset, generate.
//! Exit codes: 0 success, 1 validation error, 2 runtime error.

use crate::config::{Config, TableMode};
use crate::connector::{DestinationConnector, DirDataset, DirDestination, SourceConnector};
use crate::engine::{self, GenerateOptions};
use crate::pg::{PgDestination, PgSource, PgUrl};
use crate::scan;
use crate::subset::{self, TableRole};
use crate::{Error, Result};
use std::collections::HashMap;

const USAGE: &str = "altonic — pure-Rust data cleaning, subsetting, and replication

USAGE:
  altonic init <dir>                          scaffold a demo dataset + config
  altonic schema   --source <src>             print the source schema
  altonic scan     --source <src> [--config <file>] [--sample N]
                   [--json] [--apply <file>] [--strict]
  altonic subset   --source <src> --config <file> [--json] [--verify]
  altonic generate --source <src> --dest <dst> --config <file>
                   [--no-subset] [--dry-run] [--report <file>] [--json]

<src>/<dst> are either dataset directories (schema.json + one CSV per
table) or live Postgres URLs:
  postgres://user:password@host:5432/dbname[?schema=public]
Postgres replication preserves the source's exact column types, primary
keys, unique constraints, and foreign keys in the destination database.
See SPEC.md for the configuration format and generator catalog.";

/// Entry point; returns the process exit code.
pub fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(&args) {
        Ok(()) => 0,
        Err(Error::Validation(m)) => {
            eprintln!("error: {m}");
            1
        }
        Err(Error::Runtime(m)) => {
            eprintln!("error: {m}");
            2
        }
    }
}

struct Flags {
    positional: Vec<String>,
    named: HashMap<String, String>,
    switches: Vec<String>,
}

fn parse_flags(args: &[String], named_keys: &[&str], switch_keys: &[&str]) -> Result<Flags> {
    let mut f = Flags { positional: Vec::new(), named: HashMap::new(), switches: Vec::new() };
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(key) = a.strip_prefix("--") {
            if switch_keys.contains(&key) {
                f.switches.push(key.to_string());
            } else if named_keys.contains(&key) {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| crate::verr(format!("--{key} requires a value")))?;
                f.named.insert(key.to_string(), v.clone());
            } else {
                return Err(crate::verr(format!("unknown flag --{key}")));
            }
        } else {
            f.positional.push(a.clone());
        }
        i += 1;
    }
    Ok(f)
}

impl Flags {
    fn require(&self, key: &str) -> Result<&str> {
        self.named
            .get(key)
            .map(String::as_str)
            .ok_or_else(|| crate::verr(format!("--{key} is required")))
    }

    fn has(&self, key: &str) -> bool {
        self.switches.iter().any(|s| s == key)
    }
}

fn dispatch(args: &[String]) -> Result<()> {
    let Some(cmd) = args.first() else {
        println!("{USAGE}");
        return Ok(());
    };
    let rest = &args[1..];
    match cmd.as_str() {
        "init" => cmd_init(rest),
        "schema" => cmd_schema(rest),
        "scan" => cmd_scan(rest),
        "subset" => cmd_subset(rest),
        "generate" => cmd_generate(rest),
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        other => Err(crate::verr(format!("unknown command {other:?}; run `altonic help`"))),
    }
}

fn load_config(path: &str) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| crate::rerr(format!("cannot read config {path}: {e}")))?;
    Config::parse(&text)
}

/// Open a source: a Postgres URL or a dataset directory.
fn open_source(spec: &str) -> Result<Box<dyn SourceConnector>> {
    if PgUrl::is_pg_url(spec) {
        Ok(Box::new(PgSource::connect(spec)?))
    } else {
        Ok(Box::new(DirDataset::open(spec)?))
    }
}

/// Open a destination: a Postgres URL or a dataset directory.
fn open_dest(spec: &str) -> Result<Box<dyn DestinationConnector>> {
    if PgUrl::is_pg_url(spec) {
        Ok(Box::new(PgDestination::connect(spec)?))
    } else {
        Ok(Box::new(DirDestination::create(spec)?))
    }
}

// ---------- schema ----------

fn cmd_schema(args: &[String]) -> Result<()> {
    let f = parse_flags(args, &["source"], &[])?;
    let ds = open_source(f.require("source")?)?;
    for t in &ds.schema().tables {
        let rows = ds.count_rows(&t.name)?;
        println!("{}  ({} rows)", t.name, rows);
        for c in &t.columns {
            let mut notes = Vec::new();
            if t.primary_key.contains(&c.name) {
                notes.push("PK".to_string());
            }
            for fk in &t.foreign_keys {
                if fk.columns.contains(&c.name) {
                    notes.push(format!("FK -> {}", fk.references_table));
                }
            }
            if c.unique && !t.primary_key.contains(&c.name) {
                notes.push("unique".to_string());
            }
            if !c.nullable {
                notes.push("not null".to_string());
            }
            let suffix = if notes.is_empty() { String::new() } else { format!("  [{}]", notes.join(", ")) };
            println!("  {:<24} {}{}", c.name, c.ty.name(), suffix);
        }
        println!();
    }
    Ok(())
}

// ---------- scan ----------

fn cmd_scan(args: &[String]) -> Result<()> {
    let f = parse_flags(args, &["source", "config", "sample", "apply"], &["json", "strict"])?;
    let ds = open_source(f.require("source")?)?;
    let config = match f.named.get("config") {
        Some(p) => Some(load_config(p)?),
        None => None,
    };
    let sample: usize = match f.named.get("sample") {
        Some(s) => s.parse().map_err(|_| crate::verr("--sample must be a number"))?,
        None => 1000,
    };
    let report = scan::scan(ds.as_ref(), config.as_ref(), sample)?;

    if f.has("json") {
        println!("{}", report.to_json().to_string_pretty());
    } else if report.findings.is_empty() {
        println!("No likely-PII columns detected ({} rows sampled).", report.sampled_rows);
    } else {
        println!(
            "{} finding(s) across {} sampled rows:\n",
            report.findings.len(),
            report.sampled_rows
        );
        println!(
            "{:<28} {:<14} {:<10} {:<20} configured?",
            "column", "type", "confidence", "recommended"
        );
        for finding in &report.findings {
            println!(
                "{:<28} {:<14} {:<10} {:<20} {}",
                format!("{}.{}", finding.table, finding.column),
                finding.pii_type,
                finding.confidence.name(),
                finding.recommended_generator,
                if finding.already_configured { "yes" } else { "NO" }
            );
        }
    }

    if let Some(apply_path) = f.named.get("apply") {
        let mut config = if std::path::Path::new(apply_path).exists() {
            load_config(apply_path)?
        } else {
            Config::parse(&format!(
                r#"{{"name": "workspace", "secret": "{}"}}"#,
                fresh_secret()
            ))?
        };
        let applied = scan::apply_to_config(&report, &mut config);
        std::fs::write(apply_path, config.to_json().to_string_pretty())
            .map_err(|e| crate::rerr(format!("cannot write {apply_path}: {e}")))?;
        println!("\nApplied {applied} recommendation(s) to {apply_path}");
    }

    if f.has("strict") {
        let unresolved = scan::unresolved(&report);
        if !unresolved.is_empty() {
            return Err(crate::verr(format!(
                "strict mode: {} likely-PII column(s) have no generator configured: {}",
                unresolved.len(),
                unresolved
                    .iter()
                    .map(|u| format!("{}.{}", u.table, u.column))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    Ok(())
}

/// Non-cryptographic convenience secret for scaffolded configs, derived from
/// ambient entropy (time + pid). Users should replace it for real workspaces.
fn fresh_secret() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let digest = crate::crypto::hmac_parts(
        b"altonic-init",
        &[&now.to_be_bytes(), &(std::process::id() as u64).to_be_bytes()],
    );
    crate::crypto::to_hex(&digest)[..32].to_string()
}

// ---------- subset ----------

fn cmd_subset(args: &[String]) -> Result<()> {
    let f = parse_flags(args, &["source", "config"], &["json", "verify"])?;
    let ds = open_source(f.require("source")?)?;
    let config = load_config(f.require("config")?)?;
    let sc = config
        .subset
        .as_ref()
        .ok_or_else(|| crate::verr("config has no \"subset\" section"))?;
    let result = subset::compute(ds.as_ref(), sc, config.secret.as_bytes())?;
    if f.has("verify") {
        subset::verify_closure(ds.as_ref(), &result)?;
    }

    let mut names: Vec<&String> = result.tables.keys().collect();
    names.sort();
    if f.has("json") {
        let mut members = Vec::new();
        for name in names {
            let ts = &result.tables[name];
            members.push((
                name.clone(),
                crate::json::Json::Object(vec![
                    ("role".to_string(), crate::json::Json::Str(format!("{:?}", ts.role).to_lowercase())),
                    ("selected".to_string(), crate::json::Json::Int(ts.rows.len() as i64)),
                    ("total".to_string(), crate::json::Json::Int(ts.total as i64)),
                ]),
            ));
        }
        println!("{}", crate::json::Json::Object(members).to_string_pretty());
    } else {
        println!("{:<24} {:<14} {:>10} {:>10} {:>7}", "table", "role", "selected", "total", "%");
        for name in names {
            let ts = &result.tables[name];
            let pct = if ts.total == 0 { 0.0 } else { 100.0 * ts.rows.len() as f64 / ts.total as f64 };
            println!(
                "{:<24} {:<14} {:>10} {:>10} {:>6.1}%",
                name,
                format!("{:?}", ts.role).to_lowercase(),
                ts.rows.len(),
                ts.total,
                pct
            );
        }
        if f.has("verify") {
            println!("\nClosure verified: every included row's foreign keys resolve in the subset.");
        }
    }
    Ok(())
}

// ---------- generate ----------

fn cmd_generate(args: &[String]) -> Result<()> {
    let f = parse_flags(
        args,
        &["source", "dest", "config", "report"],
        &["no-subset", "dry-run", "json"],
    )?;
    let source_path = f.require("source")?;
    let dest_path = f.require("dest")?;
    let config_path = f.require("config")?;
    let ds = open_source(source_path)?;
    let config = load_config(config_path)?;
    let mut dest = open_dest(dest_path)?;
    let options = GenerateOptions {
        use_subset: !f.has("no-subset"),
        dry_run: f.has("dry-run"),
    };
    let report = engine::generate(ds.as_ref(), dest.as_mut(), &config, &options)?;

    if f.has("json") {
        println!("{}", report.to_json().to_string_pretty());
    } else {
        println!(
            "{:<24} {:<22} {:>10} {:>10} {:>12}",
            "table", "mode", "rows in", "rows out", "cells masked"
        );
        for t in &report.tables {
            let mode = match (t.mode, t.role) {
                (TableMode::DeIdentify, Some(TableRole::OutOfSubset)) => "de_identify (truncated)".to_string(),
                (m, _) => m.name().to_string(),
            };
            println!(
                "{:<24} {:<22} {:>10} {:>10} {:>12}",
                t.table, mode, t.rows_read, t.rows_written, t.cells_masked
            );
        }
        let action = if options.dry_run { "Dry run finished" } else { "Generation finished" };
        println!("\n{action} in {} ms; destination: {dest_path}", report.elapsed_ms);
    }
    if let Some(path) = f.named.get("report") {
        std::fs::write(path, report.to_json().to_string_pretty())
            .map_err(|e| crate::rerr(format!("cannot write {path}: {e}")))?;
    }
    Ok(())
}

// ---------- init ----------

fn cmd_init(args: &[String]) -> Result<()> {
    let f = parse_flags(args, &[], &[])?;
    let dir = f
        .positional
        .first()
        .ok_or_else(|| crate::verr("usage: altonic init <dir>"))?;
    let root = std::path::Path::new(dir);
    let source = root.join("source");
    if source.exists() {
        return Err(crate::verr(format!("{} already exists", source.display())));
    }
    std::fs::create_dir_all(&source)?;

    std::fs::write(source.join("schema.json"), DEMO_SCHEMA)?;
    std::fs::write(source.join("states.csv"), DEMO_STATES)?;
    std::fs::write(source.join("customers.csv"), DEMO_CUSTOMERS)?;
    std::fs::write(source.join("orders.csv"), DEMO_ORDERS)?;
    std::fs::write(source.join("order_items.csv"), DEMO_ORDER_ITEMS)?;
    std::fs::write(source.join("products.csv"), DEMO_PRODUCTS)?;
    std::fs::write(source.join("audit_log.csv"), DEMO_AUDIT)?;

    let config_path = root.join("altonic.json");
    let config_text = DEMO_CONFIG.replace("__SECRET__", &fresh_secret());
    // Validate what we scaffold.
    Config::parse(&config_text)?;
    std::fs::write(&config_path, &config_text)?;

    println!("Scaffolded demo workspace in {}:", root.display());
    println!("  {}/           source dataset (schema.json + CSVs)", source.display());
    println!("  {}    workspace config", config_path.display());
    println!("\nTry:");
    println!("  altonic schema   --source {}", source.display());
    println!("  altonic scan     --source {} --config {}", source.display(), config_path.display());
    println!(
        "  altonic subset   --source {} --config {} --verify",
        source.display(),
        config_path.display()
    );
    println!(
        "  altonic generate --source {} --dest {}/dest --config {}",
        source.display(),
        root.display(),
        config_path.display()
    );
    Ok(())
}

const DEMO_SCHEMA: &str = r#"{
  "tables": [
    {
      "name": "states",
      "columns": [
        {"name": "code", "type": "text", "nullable": false},
        {"name": "name", "type": "text"}
      ],
      "primary_key": ["code"]
    },
    {
      "name": "products",
      "columns": [
        {"name": "id", "type": "integer"},
        {"name": "name", "type": "text"},
        {"name": "unit_price", "type": "float"}
      ],
      "primary_key": ["id"]
    },
    {
      "name": "customers",
      "columns": [
        {"name": "id", "type": "integer"},
        {"name": "first_name", "type": "text"},
        {"name": "last_name", "type": "text"},
        {"name": "email", "type": "text", "unique": true},
        {"name": "phone", "type": "text"},
        {"name": "ssn", "type": "text"},
        {"name": "street", "type": "text"},
        {"name": "city", "type": "text"},
        {"name": "state_code", "type": "text"},
        {"name": "zip", "type": "text"},
        {"name": "vip", "type": "boolean"},
        {"name": "created_at", "type": "timestamp"}
      ],
      "primary_key": ["id"],
      "foreign_keys": [
        {"columns": ["state_code"], "references_table": "states", "references_columns": ["code"]}
      ]
    },
    {
      "name": "orders",
      "columns": [
        {"name": "id", "type": "integer"},
        {"name": "customer_id", "type": "integer", "nullable": false},
        {"name": "status", "type": "text"},
        {"name": "total", "type": "float"},
        {"name": "placed_at", "type": "timestamp"}
      ],
      "primary_key": ["id"],
      "foreign_keys": [
        {"columns": ["customer_id"], "references_table": "customers", "references_columns": ["id"]}
      ]
    },
    {
      "name": "order_items",
      "columns": [
        {"name": "id", "type": "integer"},
        {"name": "order_id", "type": "integer", "nullable": false},
        {"name": "product_id", "type": "integer", "nullable": false},
        {"name": "quantity", "type": "integer"},
        {"name": "unit_price", "type": "float"}
      ],
      "primary_key": ["id"],
      "foreign_keys": [
        {"columns": ["order_id"], "references_table": "orders", "references_columns": ["id"]},
        {"columns": ["product_id"], "references_table": "products", "references_columns": ["id"]}
      ]
    },
    {
      "name": "audit_log",
      "columns": [
        {"name": "id", "type": "integer"},
        {"name": "actor_email", "type": "text"},
        {"name": "action", "type": "text"},
        {"name": "at", "type": "timestamp"}
      ],
      "primary_key": ["id"]
    }
  ]
}
"#;

const DEMO_STATES: &str = "code,name\nCA,California\nNY,New York\nTX,Texas\nWA,Washington\nIL,Illinois\n";

const DEMO_PRODUCTS: &str = "\
id,name,unit_price
1,Widget,9.99
2,Gadget,24.5
3,Sprocket,3.75
4,Doohickey,149.0
";

const DEMO_CUSTOMERS: &str = "\
id,first_name,last_name,email,phone,ssn,street,city,state_code,zip,vip,created_at
1,Alice,Anderson,alice.anderson@corp.example,(415) 555-0132,545-11-2211,12 Bay St,San Francisco,CA,94110,true,2023-01-12 09:15:00
2,Bob,Baker,bob.baker@corp.example,(212) 555-0177,232-44-8899,88 5th Ave,New York,NY,10011,false,2023-02-03 14:22:31
3,Carla,Chen,carla.chen@corp.example,(512) 555-0101,451-77-3321,7 Oak Ln,Austin,TX,78701,false,2023-03-19 11:05:12
4,Dan,Diaz,dan.diaz@corp.example,(206) 555-0155,090-55-1276,901 Pine Rd,Seattle,WA,98101,true,2023-05-27 16:44:03
5,Erin,Evans,erin.evans@corp.example,(312) 555-0190,340-22-9087,455 Lake Dr,Chicago,IL,60601,false,2023-08-08 08:01:59
6,Frank,Foster,frank.foster@corp.example,(415) 555-0119,520-66-4455,2201 Hill Blvd,San Francisco,CA,94117,false,2023-09-14 19:30:45
";

const DEMO_ORDERS: &str = "\
id,customer_id,status,total,placed_at
101,1,shipped,34.49,2024-01-05 10:00:00
102,1,shipped,149.0,2024-02-11 12:30:00
103,2,pending,24.5,2024-02-14 09:12:00
104,3,shipped,19.98,2024-03-01 15:45:00
105,3,cancelled,3.75,2024-03-02 08:20:00
106,4,shipped,298.0,2024-04-22 13:37:00
107,5,pending,9.99,2024-05-30 17:55:00
108,6,shipped,52.74,2024-06-18 11:11:00
";

const DEMO_ORDER_ITEMS: &str = "\
id,order_id,product_id,quantity,unit_price
1,101,1,1,9.99
2,101,2,1,24.5
3,102,4,1,149.0
4,103,2,1,24.5
5,104,1,2,9.99
6,105,3,1,3.75
7,106,4,2,149.0
8,107,1,1,9.99
9,108,2,1,24.5
10,108,1,2,9.99
11,108,3,2,3.75
";

const DEMO_AUDIT: &str = "\
id,actor_email,action,at
1,admin@corp.example,login,2024-01-01 00:00:01
2,admin@corp.example,export,2024-01-02 00:10:44
";

const DEMO_CONFIG: &str = r#"{
  "name": "demo",
  "secret": "__SECRET__",
  "seed": 1,
  "preserve_referential_integrity": true,
  "tables": {
    "customers": {
      "columns": {
        "id": {"generator": "integer_key", "options": {"consistency": "value"}},
        "first_name": {"generator": "name", "options": {"part": "first", "link": "person", "consistency": "value"}},
        "last_name": {"generator": "name", "options": {"part": "last", "link": "person", "consistency": "value"}},
        "email": {"generator": "email", "options": {"consistency": "value"}},
        "phone": {"generator": "phone", "options": {"consistency": "value"}},
        "ssn": {"generator": "ssn", "options": {"consistency": "value"}},
        "street": {"generator": "address", "options": {"part": "street", "link": "addr"}},
        "city": {"generator": "address", "options": {"part": "city", "link": "addr"}},
        "zip": {"generator": "address", "options": {"part": "zip", "link": "addr"}},
        "created_at": {"generator": "timestamp_shift", "options": {"days": 30, "consistency": "value"}}
      }
    },
    "orders": {
      "columns": {
        "total": {"generator": "noise", "options": {"scale": 0.05, "decimals": 2}},
        "placed_at": {"generator": "timestamp_shift", "options": {"days": 14}}
      }
    },
    "audit_log": {"mode": "truncate"}
  },
  "subset": {
    "enabled": false,
    "targets": [
      {"table": "orders", "percent": 50.0}
    ],
    "lookup_tables": ["states", "products"],
    "upstream": [
      {"table": "order_items"}
    ],
    "out_of_subset": "truncate"
  }
}
"#;
