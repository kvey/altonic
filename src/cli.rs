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
use clap::{Parser, Subcommand};

const AFTER_HELP: &str = "\
<src>/<dst> are either dataset directories (schema.json + one CSV per
table) or live Postgres URLs:
  postgres://user:password@host:5432/dbname[?schema=public]
Postgres replication preserves the source's exact column types, primary
keys, unique constraints, and foreign keys in the destination database.
See SPEC.md for the configuration format and generator catalog.";

/// Rows sampled for PII detection when `--sample` is not given.
const DEFAULT_SAMPLE: usize = 1000;

#[derive(Parser)]
#[command(
    name = "altonic",
    version,
    about = "altonic — data cleaning, subsetting, and replication",
    after_help = AFTER_HELP,
    subcommand_required = false,
    arg_required_else_help = false
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Scaffold a demo dataset + config
    Init {
        /// Directory to create the workspace in
        dir: String,
    },
    /// Print the source schema
    Schema {
        #[arg(long)]
        source: String,
    },
    /// Detect likely-PII columns and recommend generators
    Scan {
        #[arg(long)]
        source: String,
        #[arg(long)]
        config: Option<String>,
        /// Rows to sample per table
        #[arg(long, default_value_t = DEFAULT_SAMPLE)]
        sample: usize,
        /// Emit the report as JSON
        #[arg(long)]
        json: bool,
        /// Write recommendations into this config file
        #[arg(long)]
        apply: Option<String>,
        /// Fail if any likely-PII column has no generator
        #[arg(long)]
        strict: bool,
    },
    /// Check a config against the source schema
    Check {
        #[arg(long)]
        source: String,
        #[arg(long)]
        config: String,
        #[arg(long, default_value_t = DEFAULT_SAMPLE)]
        sample: usize,
        #[arg(long)]
        json: bool,
        /// Fail on warnings as well as errors
        #[arg(long)]
        strict: bool,
    },
    /// Preview the subset a config selects
    Subset {
        #[arg(long)]
        source: String,
        #[arg(long)]
        config: String,
        #[arg(long)]
        json: bool,
        /// Check that every selected row's foreign keys resolve
        #[arg(long)]
        verify: bool,
    },
    /// Mask the source into the destination
    Generate {
        #[arg(long)]
        source: String,
        #[arg(long)]
        dest: String,
        #[arg(long)]
        config: String,
        /// Copy every table in full, ignoring the subset configuration
        #[arg(long = "no-subset")]
        no_subset: bool,
        /// Do all the work but write nothing
        #[arg(long = "dry-run")]
        dry_run: bool,
        #[arg(long)]
        json: bool,
        /// Write the run report as JSON to this path
        #[arg(long)]
        report: Option<String>,
    },
}

/// Entry point; returns the process exit code.
pub fn run() -> i32 {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            // --help/--version are a successful request for output, not a
            // usage error; everything else is a validation failure (exit 1).
            return match e.kind() {
                clap::error::ErrorKind::DisplayHelp
                | clap::error::ErrorKind::DisplayVersion
                | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
                    print!("{e}");
                    0
                }
                _ => {
                    eprint!("{e}");
                    1
                }
            };
        }
    };
    match dispatch(cli.command) {
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

fn dispatch(command: Option<Command>) -> Result<()> {
    let Some(command) = command else {
        // Bare `altonic`: show what it can do rather than an error.
        let mut cmd = <Cli as clap::CommandFactory>::command();
        cmd.print_help().map_err(|e| crate::rerr(e.to_string()))?;
        println!();
        return Ok(());
    };
    match command {
        Command::Init { dir } => cmd_init(&dir),
        Command::Schema { source } => cmd_schema(&source),
        Command::Scan { source, config, sample, json, apply, strict } => {
            cmd_scan(&source, config.as_deref(), sample, json, apply.as_deref(), strict)
        }
        Command::Check { source, config, sample, json, strict } => {
            cmd_check(&source, &config, sample, json, strict)
        }
        Command::Subset { source, config, json, verify } => {
            cmd_subset(&source, &config, json, verify)
        }
        Command::Generate { source, dest, config, no_subset, dry_run, json, report } => {
            cmd_generate(&source, &dest, &config, no_subset, dry_run, json, report.as_deref())
        }
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

fn cmd_schema(source: &str) -> Result<()> {
    let ds = open_source(source)?;
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

fn cmd_scan(
    source: &str,
    config_path: Option<&str>,
    sample: usize,
    json: bool,
    apply: Option<&str>,
    strict: bool,
) -> Result<()> {
    let ds = open_source(source)?;
    let config = match config_path {
        Some(p) => Some(load_config(p)?),
        None => None,
    };
    let report = scan::scan(ds.as_ref(), config.as_ref(), sample)?;

    if json {
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

    if let Some(apply_path) = apply {
        let mut config = if std::path::Path::new(apply_path).exists() {
            load_config(apply_path)?
        } else {
            Config::parse(&format!(
                r#"{{"name": "workspace", "secret": "{}"}}"#,
                fresh_secret()
            ))?
        };
        let applied = scan::apply_to_config(&report, &mut config);
        // Match the target's extension: .json keeps JSON, everything else
        // gets TOML (the preferred format).
        let rendered = if apply_path.ends_with(".json") {
            config.to_json().to_string_pretty()
        } else {
            config.to_toml()?
        };
        std::fs::write(apply_path, rendered)
            .map_err(|e| crate::rerr(format!("cannot write {apply_path}: {e}")))?;
        println!("\nApplied {applied} recommendation(s) to {apply_path}");
    }

    if strict {
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

// ---------- check ----------

fn cmd_check(
    source_spec: &str,
    config_path: &str,
    sample: usize,
    json: bool,
    strict: bool,
) -> Result<()> {
    let ds = open_source(source_spec)?;
    let config = load_config(config_path)?;
    let report = crate::check::check(ds.as_ref(), &config, sample)?;

    if json {
        println!("{}", report.to_json().to_string_pretty());
    } else {
        println!("Config {config_path} vs detected schema of {source_spec}\n");
        println!(
            "{:<24} {:>7} {:>10} {:>13} {:>12} {:>8}",
            "table", "columns", "masked", "inherited-key", "passthrough", "invalid"
        );
        for t in ds.schema().tables.iter() {
            let (masked, inherited, passthrough, invalid, na, total) =
                crate::check::table_summary(&report, &t.name);
            let mode = config.mode_for(&t.name);
            if na == total && total > 0 {
                println!(
                    "{:<24} {:>7}   ({} — column masking not applicable)",
                    t.name,
                    total,
                    mode.name()
                );
            } else {
                println!(
                    "{:<24} {:>7} {:>10} {:>13} {:>12} {:>8}",
                    t.name, total, masked, inherited, passthrough, invalid
                );
            }
        }
        if !report.issues.is_empty() {
            println!();
            for issue in &report.issues {
                let sev = match issue.severity {
                    crate::check::Severity::Error => "ERROR",
                    crate::check::Severity::Warning => "WARN ",
                };
                println!("[{sev}] {:<28} {}", issue.code, issue.message);
            }
        }
        println!(
            "\n{} error(s), {} warning(s); {} column(s) across {} table(s); {} row(s) sampled for PII.",
            report.errors(),
            report.warnings(),
            report.columns.len(),
            ds.schema().tables.len(),
            report.sampled_rows
        );
        if report.errors() == 0 && report.warnings() == 0 {
            println!("Config is fully consistent with the detected schema.");
        }
    }

    if report.errors() > 0 {
        return Err(crate::verr(format!(
            "check found {} error(s); `generate` would fail with this config",
            report.errors()
        )));
    }
    if strict && report.warnings() > 0 {
        return Err(crate::verr(format!(
            "strict mode: {} warning(s) (unprotected PII or ignored configuration)",
            report.warnings()
        )));
    }
    Ok(())
}

// ---------- subset ----------

fn cmd_subset(source: &str, config_path: &str, json: bool, verify: bool) -> Result<()> {
    let ds = open_source(source)?;
    let config = load_config(config_path)?;
    let sc = config
        .subset
        .as_ref()
        .ok_or_else(|| crate::verr("config has no \"subset\" section"))?;
    let result = subset::compute(ds.as_ref(), sc, config.secret.as_bytes())?;
    if verify {
        subset::verify_closure(ds.as_ref(), &result)?;
    }

    let mut names: Vec<&String> = result.tables.keys().collect();
    names.sort();
    if json {
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
        if verify {
            println!("\nClosure verified: every included row's foreign keys resolve in the subset.");
        }
    }
    Ok(())
}

// ---------- generate ----------

fn cmd_generate(
    source_path: &str,
    dest_path: &str,
    config_path: &str,
    no_subset: bool,
    dry_run: bool,
    json: bool,
    report_path: Option<&str>,
) -> Result<()> {
    let ds = open_source(source_path)?;
    let config = load_config(config_path)?;
    let mut dest = open_dest(dest_path)?;
    let options = GenerateOptions { use_subset: !no_subset, dry_run };
    let report = engine::generate(ds.as_ref(), dest.as_mut(), &config, &options)?;

    if json {
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
    if let Some(path) = report_path {
        std::fs::write(path, report.to_json().to_string_pretty())
            .map_err(|e| crate::rerr(format!("cannot write {path}: {e}")))?;
    }
    Ok(())
}

// ---------- init ----------

fn cmd_init(dir: &str) -> Result<()> {
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

    let config_path = root.join("altonic.toml");
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

const DEMO_CONFIG: &str = r#"# Altonic workspace configuration.
# The secret keys all consistent masking - treat it like a credential.
name = "demo"
secret = "__SECRET__"
seed = 1
preserve_referential_integrity = true

[tables.customers.columns.id]
generator = "integer_key"
options = { consistency = "value" }

[tables.customers.columns.first_name]
generator = "name"
options = { part = "first", link = "person", consistency = "value" }

[tables.customers.columns.last_name]
generator = "name"
options = { part = "last", link = "person", consistency = "value" }

[tables.customers.columns.email]
generator = "email"
options = { consistency = "value" }

[tables.customers.columns.phone]
generator = "phone"
options = { consistency = "value" }

[tables.customers.columns.ssn]
generator = "ssn"
options = { consistency = "value" }

[tables.customers.columns.street]
generator = "address"
options = { part = "street", link = "addr" }

[tables.customers.columns.city]
generator = "address"
options = { part = "city", link = "addr" }

[tables.customers.columns.zip]
generator = "address"
options = { part = "zip", link = "addr" }

[tables.customers.columns.created_at]
generator = "timestamp_shift"
options = { days = 30, consistency = "value" }

[tables.orders.columns.total]
generator = "noise"
options = { scale = 0.05, decimals = 2 }

[tables.orders.columns.placed_at]
generator = "timestamp_shift"
options = { days = 14 }

[tables.audit_log]
mode = "truncate"

[subset]
enabled = false
lookup_tables = ["states", "products"]
out_of_subset = "truncate"

[[subset.targets]]
table = "orders"
percent = 50.0

[[subset.upstream]]
table = "order_items"
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// clap's own consistency check over the command tree.
    #[test]
    fn command_tree_is_valid() {
        Cli::command().debug_assert();
    }

    /// The flag names are the user-facing contract; pin them.
    #[test]
    fn generate_flags() {
        let cli = Cli::try_parse_from([
            "altonic", "generate", "--source", "s", "--dest", "d", "--config", "c", "--no-subset",
            "--dry-run",
        ])
        .unwrap();
        let Some(Command::Generate { source, dest, config, no_subset, dry_run, report, json }) =
            cli.command
        else {
            panic!("expected the generate subcommand");
        };
        assert_eq!((source.as_str(), dest.as_str(), config.as_str()), ("s", "d", "c"));
        assert!(no_subset && dry_run);
        assert!(report.is_none() && !json);
    }

    #[test]
    fn sample_defaults_and_parses() {
        let default = Cli::try_parse_from(["altonic", "scan", "--source", "s"]).unwrap();
        assert!(matches!(default.command, Some(Command::Scan { sample, .. }) if sample == DEFAULT_SAMPLE));
        let zero =
            Cli::try_parse_from(["altonic", "scan", "--source", "s", "--sample", "0"]).unwrap();
        assert!(matches!(zero.command, Some(Command::Scan { sample: 0, .. })));
        assert!(Cli::try_parse_from(["altonic", "scan", "--source", "s", "--sample", "x"]).is_err());
    }

    #[test]
    fn required_flags_are_enforced() {
        assert!(Cli::try_parse_from(["altonic", "generate", "--source", "s"]).is_err());
        assert!(Cli::try_parse_from(["altonic", "schema"]).is_err());
        assert!(Cli::try_parse_from(["altonic", "scan", "--source", "s", "--bogus"]).is_err());
    }
}
