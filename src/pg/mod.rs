//! Postgres connectors: a `SourceConnector` that introspects and streams a
//! live database, and a `DestinationConnector` that recreates the same shape —
//! original column types, PKs, uniques, and FKs — and bulk-loads masked rows
//! via COPY. The wire protocol, authentication and TLS-less transport all come
//! from the `postgres` driver.

mod copy;
mod text;
mod url;

pub use url::PgUrl;

/// Run one or more SQL statements against a database (used by integration
/// tests and one-off administration; results are discarded).
pub fn run_sql(url_text: &str, sql: &str) -> Result<()> {
    let mut client = PgUrl::parse(url_text)?.connect()?;
    client
        .batch_execute(sql)
        .map_err(|e| rerr(format!("postgres: {}", describe(&e))))?;
    Ok(())
}

use crate::connector::{DestinationConnector, RowSink, RowSource, SourceConnector};
use crate::schema::{Column, ForeignKey, Schema, Table};
use crate::value::Row;
use crate::{rerr, verr, Result};
use copy::{CopyIn, CopyOut};
use postgres::{Client, SimpleQueryMessage};
use std::collections::HashMap;
use text::{copy_escape, copy_split, default_pg_type, map_pg_type, quote_ident, quote_literal, text_to_value, value_to_text};

/// Render a driver error, preferring the server's own severity and message
/// over the driver's wrapper text.
fn describe(e: &postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => {
            let detail = db.detail().map(|d| format!(" ({d})")).unwrap_or_default();
            format!("{}: {}{detail}", db.severity(), db.message())
        }
        None => e.to_string(),
    }
}

/// Run a statement and collect its rows as text (None = NULL per cell).
fn text_rows(client: &mut Client, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
    let messages = client
        .simple_query(sql)
        .map_err(|e| rerr(format!("postgres: {}\n  while running: {sql}", describe(&e))))?;
    let mut rows = Vec::new();
    for message in messages {
        if let SimpleQueryMessage::Row(r) = message {
            rows.push((0..r.len()).map(|i| r.get(i).map(str::to_string)).collect());
        }
    }
    Ok(rows)
}

/// Convenience: run and expect a single text column.
fn text_column(client: &mut Client, sql: &str) -> Result<Vec<String>> {
    Ok(text_rows(client, sql)?
        .into_iter()
        .filter_map(|r| r.into_iter().next().flatten())
        .collect())
}

// ---------- Source ----------

pub struct PgSource {
    url: PgUrl,
    schema: Schema,
}

impl PgSource {
    pub fn connect(url_text: &str) -> Result<PgSource> {
        let url = PgUrl::parse(url_text)?;
        let mut client = url.connect()?;
        let schema = introspect(&mut client, &url.schema)?;
        if schema.tables.is_empty() {
            return Err(verr(format!(
                "postgres source {} has no tables in schema {:?}",
                url.redacted(),
                url.schema
            )));
        }
        Ok(PgSource { url, schema })
    }
}

impl SourceConnector for PgSource {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn read_table(&self, table: &str) -> Result<Box<dyn RowSource>> {
        let t = self
            .schema
            .table(table)
            .ok_or_else(|| verr(format!("unknown table {table:?}")))?
            .clone();
        // Dedicated connection per stream: the engine holds several streams
        // across its passes, and COPY occupies a whole connection.
        let cols: Vec<String> = t.columns.iter().map(|c| quote_ident(&c.name)).collect();
        let sql = format!(
            "COPY {}.{} ({}) TO STDOUT",
            quote_ident(&self.url.schema),
            quote_ident(&t.name),
            cols.join(", ")
        );
        let copy = CopyOut::start(&self.url, sql)?;
        Ok(Box::new(PgRowSource { copy, table: t }))
    }
}

struct PgRowSource {
    copy: CopyOut,
    table: Table,
}

impl RowSource for PgRowSource {
    fn next_row(&mut self) -> Result<Option<Row>> {
        let Some(line) = self.copy.next_line()? else { return Ok(None) };
        let fields = copy_split(&line)?;
        if fields.len() != self.table.columns.len() {
            return Err(rerr(format!(
                "table {:?}: COPY returned {} fields, schema has {} columns",
                self.table.name,
                fields.len(),
                self.table.columns.len()
            )));
        }
        let mut row = Vec::with_capacity(fields.len());
        for (f, col) in fields.into_iter().zip(&self.table.columns) {
            row.push(text_to_value(f, col.ty, &col.name)?);
        }
        Ok(Some(row))
    }
}

// ---------- Introspection ----------

fn introspect(client: &mut Client, schema_name: &str) -> Result<Schema> {
    let ns = quote_literal(schema_name);

    // Base tables, in name order (deterministic).
    let table_names = text_column(client, &format!(
        "SELECT table_name FROM information_schema.tables \
         WHERE table_schema = {ns} AND table_type = 'BASE TABLE' ORDER BY table_name"
    ))?;

    // Columns with exact types.
    let col_rows = text_rows(client, &format!(
        "SELECT c.table_name, c.column_name, c.udt_name, c.is_nullable, \
                format_type(a.atttypid, a.atttypmod) \
         FROM information_schema.columns c \
         JOIN pg_attribute a ON a.attrelid = (quote_ident(c.table_schema) || '.' || quote_ident(c.table_name))::regclass \
                            AND a.attname = c.column_name \
         WHERE c.table_schema = {ns} \
         ORDER BY c.table_name, c.ordinal_position"
    ))?;

    // Primary keys, unique single-column constraints.
    let pk_rows = text_rows(client, &format!(
        "SELECT tc.table_name, kcu.column_name, tc.constraint_type, tc.constraint_name \
         FROM information_schema.table_constraints tc \
         JOIN information_schema.key_column_usage kcu \
           ON kcu.constraint_name = tc.constraint_name AND kcu.constraint_schema = tc.constraint_schema \
         WHERE tc.table_schema = {ns} AND tc.constraint_type IN ('PRIMARY KEY', 'UNIQUE') \
         ORDER BY tc.table_name, tc.constraint_name, kcu.ordinal_position"
    ))?;

    // Foreign keys with correct composite pairing via position_in_unique_constraint.
    let fk_rows = text_rows(client, &format!(
        "SELECT rc.constraint_name, kcu.table_name, kcu.column_name, \
                kcu2.table_name, kcu2.column_name \
         FROM information_schema.referential_constraints rc \
         JOIN information_schema.key_column_usage kcu \
           ON kcu.constraint_name = rc.constraint_name AND kcu.constraint_schema = rc.constraint_schema \
         JOIN information_schema.key_column_usage kcu2 \
           ON kcu2.constraint_name = rc.unique_constraint_name \
          AND kcu2.constraint_schema = rc.unique_constraint_schema \
          AND kcu2.ordinal_position = kcu.position_in_unique_constraint \
         WHERE rc.constraint_schema = {ns} \
         ORDER BY rc.constraint_name, kcu.ordinal_position"
    ))?;

    // Assemble.
    let mut columns_by_table: HashMap<String, Vec<Column>> = HashMap::new();
    for r in &col_rows {
        let (Some(tname), Some(cname), Some(udt), Some(nullable), Some(exact)) =
            (&r[0], &r[1], &r[2], &r[3], &r[4])
        else {
            return Err(rerr("postgres: NULL in column introspection row"));
        };
        columns_by_table.entry(tname.clone()).or_default().push(Column {
            name: cname.clone(),
            ty: map_pg_type(udt),
            nullable: nullable == "YES",
            unique: false,
            db_type: Some(exact.clone()),
        });
    }

    let mut pks: HashMap<String, Vec<String>> = HashMap::new();
    let mut unique_constraints: HashMap<(String, String), Vec<String>> = HashMap::new();
    for r in &pk_rows {
        let (Some(tname), Some(cname), Some(kind), Some(constraint)) = (&r[0], &r[1], &r[2], &r[3])
        else {
            continue;
        };
        if kind == "PRIMARY KEY" {
            pks.entry(tname.clone()).or_default().push(cname.clone());
        } else {
            unique_constraints
                .entry((tname.clone(), constraint.clone()))
                .or_default()
                .push(cname.clone());
        }
    }

    let mut fks: HashMap<String, Vec<(String, ForeignKey)>> = HashMap::new();
    let mut fk_order: Vec<(String, String)> = Vec::new(); // (child table, constraint)
    let mut fk_map: HashMap<(String, String), ForeignKey> = HashMap::new();
    for r in &fk_rows {
        let (Some(constraint), Some(child_t), Some(child_c), Some(parent_t), Some(parent_c)) =
            (&r[0], &r[1], &r[2], &r[3], &r[4])
        else {
            continue;
        };
        let key = (child_t.clone(), constraint.clone());
        let entry = fk_map.entry(key.clone()).or_insert_with(|| {
            fk_order.push(key.clone());
            ForeignKey {
                columns: Vec::new(),
                references_table: parent_t.clone(),
                references_columns: Vec::new(),
            }
        });
        entry.columns.push(child_c.clone());
        entry.references_columns.push(parent_c.clone());
    }
    for (child_t, constraint) in fk_order {
        let fk = fk_map.remove(&(child_t.clone(), constraint)).unwrap();
        fks.entry(child_t).or_default().push((String::new(), fk));
    }

    let mut tables_json = Vec::new();
    for tname in &table_names {
        let mut cols = columns_by_table.remove(tname).unwrap_or_default();
        // Mark single-column UNIQUE constraints on the column.
        for ((ut, _), ucols) in &unique_constraints {
            if ut == tname && ucols.len() == 1 {
                if let Some(c) = cols.iter_mut().find(|c| c.name == ucols[0]) {
                    c.unique = true;
                }
            }
        }
        tables_json.push((
            tname.clone(),
            cols,
            pks.get(tname).cloned().unwrap_or_default(),
            fks.remove(tname).map(|v| v.into_iter().map(|(_, fk)| fk).collect()).unwrap_or_default(),
        ));
    }

    // Route through the JSON constructor so all Schema invariants (and the
    // PK-implies-unique rule) apply exactly as for file datasets.
    let json = crate::json::Json::Object(vec![(
        "tables".to_string(),
        crate::json::Json::Array(
            tables_json
                .into_iter()
                .map(|(name, cols, pk, fks): (String, Vec<Column>, Vec<String>, Vec<ForeignKey>)| {
                    let t = Table::assemble(name, cols, pk, fks);
                    crate::schema::table_to_json_pub(&t)
                })
                .collect(),
        ),
    )]);
    Schema::from_json(&json)
}

// ---------- Destination ----------

pub struct PgDestination {
    url: PgUrl,
    client: Client,
    schema: Option<Schema>,
    /// Tables whose DDL ran (dropped + created) this run.
    created: Vec<String>,
}

impl PgDestination {
    pub fn connect(url_text: &str) -> Result<PgDestination> {
        let url = PgUrl::parse(url_text)?;
        let client = url.connect()?;
        Ok(PgDestination { url, client, schema: None, created: Vec::new() })
    }

    fn run(&mut self, sql: &str) -> Result<()> {
        text_rows(&mut self.client, sql).map(|_| ())
    }

    fn ddl_for(&self, t: &Table) -> String {
        let schema = quote_ident(&self.url.schema);
        let name = quote_ident(&t.name);
        let mut parts: Vec<String> = t
            .columns
            .iter()
            .map(|c| {
                let ty = c.db_type.clone().unwrap_or_else(|| default_pg_type(c.ty).to_string());
                let mut s = format!("{} {}", quote_ident(&c.name), ty);
                if !c.nullable {
                    s.push_str(" NOT NULL");
                }
                s
            })
            .collect();
        if !t.primary_key.is_empty() {
            let cols: Vec<String> = t.primary_key.iter().map(|c| quote_ident(c)).collect();
            parts.push(format!("PRIMARY KEY ({})", cols.join(", ")));
        }
        for c in &t.columns {
            if c.unique && !(t.primary_key.len() == 1 && t.primary_key[0] == c.name) {
                parts.push(format!("UNIQUE ({})", quote_ident(&c.name)));
            }
        }
        format!("CREATE TABLE {schema}.{name} ({})", parts.join(", "))
    }
}

impl DestinationConnector for PgDestination {
    fn write_schema(&mut self, schema: &Schema) -> Result<()> {
        // DDL runs lazily per table in open_table so that
        // preserve_destination tables are never touched; here we just make
        // sure the target schema exists and remember the plan.
        self.run(&format!(
            "CREATE SCHEMA IF NOT EXISTS {}",
            quote_ident(&self.url.schema)
        ))?;
        self.schema = Some(schema.clone());
        Ok(())
    }

    fn open_table(&mut self, table: &Table) -> Result<Box<dyn RowSink>> {
        let schema = quote_ident(&self.url.schema);
        let name = quote_ident(&table.name);
        self.run(&format!("DROP TABLE IF EXISTS {schema}.{name} CASCADE"))?;
        let ddl = self.ddl_for(table);
        self.run(&ddl)?;
        self.created.push(table.name.clone());

        // COPY occupies the whole connection; give the sink its own so the
        // destination connection stays usable for DDL of later tables.
        let cols: Vec<String> = table.columns.iter().map(|c| quote_ident(&c.name)).collect();
        let copy = CopyIn::start(
            &self.url,
            format!("COPY {schema}.{name} ({}) FROM STDIN", cols.join(", ")),
        )?;
        Ok(Box::new(PgRowSink { copy, line: Vec::new(), done: false }))
    }

    fn has_table_data(&self, table: &str) -> bool {
        let Ok(mut client) = self.url.connect() else { return false };
        // Probe existence first (a SELECT from a missing table errors).
        let regclass = quote_literal(&format!(
            "{}.{}",
            quote_ident(&self.url.schema),
            quote_ident(table)
        ));
        let exists = text_column(
            &mut client,
            &format!("SELECT CASE WHEN to_regclass({regclass}) IS NULL THEN 'f' ELSE 't' END"),
        )
        .map(|r| r.first().map(|s| s == "t").unwrap_or(false))
        .unwrap_or(false);
        if !exists {
            return false;
        }
        text_column(&mut client, &format!(
            "SELECT CASE WHEN EXISTS (SELECT 1 FROM {}.{} LIMIT 1) THEN 't' ELSE 'f' END",
            quote_ident(&self.url.schema),
            quote_ident(table)
        ))
        .map(|r| r.first().map(|s| s == "t").unwrap_or(false))
        .unwrap_or(false)
    }

    fn finalize(&mut self) -> Result<()> {
        // Add FK constraints after all data has landed, for tables we
        // created this run.
        let Some(schema) = self.schema.clone() else { return Ok(()) };
        let ns = quote_ident(&self.url.schema);
        for t in &schema.tables {
            if !self.created.contains(&t.name) {
                continue;
            }
            for (i, fk) in t.foreign_keys.iter().enumerate() {
                let cols: Vec<String> = fk.columns.iter().map(|c| quote_ident(c)).collect();
                let refs: Vec<String> =
                    fk.references_columns.iter().map(|c| quote_ident(c)).collect();
                let cname = quote_ident(&format!("altonic_fk_{}_{}", t.name, i));
                self.run(&format!(
                    "ALTER TABLE {ns}.{} ADD CONSTRAINT {cname} FOREIGN KEY ({}) REFERENCES {ns}.{} ({})",
                    quote_ident(&t.name),
                    cols.join(", "),
                    quote_ident(&fk.references_table),
                    refs.join(", ")
                ))?;
            }
        }
        Ok(())
    }
}

struct PgRowSink {
    copy: CopyIn,
    line: Vec<u8>,
    done: bool,
}

impl RowSink for PgRowSink {
    fn write_row(&mut self, row: &Row) -> Result<()> {
        self.line.clear();
        for (i, v) in row.iter().enumerate() {
            if i > 0 {
                self.line.push(b'\t');
            }
            match value_to_text(v) {
                None => self.line.extend_from_slice(b"\\N"),
                Some(s) => copy_escape(&s, &mut self.line),
            }
        }
        self.copy.write_line(&self.line)
    }

    fn finish(&mut self) -> Result<()> {
        if !self.done {
            self.done = true;
            self.copy.finish()?;
        }
        Ok(())
    }
}
