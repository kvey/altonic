//! Connector traits (SPEC §7) and the v1 directory-dataset connector:
//! a dataset is a directory containing `schema.json` plus one `<table>.csv`
//! per table, with a header row in the table's column order.

use crate::csv::{CsvReader, CsvWriter};
use crate::json::Json;
use crate::schema::{Schema, Table};
use crate::value::{Row, Value};
use crate::{rerr, verr, Result};
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

/// Streaming row source for one table. Must be re-openable via
/// `SourceConnector::read_table` because the pipeline makes multiple passes.
pub trait RowSource {
    fn next_row(&mut self) -> Result<Option<Row>>;
}

pub trait SourceConnector {
    fn schema(&self) -> &Schema;
    fn read_table(&self, table: &str) -> Result<Box<dyn RowSource>>;
    /// Total row count (used for reporting; may stream-count).
    fn count_rows(&self, table: &str) -> Result<usize> {
        let mut n = 0;
        let mut src = self.read_table(table)?;
        while src.next_row()?.is_some() {
            n += 1;
        }
        Ok(n)
    }
}

pub trait DestinationConnector {
    fn write_schema(&mut self, schema: &Schema) -> Result<()>;
    fn open_table(&mut self, table: &Table) -> Result<Box<dyn RowSink>>;
    /// Whether the destination already holds data for this table
    /// (drives `preserve_destination` mode).
    fn has_table_data(&self, table: &str) -> bool;
    /// Called once after all tables are written; database destinations add
    /// deferred constraints (foreign keys) here.
    fn finalize(&mut self) -> Result<()> {
        Ok(())
    }
}

pub trait RowSink {
    fn write_row(&mut self, row: &Row) -> Result<()>;
    fn finish(&mut self) -> Result<()>;
}

// ---------- Directory dataset ----------

pub struct DirDataset {
    pub root: PathBuf,
    schema: Schema,
}

impl DirDataset {
    pub fn open(root: impl AsRef<Path>) -> Result<DirDataset> {
        let root = root.as_ref().to_path_buf();
        let schema_path = root.join("schema.json");
        let text = std::fs::read_to_string(&schema_path)
            .map_err(|e| rerr(format!("cannot read {}: {e}", schema_path.display())))?;
        let json = Json::parse(&text)
            .map_err(|e| verr(format!("{}: {e}", schema_path.display())))?;
        let schema = Schema::from_json(&json)?;
        Ok(DirDataset { root, schema })
    }

    pub fn table_path(&self, table: &str) -> PathBuf {
        self.root.join(format!("{table}.csv"))
    }
}

impl SourceConnector for DirDataset {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn read_table(&self, table: &str) -> Result<Box<dyn RowSource>> {
        let t = self
            .schema
            .table(table)
            .ok_or_else(|| verr(format!("unknown table {table:?}")))?;
        let path = self.table_path(table);
        let file = File::open(&path)
            .map_err(|e| rerr(format!("cannot open {}: {e}", path.display())))?;
        let mut reader = CsvReader::new(BufReader::new(file));
        // Validate header.
        let header = reader
            .next_record()
            .map_err(|e| rerr(format!("{}: {e}", path.display())))?
            .ok_or_else(|| rerr(format!("{}: missing header row", path.display())))?;
        let expected: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
        let got: Vec<&str> = header.iter().map(|f| f.text.as_str()).collect();
        if expected != got {
            return Err(verr(format!(
                "{}: header {:?} does not match schema columns {:?}",
                path.display(),
                got,
                expected
            )));
        }
        Ok(Box::new(CsvRowSource {
            reader,
            table: t.clone(),
            path,
            line: 1,
        }))
    }
}

struct CsvRowSource {
    reader: CsvReader<BufReader<File>>,
    table: Table,
    path: PathBuf,
    line: usize,
}

impl RowSource for CsvRowSource {
    fn next_row(&mut self) -> Result<Option<Row>> {
        let rec = match self
            .reader
            .next_record()
            .map_err(|e| rerr(format!("{}: {e}", self.path.display())))?
        {
            Some(r) => r,
            None => return Ok(None),
        };
        self.line += 1;
        if rec.len() != self.table.columns.len() {
            return Err(rerr(format!(
                "{} record {}: expected {} fields, found {}",
                self.path.display(),
                self.line,
                self.table.columns.len(),
                rec.len()
            )));
        }
        let mut row = Vec::with_capacity(rec.len());
        for (field, col) in rec.iter().zip(&self.table.columns) {
            let v = Value::from_csv(&field.text, field.quoted, col.ty).map_err(|e| {
                rerr(format!(
                    "{} record {} column {:?}: {e}",
                    self.path.display(),
                    self.line,
                    col.name
                ))
            })?;
            if v.is_null() && !col.nullable {
                return Err(rerr(format!(
                    "{} record {}: NULL in non-nullable column {:?}",
                    self.path.display(),
                    self.line,
                    col.name
                )));
            }
            row.push(v);
        }
        Ok(Some(row))
    }
}

/// Destination over a directory: writes schema.json and per-table CSVs.
pub struct DirDestination {
    pub root: PathBuf,
}

impl DirDestination {
    pub fn create(root: impl AsRef<Path>) -> Result<DirDestination> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)
            .map_err(|e| rerr(format!("cannot create {}: {e}", root.display())))?;
        Ok(DirDestination { root })
    }
}

impl DestinationConnector for DirDestination {
    fn write_schema(&mut self, schema: &Schema) -> Result<()> {
        let path = self.root.join("schema.json");
        std::fs::write(&path, schema.to_json().to_string_pretty())
            .map_err(|e| rerr(format!("cannot write {}: {e}", path.display())))?;
        Ok(())
    }

    fn open_table(&mut self, table: &Table) -> Result<Box<dyn RowSink>> {
        let path = self.root.join(format!("{}.csv", table.name));
        let file = File::create(&path)
            .map_err(|e| rerr(format!("cannot create {}: {e}", path.display())))?;
        let mut writer = CsvWriter::new(BufWriter::new(file));
        writer
            .write_record(table.columns.iter().map(|c| (c.name.as_str(), false)))
            .map_err(|e| rerr(format!("{}: {e}", path.display())))?;
        Ok(Box::new(CsvRowSink { writer, path }))
    }

    fn has_table_data(&self, table: &str) -> bool {
        let path = self.root.join(format!("{table}.csv"));
        match std::fs::metadata(&path) {
            Ok(m) => m.len() > 0,
            Err(_) => false,
        }
    }
}

struct CsvRowSink {
    writer: CsvWriter<BufWriter<File>>,
    path: PathBuf,
}

impl RowSink for CsvRowSink {
    fn write_row(&mut self, row: &Row) -> Result<()> {
        let cells: Vec<(String, bool)> = row.iter().map(|v| v.to_csv()).collect();
        self.writer
            .write_record(cells.iter().map(|(s, q)| (s.as_str(), *q)))
            .map_err(|e| rerr(format!("{}: {e}", self.path.display())))?;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.writer.flush().map_err(|e| rerr(format!("{}: {e}", self.path.display())))
    }
}

/// Read an entire table into memory (used by the subsetter's key indexes and
/// by tests). Large tables stream through the engine instead.
pub fn read_all_rows(src: &dyn SourceConnector, table: &str) -> Result<Vec<Row>> {
    let mut out = Vec::new();
    let mut rs = src.read_table(table)?;
    while let Some(r) = rs.next_row()? {
        out.push(r);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_demo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("schema.json"), crate::schema::tests::demo_schema_json()).unwrap();
        std::fs::write(dir.join("states.csv"), "code,name\nCA,California\nNY,New York\n").unwrap();
        std::fs::write(
            dir.join("customers.csv"),
            "id,email,state_code\n1,a@x.com,CA\n2,b@y.com,\n",
        )
        .unwrap();
        std::fs::write(dir.join("orders.csv"), "id,customer_id\n10,1\n11,2\n").unwrap();
    }

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("altonic-conn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_demo(&dir);
        let ds = DirDataset::open(&dir).unwrap();
        let rows = read_all_rows(&ds, "customers").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][1], Value::Text("a@x.com".into()));
        assert_eq!(rows[1][2], Value::Null);

        // Write out and re-read.
        let out = dir.join("dest");
        let mut dest = DirDestination::create(&out).unwrap();
        dest.write_schema(ds.schema()).unwrap();
        let t = ds.schema().table("customers").unwrap().clone();
        {
            let mut sink = dest.open_table(&t).unwrap();
            for r in &rows {
                sink.write_row(r).unwrap();
            }
            sink.finish().unwrap();
        }
        let ds2 = DirDataset::open(&out).unwrap();
        let rows2 = read_all_rows(&ds2, "customers").unwrap();
        assert_eq!(rows, rows2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn header_mismatch_rejected() {
        let dir = std::env::temp_dir().join(format!("altonic-conn2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_demo(&dir);
        std::fs::write(dir.join("orders.csv"), "id,wrong\n1,2\n").unwrap();
        let ds = DirDataset::open(&dir).unwrap();
        assert!(ds.read_table("orders").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn null_in_non_nullable_rejected() {
        let dir = std::env::temp_dir().join(format!("altonic-conn3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_demo(&dir);
        std::fs::write(dir.join("orders.csv"), "id,customer_id\n10,\n").unwrap();
        let ds = DirDataset::open(&dir).unwrap();
        let mut rs = ds.read_table("orders").unwrap();
        assert!(rs.next_row().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
