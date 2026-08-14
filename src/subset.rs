//! Subsetter (SPEC §8): compute a referentially-closed set of rows per table
//! from target filters, lookup tables, and optional upstream rules.
//!
//! FK edges point child → parent. Parents of included rows are *required*
//! (downstream closure); children referencing included rows are *optional*
//! (upstream rules). The closure is a monotone worklist fixpoint, so circular
//! and self-referencing FK chains terminate.

use crate::config::{OutOfSubset, SubsetConfig, TargetFilter};
use crate::connector::{read_all_rows, SourceConnector};
use crate::crypto;
use crate::expr::Expr;
use crate::schema::{Schema, Table};
use crate::value::Row;
use crate::{verr, Result};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableRole {
    Target,
    Lookup,
    Related,
    OutOfSubset,
}

#[derive(Debug)]
pub struct TableSubset {
    pub role: TableRole,
    /// Included row ordinals (0-based, in source order). For Lookup tables
    /// this is all rows.
    pub rows: HashSet<usize>,
    pub total: usize,
}

#[derive(Debug)]
pub struct SubsetResult {
    pub tables: HashMap<String, TableSubset>,
    pub out_of_subset: OutOfSubset,
}

impl SubsetResult {
    /// Whether a given source row survives subsetting.
    pub fn includes(&self, table: &str, ordinal: usize) -> bool {
        match self.tables.get(table) {
            Some(ts) => ts.rows.contains(&ordinal),
            None => false,
        }
    }

    pub fn role(&self, table: &str) -> TableRole {
        self.tables.get(table).map(|t| t.role).unwrap_or(TableRole::OutOfSubset)
    }
}

/// Validate the subset configuration against a schema (tables exist, where
/// clauses parse and reference real columns).
pub fn validate(cfg: &SubsetConfig, schema: &Schema) -> Result<()> {
    let check_where = |table: &str, src: &str| -> Result<()> {
        let t = schema
            .table(table)
            .ok_or_else(|| verr(format!("subset: unknown table {table:?}")))?;
        let expr = Expr::parse(src)?;
        let mut cols = Vec::new();
        expr.columns(&mut cols);
        for c in cols {
            if t.column(&c).is_none() {
                return Err(verr(format!(
                    "subset: where clause for {table:?} references unknown column {c:?}"
                )));
            }
        }
        Ok(())
    };
    for t in &cfg.targets {
        match &t.filter {
            TargetFilter::Percent(_) => {
                if schema.table(&t.table).is_none() {
                    return Err(verr(format!("subset: unknown target table {:?}", t.table)));
                }
            }
            TargetFilter::Where(w) => check_where(&t.table, w)?,
        }
    }
    for l in &cfg.lookup_tables {
        if schema.table(l).is_none() {
            return Err(verr(format!("subset: unknown lookup table {l:?}")));
        }
    }
    for u in &cfg.upstream {
        if schema.table(&u.table).is_none() {
            return Err(verr(format!("subset: unknown upstream table {:?}", u.table)));
        }
        if let Some(w) = &u.where_clause {
            check_where(&u.table, w)?;
        }
    }
    Ok(())
}

/// Key for locating parent rows: canonical forms of the referenced columns.
fn key_of(row: &Row, indices: &[usize]) -> Option<Vec<String>> {
    let mut key = Vec::with_capacity(indices.len());
    for &i in indices {
        if row[i].is_null() {
            return None; // null FK: nothing to require
        }
        key.push(row[i].canonical());
    }
    Some(key)
}

struct Loaded {
    rows: Vec<Row>,
}

type KeyIndexes = HashMap<(String, Vec<String>), HashMap<Vec<String>, Vec<usize>>>;

/// Build (or reuse) the lookup index from referenced-column values to row
/// ordinals of `table`. Cloned out to keep borrows simple; indexes are built
/// once per (table, columns) pair.
fn index_for(
    key_indexes: &mut KeyIndexes,
    table: &Table,
    ref_cols: &[String],
    data: &HashMap<String, Loaded>,
) -> Result<HashMap<Vec<String>, Vec<usize>>> {
    let key = (table.name.clone(), ref_cols.to_vec());
    if let Some(idx) = key_indexes.get(&key) {
        return Ok(idx.clone());
    }
    let indices: Vec<usize> = ref_cols
        .iter()
        .map(|c| {
            table
                .column_index(c)
                .ok_or_else(|| verr(format!("subset: unknown column {c:?} in {:?}", table.name)))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut map: HashMap<Vec<String>, Vec<usize>> = HashMap::new();
    for (i, row) in data[&table.name].rows.iter().enumerate() {
        if let Some(k) = key_of(row, &indices) {
            map.entry(k).or_default().push(i);
        }
    }
    key_indexes.insert(key.clone(), map);
    Ok(key_indexes[&key].clone())
}

/// Drain the worklist, pulling in every parent row required by a foreign key
/// of an included row, until fixpoint.
fn close_downstream(
    schema: &Schema,
    data: &HashMap<String, Loaded>,
    key_indexes: &mut KeyIndexes,
    worklist: &mut Vec<(String, usize)>,
    included: &mut HashMap<String, HashSet<usize>>,
    roles: &mut HashMap<String, TableRole>,
) -> Result<()> {
    while let Some((tname, ordinal)) = worklist.pop() {
        let t = schema.table(&tname).unwrap();
        let row = &data[&tname].rows[ordinal];
        for fk in &t.foreign_keys {
            let child_indices: Vec<usize> =
                fk.columns.iter().map(|c| t.column_index(c).unwrap()).collect();
            let key = match key_of(row, &child_indices) {
                None => continue, // null FK
                Some(k) => k,
            };
            let parent = schema.table(&fk.references_table).unwrap();
            let idx = index_for(key_indexes, parent, &fk.references_columns, data)?;
            let matches = idx.get(&key).cloned().unwrap_or_default();
            if matches.is_empty() {
                return Err(crate::rerr(format!(
                    "subset: dangling foreign key {}.{:?} -> {}.{:?} for key {:?} (row {} of {})",
                    tname,
                    fk.columns,
                    parent.name,
                    fk.references_columns,
                    key,
                    ordinal + 1,
                    tname
                )));
            }
            for m in matches {
                roles.entry(parent.name.clone()).or_insert(TableRole::Related);
                if included.entry(parent.name.clone()).or_default().insert(m) {
                    worklist.push((parent.name.clone(), m));
                }
            }
        }
    }
    Ok(())
}

/// Compute the subset. Loads participating tables into memory (v1 connector
/// scale; see SPEC §8).
pub fn compute(
    src: &dyn SourceConnector,
    cfg: &SubsetConfig,
    secret: &[u8],
) -> Result<SubsetResult> {
    let schema = src.schema();
    validate(cfg, schema)?;

    // Load every table once. Row ordinals are positions in source order.
    let mut data: HashMap<String, Loaded> = HashMap::new();
    for t in &schema.tables {
        data.insert(t.name.clone(), Loaded { rows: read_all_rows(src, &t.name)? });
    }

    let mut included: HashMap<String, HashSet<usize>> = HashMap::new();
    let mut roles: HashMap<String, TableRole> = HashMap::new();
    let mut worklist: Vec<(String, usize)> = Vec::new();

    let include = |table: &str,
                       ordinal: usize,
                       included: &mut HashMap<String, HashSet<usize>>,
                       worklist: &mut Vec<(String, usize)>| {
        if included.entry(table.to_string()).or_default().insert(ordinal) {
            worklist.push((table.to_string(), ordinal));
        }
    };

    // 1. Lookup tables: everything.
    for l in &cfg.lookup_tables {
        roles.insert(l.clone(), TableRole::Lookup);
        let n = data[l].rows.len();
        for i in 0..n {
            include(l, i, &mut included, &mut worklist);
        }
    }

    // 2. Targets.
    for target in &cfg.targets {
        let t = schema.table(&target.table).unwrap();
        roles.insert(t.name.clone(), TableRole::Target);
        let rows = &data[&t.name].rows;
        match &target.filter {
            TargetFilter::Percent(p) => {
                let pk = t.pk_indices();
                let threshold = ((p / 100.0) * (u64::MAX as f64)) as u64;
                for (i, row) in rows.iter().enumerate() {
                    // Deterministic hash sampling keyed on the row identity:
                    // stable across runs and row reordering.
                    let identity = if pk.is_empty() {
                        row.iter().map(|v| v.canonical()).collect::<Vec<_>>().join("\u{1}")
                    } else {
                        pk.iter().map(|&c| row[c].canonical()).collect::<Vec<_>>().join("\u{1}")
                    };
                    let digest = crypto::hmac_parts(
                        secret,
                        &[b"subset", t.name.as_bytes(), identity.as_bytes()],
                    );
                    if crypto::digest_to_u64(&digest) <= threshold {
                        include(&t.name, i, &mut included, &mut worklist);
                    }
                }
            }
            TargetFilter::Where(w) => {
                let expr = Expr::parse(w)?;
                for (i, row) in rows.iter().enumerate() {
                    let ok = expr.eval_bool(&|name| {
                        t.column_index(name).map(|ci| row[ci].clone())
                    })?;
                    if ok {
                        include(&t.name, i, &mut included, &mut worklist);
                    }
                }
            }
        }
    }

    // Lazy parent-key indexes: (table, referenced columns) -> key -> ordinals.
    let mut key_indexes: KeyIndexes = HashMap::new();

    // 3. Downstream closure to fixpoint.
    close_downstream(schema, &data, &mut key_indexes, &mut worklist, &mut included, &mut roles)?;

    // 4. Upstream rules: children referencing included rows, filtered.
    for rule in &cfg.upstream {
        let t = schema.table(&rule.table).unwrap();
        let expr = rule.where_clause.as_deref().map(Expr::parse).transpose()?;
        let rows = &data[&t.name].rows;
        let mut to_add = Vec::new();
        for (i, row) in rows.iter().enumerate() {
            if included.get(&t.name).map(|s| s.contains(&i)).unwrap_or(false) {
                continue;
            }
            // References any included parent row? Lookup tables don't count:
            // they are included wholesale for reference integrity, and
            // treating them as anchors would pull in every child row.
            let mut references_subset = false;
            for fk in &t.foreign_keys {
                if cfg.lookup_tables.contains(&fk.references_table) {
                    continue;
                }
                let child_indices: Vec<usize> =
                    fk.columns.iter().map(|c| t.column_index(c).unwrap()).collect();
                let key = match key_of(row, &child_indices) {
                    None => continue,
                    Some(k) => k,
                };
                let parent = schema.table(&fk.references_table).unwrap();
                let idx = index_for(&mut key_indexes, parent, &fk.references_columns, &data)?;
                if let Some(matches) = idx.get(&key) {
                    if matches.iter().any(|m| {
                        included.get(&parent.name).map(|s| s.contains(m)).unwrap_or(false)
                    }) {
                        references_subset = true;
                        break;
                    }
                }
            }
            if !references_subset {
                continue;
            }
            if let Some(e) = &expr {
                let ok = e.eval_bool(&|name| t.column_index(name).map(|ci| row[ci].clone()))?;
                if !ok {
                    continue;
                }
            }
            to_add.push(i);
        }
        roles.entry(t.name.clone()).or_insert(TableRole::Related);
        for i in to_add {
            include(&t.name, i, &mut included, &mut worklist);
        }
        // 5. Upstream rows may require new parents.
        close_downstream(schema, &data, &mut key_indexes, &mut worklist, &mut included, &mut roles)?;
    }

    // 6. Assemble result.
    let mut tables = HashMap::new();
    for t in &schema.tables {
        let total = data[&t.name].rows.len();
        let rows = included.remove(&t.name).unwrap_or_default();
        let role = if rows.is_empty() {
            TableRole::OutOfSubset
        } else {
            *roles.get(&t.name).unwrap_or(&TableRole::Related)
        };
        tables.insert(t.name.clone(), TableSubset { role, rows, total });
    }
    Ok(SubsetResult { tables, out_of_subset: cfg.out_of_subset })
}

/// Check the closure invariant: every included child row's non-null FKs
/// reference included parent rows. Used by tests and `subset --verify`.
pub fn verify_closure(src: &dyn SourceConnector, result: &SubsetResult) -> Result<()> {
    let schema = src.schema();
    let mut cache: HashMap<String, Vec<Row>> = HashMap::new();
    for t in &schema.tables {
        cache.insert(t.name.clone(), read_all_rows(src, &t.name)?);
    }
    for t in &schema.tables {
        let Some(ts) = result.tables.get(&t.name) else { continue };
        for &ordinal in &ts.rows {
            let row = &cache[&t.name][ordinal];
            for fk in &t.foreign_keys {
                let child_indices: Vec<usize> =
                    fk.columns.iter().map(|c| t.column_index(c).unwrap()).collect();
                let Some(key) = key_of(row, &child_indices) else { continue };
                let parent = schema.table(&fk.references_table).unwrap();
                let parent_indices: Vec<usize> = fk
                    .references_columns
                    .iter()
                    .map(|c| parent.column_index(c).unwrap())
                    .collect();
                let found = cache[&parent.name].iter().enumerate().any(|(i, prow)| {
                    result.includes(&parent.name, i)
                        && key_of(prow, &parent_indices).as_ref() == Some(&key)
                });
                if !found {
                    return Err(crate::rerr(format!(
                        "subset closure violated: {}.{:?}={:?} has no included parent in {}",
                        t.name, fk.columns, key, parent.name
                    )));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SubsetTarget, UpstreamRule};
    use crate::connector::DirDataset;
    use std::path::PathBuf;

    fn demo(dirname: &str) -> (PathBuf, DirDataset) {
        let dir = std::env::temp_dir().join(format!("altonic-subset-{dirname}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("schema.json"),
            r#"{
              "tables": [
                {"name": "states", "columns": [
                  {"name": "code", "type": "text", "nullable": false},
                  {"name": "name", "type": "text"}], "primary_key": ["code"]},
                {"name": "customers", "columns": [
                  {"name": "id", "type": "integer"},
                  {"name": "state_code", "type": "text"},
                  {"name": "referred_by", "type": "integer"}],
                 "primary_key": ["id"],
                 "foreign_keys": [
                   {"columns": ["state_code"], "references_table": "states", "references_columns": ["code"]},
                   {"columns": ["referred_by"], "references_table": "customers", "references_columns": ["id"]}]},
                {"name": "orders", "columns": [
                  {"name": "id", "type": "integer"},
                  {"name": "customer_id", "type": "integer", "nullable": false},
                  {"name": "total", "type": "float"}],
                 "primary_key": ["id"],
                 "foreign_keys": [
                   {"columns": ["customer_id"], "references_table": "customers", "references_columns": ["id"]}]},
                {"name": "audit_log", "columns": [
                  {"name": "id", "type": "integer"},
                  {"name": "message", "type": "text"}], "primary_key": ["id"]}
              ]
            }"#,
        )
        .unwrap();
        std::fs::write(dir.join("states.csv"), "code,name\nCA,California\nNY,New York\nTX,Texas\n").unwrap();
        // Customer 3 was referred by 2, who was referred by 1 (self-FK chain).
        std::fs::write(
            dir.join("customers.csv"),
            "id,state_code,referred_by\n1,CA,\n2,NY,1\n3,TX,2\n4,CA,\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("orders.csv"),
            "id,customer_id,total\n100,3,50.0\n101,4,200.0\n102,1,75.0\n",
        )
        .unwrap();
        std::fs::write(dir.join("audit_log.csv"), "id,message\n1,hello\n").unwrap();
        (dir.clone(), DirDataset::open(&dir).unwrap())
    }

    fn cfg(targets: Vec<SubsetTarget>, lookups: Vec<&str>, upstream: Vec<UpstreamRule>) -> SubsetConfig {
        SubsetConfig {
            enabled: true,
            targets,
            lookup_tables: lookups.into_iter().map(str::to_string).collect(),
            upstream,
            out_of_subset: OutOfSubset::Truncate,
        }
    }

    #[test]
    fn where_target_pulls_parents_transitively() {
        let (dir, ds) = demo("where");
        let c = cfg(
            vec![SubsetTarget { table: "orders".into(), filter: TargetFilter::Where("id == 100".into()) }],
            vec!["states"],
            vec![],
        );
        let r = compute(&ds, &c, b"secret").unwrap();
        // Order 100 → customer 3 → referred_by 2 → referred_by 1.
        assert_eq!(r.tables["orders"].rows.len(), 1);
        let cust: HashSet<usize> = r.tables["customers"].rows.clone();
        assert_eq!(cust.len(), 3); // ordinals 0,1,2 (ids 1,2,3)
        assert!(!cust.contains(&3)); // customer 4 not needed
        assert_eq!(r.tables["states"].rows.len(), 3); // lookup: all
        assert_eq!(r.tables["audit_log"].rows.len(), 0);
        assert_eq!(r.role("audit_log"), TableRole::OutOfSubset);
        verify_closure(&ds, &r).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upstream_rule_adds_children_and_recloses() {
        let (dir, ds) = demo("upstream");
        let c = cfg(
            vec![SubsetTarget {
                table: "customers".into(),
                filter: TargetFilter::Where("id == 4".into()),
            }],
            vec![],
            vec![UpstreamRule { table: "orders".into(), where_clause: Some("total > 100".into()) }],
        );
        let r = compute(&ds, &c, b"secret").unwrap();
        // Customer 4 included; upstream pulls order 101 (total 200 > 100) but
        // not orders of other customers.
        assert_eq!(r.tables["orders"].rows.len(), 1);
        assert!(r.includes("orders", 1)); // ordinal 1 = id 101
        // Downstream reclose: order 101's customer (4) already in; states of
        // customer 4 pulled as related.
        assert!(!r.tables["states"].rows.is_empty());
        verify_closure(&ds, &r).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upstream_ignores_lookup_table_anchors() {
        let (dir, ds) = demo("upstream-lookup");
        // customers is a lookup here; an unrestricted upstream rule on orders
        // must NOT pull all orders in just because customers are all included
        // — only orders of non-lookup subset members would qualify, and there
        // are none (target selects nothing).
        let c = cfg(
            vec![SubsetTarget {
                table: "audit_log".into(),
                filter: TargetFilter::Where("id == 999".into()), // empty target
            }],
            vec!["customers", "states"],
            vec![UpstreamRule { table: "orders".into(), where_clause: None }],
        );
        let r = compute(&ds, &c, b"secret").unwrap();
        assert_eq!(r.tables["orders"].rows.len(), 0, "lookup FKs must not anchor upstream pulls");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn percent_sampling_is_deterministic() {
        let (dir, ds) = demo("percent");
        let c = cfg(
            vec![SubsetTarget { table: "orders".into(), filter: TargetFilter::Percent(50.0) }],
            vec![],
            vec![],
        );
        let a = compute(&ds, &c, b"secret").unwrap();
        let b = compute(&ds, &c, b"secret").unwrap();
        assert_eq!(a.tables["orders"].rows, b.tables["orders"].rows);
        // A different secret gives a (probably) different draw; at minimum it
        // must still verify closure.
        let d = compute(&ds, &c, b"other-secret").unwrap();
        verify_closure(&ds, &a).unwrap();
        verify_closure(&ds, &d).unwrap();
        // 0% and 100% are exact.
        let c0 = cfg(
            vec![SubsetTarget { table: "orders".into(), filter: TargetFilter::Percent(0.0) }],
            vec![],
            vec![],
        );
        // 0%: nothing sampled (hash <= 0 has ~0 probability; threshold 0 keeps
        // only hash==0). Allow 0 rows.
        let r0 = compute(&ds, &c0, b"secret").unwrap();
        assert!(r0.tables["orders"].rows.len() <= 1);
        let c100 = cfg(
            vec![SubsetTarget { table: "orders".into(), filter: TargetFilter::Percent(100.0) }],
            vec![],
            vec![],
        );
        let r100 = compute(&ds, &c100, b"secret").unwrap();
        assert_eq!(r100.tables["orders"].rows.len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dangling_fk_is_reported() {
        let (dir, _unused) = demo("dangling");
        std::fs::write(dir.join("orders.csv"), "id,customer_id,total\n100,999,50.0\n").unwrap();
        let ds = DirDataset::open(&dir).unwrap();
        let c = cfg(
            vec![SubsetTarget { table: "orders".into(), filter: TargetFilter::Percent(100.0) }],
            vec![],
            vec![],
        );
        let err = compute(&ds, &c, b"secret").unwrap_err();
        assert!(err.to_string().contains("dangling"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validation_rejects_unknowns() {
        let (dir, ds) = demo("validate");
        let bad = cfg(
            vec![SubsetTarget { table: "ghost".into(), filter: TargetFilter::Percent(10.0) }],
            vec![],
            vec![],
        );
        assert!(compute(&ds, &bad, b"s").is_err());
        let bad2 = cfg(
            vec![SubsetTarget {
                table: "orders".into(),
                filter: TargetFilter::Where("ghost_col > 1".into()),
            }],
            vec![],
            vec![],
        );
        assert!(compute(&ds, &bad2, b"s").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
