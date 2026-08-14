//! Schema model: tables, columns, primary keys, foreign keys, uniques
//! (SPEC §3.2). Parsed from `schema.json` in the dataset directory.

use crate::json::Json;
use crate::value::ColumnType;
use crate::{verr, Result};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub ty: ColumnType,
    pub nullable: bool,
    pub unique: bool,
}

#[derive(Debug, Clone)]
pub struct ForeignKey {
    /// Referencing (child) columns, in this table.
    pub columns: Vec<String>,
    pub references_table: String,
    pub references_columns: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    pub primary_key: Vec<String>,
    pub foreign_keys: Vec<ForeignKey>,
    col_index: HashMap<String, usize>,
}

impl Table {
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.col_index.get(name).copied()
    }

    pub fn column(&self, name: &str) -> Option<&Column> {
        self.column_index(name).map(|i| &self.columns[i])
    }

    /// Indices of the primary key columns (empty if no PK).
    pub fn pk_indices(&self) -> Vec<usize> {
        self.primary_key.iter().filter_map(|c| self.column_index(c)).collect()
    }
}

#[derive(Debug, Clone)]
pub struct Schema {
    pub tables: Vec<Table>,
    table_index: HashMap<String, usize>,
}

impl Schema {
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.table_index.get(name).map(|&i| &self.tables[i])
    }

    pub fn table_names(&self) -> impl Iterator<Item = &str> {
        self.tables.iter().map(|t| t.name.as_str())
    }

    /// FKs across the whole schema pointing AT the given table.
    pub fn referencing_fks(&self, target: &str) -> Vec<(&Table, &ForeignKey)> {
        let mut out = Vec::new();
        for t in &self.tables {
            for fk in &t.foreign_keys {
                if fk.references_table == target {
                    out.push((t, fk));
                }
            }
        }
        out
    }

    pub fn from_json(root: &Json) -> Result<Schema> {
        let tables_json = root
            .get("tables")
            .and_then(|t| t.as_array())
            .ok_or_else(|| verr("schema: missing \"tables\" array"))?;
        let mut tables = Vec::new();
        let mut table_index = HashMap::new();
        for tj in tables_json {
            let table = parse_table(tj)?;
            if table_index.insert(table.name.clone(), tables.len()).is_some() {
                return Err(verr(format!("schema: duplicate table {:?}", table.name)));
            }
            tables.push(table);
        }
        let schema = Schema { tables, table_index };
        schema.validate()?;
        Ok(schema)
    }

    fn validate(&self) -> Result<()> {
        for t in &self.tables {
            for pk_col in &t.primary_key {
                if t.column(pk_col).is_none() {
                    return Err(verr(format!(
                        "schema: table {:?} primary key references unknown column {pk_col:?}",
                        t.name
                    )));
                }
            }
            for fk in &t.foreign_keys {
                let parent = self.table(&fk.references_table).ok_or_else(|| {
                    verr(format!(
                        "schema: table {:?} foreign key references unknown table {:?}",
                        t.name, fk.references_table
                    ))
                })?;
                if fk.columns.len() != fk.references_columns.len() || fk.columns.is_empty() {
                    return Err(verr(format!(
                        "schema: table {:?} foreign key column count mismatch",
                        t.name
                    )));
                }
                for c in &fk.columns {
                    if t.column(c).is_none() {
                        return Err(verr(format!(
                            "schema: table {:?} foreign key uses unknown column {c:?}",
                            t.name
                        )));
                    }
                }
                for c in &fk.references_columns {
                    if parent.column(c).is_none() {
                        return Err(verr(format!(
                            "schema: foreign key {:?}.{:?} references unknown column {:?}.{c:?}",
                            t.name, fk.columns, parent.name
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn to_json(&self) -> Json {
        Json::Array(self.tables.iter().map(table_to_json).collect()).wrap_tables()
    }
}

trait WrapTables {
    fn wrap_tables(self) -> Json;
}
impl WrapTables for Json {
    fn wrap_tables(self) -> Json {
        Json::Object(vec![("tables".to_string(), self)])
    }
}

fn table_to_json(t: &Table) -> Json {
    let mut members = vec![
        ("name".to_string(), Json::Str(t.name.clone())),
        (
            "columns".to_string(),
            Json::Array(
                t.columns
                    .iter()
                    .map(|c| {
                        let mut m = vec![
                            ("name".to_string(), Json::Str(c.name.clone())),
                            ("type".to_string(), Json::Str(c.ty.name().to_string())),
                        ];
                        if !c.nullable {
                            m.push(("nullable".to_string(), Json::Bool(false)));
                        }
                        if c.unique {
                            m.push(("unique".to_string(), Json::Bool(true)));
                        }
                        Json::Object(m)
                    })
                    .collect(),
            ),
        ),
    ];
    if !t.primary_key.is_empty() {
        members.push((
            "primary_key".to_string(),
            Json::Array(t.primary_key.iter().map(|c| Json::Str(c.clone())).collect()),
        ));
    }
    if !t.foreign_keys.is_empty() {
        members.push((
            "foreign_keys".to_string(),
            Json::Array(
                t.foreign_keys
                    .iter()
                    .map(|fk| {
                        Json::Object(vec![
                            (
                                "columns".to_string(),
                                Json::Array(fk.columns.iter().map(|c| Json::Str(c.clone())).collect()),
                            ),
                            (
                                "references_table".to_string(),
                                Json::Str(fk.references_table.clone()),
                            ),
                            (
                                "references_columns".to_string(),
                                Json::Array(
                                    fk.references_columns.iter().map(|c| Json::Str(c.clone())).collect(),
                                ),
                            ),
                        ])
                    })
                    .collect(),
            ),
        ));
    }
    Json::Object(members)
}

fn parse_table(tj: &Json) -> Result<Table> {
    let name = tj
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| verr("schema: table missing \"name\""))?
        .to_string();
    let cols_json = tj
        .get("columns")
        .and_then(|c| c.as_array())
        .ok_or_else(|| verr(format!("schema: table {name:?} missing \"columns\"")))?;
    let mut columns = Vec::new();
    let mut col_index = HashMap::new();
    let mut seen = HashSet::new();
    for cj in cols_json {
        let cname = cj
            .get("name")
            .and_then(|n| n.as_str())
            .ok_or_else(|| verr(format!("schema: column in table {name:?} missing \"name\"")))?
            .to_string();
        if !seen.insert(cname.clone()) {
            return Err(verr(format!("schema: duplicate column {cname:?} in table {name:?}")));
        }
        let ty_str = cj
            .get("type")
            .and_then(|t| t.as_str())
            .ok_or_else(|| verr(format!("schema: column {name:?}.{cname:?} missing \"type\"")))?;
        let ty = ColumnType::parse(ty_str)
            .ok_or_else(|| verr(format!("schema: unknown column type {ty_str:?} on {name:?}.{cname:?}")))?;
        let nullable = cj.get("nullable").and_then(|n| n.as_bool()).unwrap_or(true);
        let unique = cj.get("unique").and_then(|u| u.as_bool()).unwrap_or(false);
        col_index.insert(cname.clone(), columns.len());
        columns.push(Column { name: cname, ty, nullable, unique });
    }
    let primary_key = match tj.get("primary_key") {
        Some(Json::Array(items)) => items
            .iter()
            .map(|i| {
                i.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| verr(format!("schema: table {name:?} primary_key must be strings")))
            })
            .collect::<Result<Vec<_>>>()?,
        None => Vec::new(),
        Some(_) => return Err(verr(format!("schema: table {name:?} primary_key must be an array"))),
    };
    let foreign_keys = match tj.get("foreign_keys") {
        Some(Json::Array(items)) => items
            .iter()
            .map(|fj| {
                let columns = str_array(fj.get("columns"), &name, "foreign key columns")?;
                let references_table = fj
                    .get("references_table")
                    .and_then(|r| r.as_str())
                    .ok_or_else(|| verr(format!("schema: table {name:?} FK missing references_table")))?
                    .to_string();
                let references_columns =
                    str_array(fj.get("references_columns"), &name, "foreign key references_columns")?;
                Ok(ForeignKey { columns, references_table, references_columns })
            })
            .collect::<Result<Vec<_>>>()?,
        None => Vec::new(),
        Some(_) => return Err(verr(format!("schema: table {name:?} foreign_keys must be an array"))),
    };
    // PK columns are implicitly non-nullable and (as a whole) unique; a
    // single-column PK marks that column unique for generator validation.
    let mut table = Table { name, columns, primary_key, foreign_keys, col_index };
    if table.primary_key.len() == 1 {
        let pk = table.primary_key[0].clone();
        if let Some(i) = table.column_index(&pk) {
            table.columns[i].unique = true;
            table.columns[i].nullable = false;
        }
    }
    Ok(table)
}

fn str_array(j: Option<&Json>, table: &str, what: &str) -> Result<Vec<String>> {
    j.and_then(|a| a.as_array())
        .ok_or_else(|| verr(format!("schema: table {table:?} {what} must be an array")))?
        .iter()
        .map(|i| {
            i.as_str()
                .map(str::to_string)
                .ok_or_else(|| verr(format!("schema: table {table:?} {what} must be strings")))
        })
        .collect()
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn demo_schema_json() -> &'static str {
        r#"{
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
              "name": "customers",
              "columns": [
                {"name": "id", "type": "integer"},
                {"name": "email", "type": "text", "unique": true},
                {"name": "state_code", "type": "text"}
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
                {"name": "customer_id", "type": "integer", "nullable": false}
              ],
              "primary_key": ["id"],
              "foreign_keys": [
                {"columns": ["customer_id"], "references_table": "customers", "references_columns": ["id"]}
              ]
            }
          ]
        }"#
    }

    #[test]
    fn parses_and_validates() {
        let j = Json::parse(demo_schema_json()).unwrap();
        let s = Schema::from_json(&j).unwrap();
        assert_eq!(s.tables.len(), 3);
        let customers = s.table("customers").unwrap();
        assert!(customers.column("id").unwrap().unique); // implicit via PK
        assert!(!customers.column("id").unwrap().nullable);
        assert_eq!(s.referencing_fks("customers").len(), 1);
        assert_eq!(s.referencing_fks("states").len(), 1);
    }

    #[test]
    fn rejects_bad_refs() {
        let bad = r#"{"tables": [{"name": "a", "columns": [{"name": "x", "type": "integer"}],
            "foreign_keys": [{"columns": ["x"], "references_table": "missing", "references_columns": ["y"]}]}]}"#;
        let j = Json::parse(bad).unwrap();
        assert!(Schema::from_json(&j).is_err());
    }

    #[test]
    fn round_trips_to_json() {
        let j = Json::parse(demo_schema_json()).unwrap();
        let s = Schema::from_json(&j).unwrap();
        let j2 = s.to_json();
        let s2 = Schema::from_json(&j2).unwrap();
        assert_eq!(s2.tables.len(), s.tables.len());
        assert_eq!(s2.table("orders").unwrap().foreign_keys.len(), 1);
    }
}
