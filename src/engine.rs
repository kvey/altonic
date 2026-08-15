//! Generation pipeline (SPEC §5): validate → subset → key maps → analysis →
//! transform → report.
//!
//! Referential integrity (SPEC §4, `preserve_referential_integrity`):
//! any column referenced by a foreign key and configured with a generator
//! gets a *key map* — a per-column injective mapping source value → masked
//! value, built over ALL source rows in a dedicated pass (so the mapping is
//! independent of subsetting and row order). The referenced column and every
//! FK column pointing at it read from the same map, which keeps joins intact
//! by construction. Configuring a generator directly on such an FK column is
//! rejected.

use crate::config::{Config, OutOfSubset, TableMode};
use crate::connector::{DestinationConnector, SourceConnector};
use crate::crypto;
use crate::generators::{build, BuiltGenerator, CellCtx, ColumnStats};
use crate::json::Json;
use crate::rng::Rng;
use crate::schema::{Schema, Table};
use crate::subset::{self, SubsetResult, TableRole};
use crate::value::Value;
use crate::{rerr, verr, Result};
use std::collections::{HashMap, HashSet};

pub struct GenerateOptions {
    pub use_subset: bool,
    pub dry_run: bool,
}

impl Default for GenerateOptions {
    fn default() -> Self {
        GenerateOptions { use_subset: true, dry_run: false }
    }
}

#[derive(Debug)]
pub struct TableReport {
    pub table: String,
    pub mode: TableMode,
    pub role: Option<TableRole>,
    pub rows_read: usize,
    pub rows_written: usize,
    pub cells_masked: usize,
}

#[derive(Debug, Default)]
pub struct Report {
    pub tables: Vec<TableReport>,
    pub elapsed_ms: u128,
}

impl Report {
    pub fn to_json(&self) -> Json {
        Json::Object(vec![
            ("elapsed_ms".to_string(), Json::Int(self.elapsed_ms as i64)),
            (
                "tables".to_string(),
                Json::Array(
                    self.tables
                        .iter()
                        .map(|t| {
                            let mut m = vec![
                                ("table".to_string(), Json::Str(t.table.clone())),
                                ("mode".to_string(), Json::Str(t.mode.name().to_string())),
                                ("rows_read".to_string(), Json::Int(t.rows_read as i64)),
                                ("rows_written".to_string(), Json::Int(t.rows_written as i64)),
                                ("cells_masked".to_string(), Json::Int(t.cells_masked as i64)),
                            ];
                            if let Some(role) = t.role {
                                m.push((
                                    "subset_role".to_string(),
                                    Json::Str(format!("{role:?}").to_lowercase()),
                                ));
                            }
                            Json::Object(m)
                        })
                        .collect(),
                ),
            ),
        ])
    }
}

/// One column's transform plan within a table.
enum ColumnPlan {
    /// No transformation.
    Passthrough,
    /// Apply this generator.
    Generate(BuiltGenerator),
    /// Read the parent key map built for (table, column).
    KeyMap { map_key: (String, String) },
}

struct TablePlan {
    mode: TableMode,
    columns: Vec<ColumnPlan>,
    /// Column ordinals with a unique constraint AND a generating plan
    /// (uniqueness enforced by retry).
    unique_cols: Vec<usize>,
    /// Link label -> column ordinals sharing the entity (config order).
    link_groups: HashMap<String, Vec<usize>>,
}

/// FK column pairing across the schema: child (table, column) → directly
/// referenced parent (table, column). Shared by planning and `check`.
pub(crate) fn fk_parent_map(schema: &Schema) -> HashMap<(String, String), (String, String)> {
    let mut fk_parent = HashMap::new();
    for t in &schema.tables {
        for fk in &t.foreign_keys {
            for (cc, pc) in fk.columns.iter().zip(&fk.references_columns) {
                fk_parent.insert(
                    (t.name.clone(), cc.clone()),
                    (fk.references_table.clone(), pc.clone()),
                );
            }
        }
    }
    fk_parent
}

/// Follow FK chains to the ultimate referenced column (a column can be both
/// referenced and itself an FK child, e.g. shared primary keys). Cycles
/// error out.
pub(crate) fn resolve_fk_root(
    fk_parent: &HashMap<(String, String), (String, String)>,
    mut key: (String, String),
) -> Result<(String, String)> {
    let mut seen = HashSet::new();
    while let Some(parent) = fk_parent.get(&key) {
        if !seen.insert(key.clone()) {
            return Err(verr(format!(
                "foreign key cycle through {}.{} cannot be masked; break the cycle or disable preserve_referential_integrity",
                key.0, key.1
            )));
        }
        key = parent.clone();
    }
    Ok(key)
}

/// Referenced root columns that carry an explicit generator → get key maps.
pub(crate) fn mapped_root_set(
    schema: &Schema,
    config: &Config,
    fk_parent: &HashMap<(String, String), (String, String)>,
) -> Result<HashSet<(String, String)>> {
    let _ = schema;
    let mut mapped_roots = HashSet::new();
    if config.preserve_referential_integrity {
        for parent in fk_parent.values() {
            let root = resolve_fk_root(fk_parent, parent.clone())?;
            if config.column_config(&root.0, &root.1).is_some() {
                mapped_roots.insert(root);
            }
        }
    }
    Ok(mapped_roots)
}

/// Validate the whole configuration against the schema and build per-table
/// plans. Fails fast (SPEC §4.1).
fn plan(schema: &Schema, config: &Config) -> Result<HashMap<String, TablePlan>> {
    // Unknown tables/columns in config?
    for tname in &config.table_order {
        let t = schema
            .table(tname)
            .ok_or_else(|| verr(format!("config: unknown table {tname:?}")))?;
        let tc = &config.tables[tname];
        for cname in &tc.column_order {
            if t.column(cname).is_none() {
                return Err(verr(format!("config: unknown column {tname:?}.{cname:?}")));
            }
        }
    }

    let fk_parent = fk_parent_map(schema);
    let resolve_root = |key: (String, String)| resolve_fk_root(&fk_parent, key);
    let mapped_roots = mapped_root_set(schema, config, &fk_parent)?;

    let mut plans = HashMap::new();
    for t in &schema.tables {
        let tc_mode = config.mode_for(&t.name);
        let mut columns = Vec::with_capacity(t.columns.len());
        let mut unique_cols = Vec::new();
        let mut link_groups: HashMap<String, Vec<usize>> = HashMap::new();
        for (ci, col) in t.columns.iter().enumerate() {
            let key = (t.name.clone(), col.name.clone());
            let explicit = config.column_config(&t.name, &col.name);
            let is_fk_child = fk_parent.contains_key(&key);

            let plan = if config.preserve_referential_integrity && is_fk_child {
                let root = resolve_root(fk_parent[&key].clone())?;
                if mapped_roots.contains(&root) {
                    if explicit.is_some() {
                        return Err(verr(format!(
                            "config: {}.{} is a foreign key to {}.{} which is masked; remove the generator on the FK column — it inherits the key mapping automatically",
                            t.name, col.name, root.0, root.1
                        )));
                    }
                    ColumnPlan::KeyMap { map_key: root }
                } else if explicit.is_some() {
                    return Err(verr(format!(
                        "config: {}.{} is a foreign key to unmasked column {}.{}; masking it would break referential integrity. Configure the generator on {}.{} instead (or set preserve_referential_integrity to false)",
                        t.name, col.name, root.0, root.1, root.0, root.1
                    )));
                } else {
                    ColumnPlan::Passthrough
                }
            } else if mapped_roots.contains(&key) {
                // Referenced root with a generator: reads its own key map.
                ColumnPlan::KeyMap { map_key: key.clone() }
            } else {
                match explicit {
                    None => ColumnPlan::Passthrough,
                    Some(cc) => {
                        let built = build(&cc.generator, &cc.options, t, col)?;
                        if col.unique
                            && cc.generator != "passthrough"
                            && !built.gen.supports_uniqueness()
                        {
                            return Err(verr(format!(
                                "config: column {}.{} is unique but generator {:?} cannot guarantee unique output",
                                t.name, col.name, cc.generator
                            )));
                        }
                        ColumnPlan::Generate(built)
                    }
                }
            };
            if let ColumnPlan::Generate(b) = &plan {
                if col.unique {
                    unique_cols.push(ci);
                }
                if let Some(label) = &b.link {
                    link_groups.entry(label.clone()).or_default().push(ci);
                }
            }
            columns.push(plan);
        }
        plans.insert(
            t.name.clone(),
            TablePlan { mode: tc_mode, columns, unique_cols, link_groups },
        );
    }
    Ok(plans)
}

/// Seed digest for one cell (SPEC §5.1).
#[allow(clippy::too_many_arguments)]
fn cell_digest(
    config: &Config,
    b: &BuiltGenerator,
    table: &Table,
    col_ordinal: usize,
    row: &[Value],
    row_ordinal: usize,
    link_members: Option<&[usize]>,
    attempt: u32,
) -> [u8; 32] {
    let secret = config.secret.as_bytes();
    let attempt_bytes = attempt.to_be_bytes();
    if let Some(members) = link_members {
        // Linked entity: one seed for the whole group.
        let label = b.link.as_deref().unwrap_or("");
        if b.consistent {
            let joined = members
                .iter()
                .map(|&i| row[i].canonical())
                .collect::<Vec<_>>()
                .join("\u{1}");
            crypto::hmac_parts(secret, &[b"link", label.as_bytes(), joined.as_bytes(), &attempt_bytes])
        } else {
            crypto::hmac_parts(
                secret,
                &[
                    b"link-row",
                    label.as_bytes(),
                    &config.seed.to_be_bytes(),
                    table.name.as_bytes(),
                    &(row_ordinal as u64).to_be_bytes(),
                    &attempt_bytes,
                ],
            )
        }
    } else if b.consistent {
        let input = &row[col_ordinal];
        crypto::hmac_parts(
            secret,
            &[b.family.as_bytes(), input.canonical().as_bytes(), &attempt_bytes],
        )
    } else {
        crypto::hmac_parts(
            secret,
            &[
                b"row",
                &config.seed.to_be_bytes(),
                table.name.as_bytes(),
                table.columns[col_ordinal].name.as_bytes(),
                &(row_ordinal as u64).to_be_bytes(),
                &attempt_bytes,
            ],
        )
    }
}

/// Build key maps for every mapped root column: injective value → masked
/// value over all source rows, uniqueness enforced by deterministic retry in
/// source order.
fn build_key_maps(
    src: &dyn SourceConnector,
    config: &Config,
    plans: &HashMap<String, TablePlan>,
) -> Result<HashMap<(String, String), HashMap<String, Value>>> {
    let schema = src.schema();
    let mut maps: HashMap<(String, String), HashMap<String, Value>> = HashMap::new();

    // Collect the root columns needing maps.
    let mut roots: Vec<(String, String)> = Vec::new();
    for t in &schema.tables {
        let plan = &plans[&t.name];
        for (ci, cp) in plan.columns.iter().enumerate() {
            if let ColumnPlan::KeyMap { map_key } = cp {
                if map_key.0 == t.name && map_key.1 == t.columns[ci].name && !roots.contains(map_key) {
                    roots.push(map_key.clone());
                }
            }
        }
    }

    for root in roots {
        let t = schema.table(&root.0).unwrap();
        let ci = t.column_index(&root.1).unwrap();
        let col = &t.columns[ci];
        let cc = config
            .column_config(&root.0, &root.1)
            .ok_or_else(|| verr(format!("internal: no generator for mapped root {}.{}", root.0, root.1)))?;
        let built = build(&cc.generator, &cc.options, t, col)?;
        if built.gen.needs_stats() {
            return Err(verr(format!(
                "config: {}.{}: generator {:?} needs an analysis pass and cannot be used on a key column",
                root.0, root.1, cc.generator
            )));
        }
        let mut map: HashMap<String, Value> = HashMap::new();
        let mut used: HashSet<String> = HashSet::new();
        let mut rows = src.read_table(&t.name)?;
        let mut ordinal = 0usize;
        while let Some(row) = rows.next_row()? {
            let input = &row[ci];
            if input.is_null() {
                ordinal += 1;
                continue;
            }
            let ckey = input.canonical();
            if map.contains_key(&ckey) {
                ordinal += 1;
                continue;
            }
            let mut out = None;
            for attempt in 0..1000u32 {
                let digest = cell_digest(config, &built, t, ci, &row, ordinal, None, attempt);
                let mut rng = Rng::from_digest(&digest);
                let ctx = CellCtx {
                    table: t,
                    row: &row,
                    row_ordinal: ordinal,
                    attempt,
                    secret: config.secret.as_bytes(),
                };
                let candidate = built.gen.generate(input, &ctx, &mut rng)?;
                if candidate.is_null() {
                    return Err(rerr(format!(
                        "key generator for {}.{} produced NULL for non-null input {input}",
                        root.0, root.1
                    )));
                }
                if used.insert(candidate.canonical()) {
                    out = Some(candidate);
                    break;
                }
            }
            let out = out.ok_or_else(|| {
                rerr(format!(
                    "could not find a unique masked key for {}.{} after 1000 attempts (output space too small?)",
                    root.0, root.1
                ))
            })?;
            map.insert(ckey, out);
            ordinal += 1;
        }
        maps.insert(root, map);
    }
    Ok(maps)
}

/// Analysis pass: collect column stats for generators that need them,
/// respecting subset inclusion so output distributions match shipped data.
fn analysis_pass(
    src: &dyn SourceConnector,
    plans: &mut HashMap<String, TablePlan>,
    subset: Option<&SubsetResult>,
) -> Result<()> {
    let schema = src.schema();
    for t in &schema.tables {
        let needs: Vec<usize> = {
            let plan = &plans[&t.name];
            plan.columns
                .iter()
                .enumerate()
                .filter_map(|(i, cp)| match cp {
                    ColumnPlan::Generate(b) if b.gen.needs_stats() => Some(i),
                    _ => None,
                })
                .collect()
        };
        if needs.is_empty() {
            continue;
        }
        let mut stats: HashMap<usize, ColumnStats> = needs.iter().map(|&i| (i, ColumnStats::default())).collect();
        let mut freq_index: HashMap<usize, HashMap<String, usize>> =
            needs.iter().map(|&i| (i, HashMap::new())).collect();
        let mut rows = src.read_table(&t.name)?;
        let mut ordinal = 0usize;
        while let Some(row) = rows.next_row()? {
            let included = subset.map(|s| s.includes(&t.name, ordinal)).unwrap_or(true);
            if included {
                for &i in &needs {
                    let v = &row[i];
                    if v.is_null() {
                        continue;
                    }
                    let st = stats.get_mut(&i).unwrap();
                    let fi = freq_index.get_mut(&i).unwrap();
                    let ckey = v.canonical();
                    match fi.get(&ckey) {
                        Some(&pos) => st.freqs[pos].1 += 1,
                        None => {
                            fi.insert(ckey, st.freqs.len());
                            st.freqs.push((v.clone(), 1));
                        }
                    }
                    if let Some(f) = v.as_f64() {
                        st.numbers.push(f);
                    }
                }
            }
            ordinal += 1;
        }
        let plan = plans.get_mut(&t.name).unwrap();
        for (i, mut st) in stats {
            st.numbers.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            if let ColumnPlan::Generate(b) = &mut plan.columns[i] {
                b.gen.set_stats(st);
            }
        }
    }
    Ok(())
}

/// Run the full pipeline.
pub fn generate(
    src: &dyn SourceConnector,
    dest: &mut dyn DestinationConnector,
    config: &Config,
    options: &GenerateOptions,
) -> Result<Report> {
    let start = std::time::Instant::now();
    let schema = src.schema();
    let mut plans = plan(schema, config)?;

    let subset_result = if options.use_subset && config.subset_enabled() {
        let sc = config.subset.as_ref().unwrap();
        Some(subset::compute(src, sc, config.secret.as_bytes())?)
    } else {
        if let Some(sc) = &config.subset {
            if sc.enabled {
                // still validate so --no-subset doesn't hide config rot
                subset::validate(sc, schema)?;
            }
        }
        None
    };

    let key_maps = build_key_maps(src, config, &plans)?;
    analysis_pass(src, &mut plans, subset_result.as_ref())?;

    if !options.dry_run {
        dest.write_schema(schema)?;
    }

    let mut report = Report::default();
    for t in &schema.tables {
        let plan = &plans[&t.name];
        let role = subset_result.as_ref().map(|s| s.role(&t.name));
        let mut tr = TableReport {
            table: t.name.clone(),
            mode: plan.mode,
            role,
            rows_read: 0,
            rows_written: 0,
            cells_masked: 0,
        };

        // Effective emptiness from subsetting.
        let subset_truncates = match (&subset_result, role) {
            (Some(s), Some(TableRole::OutOfSubset)) => s.out_of_subset == OutOfSubset::Truncate,
            _ => false,
        };

        match plan.mode {
            TableMode::Truncate => {
                if !options.dry_run {
                    let mut sink = dest.open_table(t)?;
                    sink.finish()?;
                }
            }
            TableMode::PreserveDestination => {
                if !options.dry_run && !dest.has_table_data(&t.name) {
                    // Nothing to preserve: emit an empty (header-only) table
                    // so the destination dataset stays well-formed.
                    let mut sink = dest.open_table(t)?;
                    sink.finish()?;
                }
            }
            TableMode::DeIdentify => {
                if subset_truncates {
                    if !options.dry_run {
                        let mut sink = dest.open_table(t)?;
                        sink.finish()?;
                    }
                } else {
                    transform_table(
                        src,
                        dest,
                        config,
                        t,
                        plan,
                        &key_maps,
                        subset_result.as_ref(),
                        options.dry_run,
                        &mut tr,
                    )?;
                }
            }
        }
        report.tables.push(tr);
    }
    if !options.dry_run {
        // Database destinations add deferred FK constraints here, after all
        // tables (in any FK order) have their data.
        dest.finalize()?;
    }
    report.elapsed_ms = start.elapsed().as_millis();
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
fn transform_table(
    src: &dyn SourceConnector,
    dest: &mut dyn DestinationConnector,
    config: &Config,
    t: &Table,
    plan: &TablePlan,
    key_maps: &HashMap<(String, String), HashMap<String, Value>>,
    subset: Option<&SubsetResult>,
    dry_run: bool,
    tr: &mut TableReport,
) -> Result<()> {
    let mut sink = if dry_run { None } else { Some(dest.open_table(t)?) };
    let mut rows = src.read_table(&t.name)?;
    let mut ordinal = 0usize;
    // Per-unique-column seen sets for retry-based uniqueness.
    let mut seen: HashMap<usize, HashSet<String>> = plan
        .unique_cols
        .iter()
        .map(|&i| (i, HashSet::new()))
        .collect();
    // Reverse lookup: column ordinal -> its link group members.
    let mut link_of: HashMap<usize, Vec<usize>> = HashMap::new();
    for members in plan.link_groups.values() {
        for &i in members {
            link_of.insert(i, members.clone());
        }
    }
    // Include only-subset rows when applicable. Process mode still filters to
    // included rows for tables IN the subset; out-of-subset Process tables
    // have empty include sets but should copy everything, handled here:
    let copy_all = match subset {
        None => true,
        Some(s) => match s.role(&t.name) {
            TableRole::OutOfSubset => s.out_of_subset == OutOfSubset::Process,
            _ => false,
        },
    };

    while let Some(row) = rows.next_row()? {
        tr.rows_read += 1;
        let this_ordinal = ordinal;
        ordinal += 1;
        let included = copy_all || subset.map(|s| s.includes(&t.name, this_ordinal)).unwrap_or(true);
        if !included {
            continue;
        }
        let mut out = row.clone();
        for (ci, cp) in plan.columns.iter().enumerate() {
            match cp {
                ColumnPlan::Passthrough => {}
                ColumnPlan::KeyMap { map_key } => {
                    let input = &row[ci];
                    if input.is_null() {
                        continue;
                    }
                    let map = key_maps.get(map_key).ok_or_else(|| {
                        rerr(format!("internal: missing key map for {}.{}", map_key.0, map_key.1))
                    })?;
                    let masked = map.get(&input.canonical()).ok_or_else(|| {
                        rerr(format!(
                            "{}.{}: value {input} has no counterpart in {}.{} (dangling foreign key in source data)",
                            t.name, t.columns[ci].name, map_key.0, map_key.1
                        ))
                    })?;
                    out[ci] = masked.clone();
                    tr.cells_masked += 1;
                }
                ColumnPlan::Generate(b) => {
                    let input = &row[ci];
                    let link_members = link_of.get(&ci).map(|v| v.as_slice());
                    let unique = seen.contains_key(&ci);
                    let mut produced = None;
                    let max_attempts = if unique { 1000 } else { 1 };
                    for attempt in 0..max_attempts {
                        let digest = cell_digest(
                            config,
                            b,
                            t,
                            ci,
                            &row,
                            this_ordinal,
                            link_members,
                            attempt,
                        );
                        let mut rng = Rng::from_digest(&digest);
                        let ctx = CellCtx {
                            table: t,
                            row: &row,
                            row_ordinal: this_ordinal,
                            attempt,
                            secret: config.secret.as_bytes(),
                        };
                        let candidate = b.gen.generate(input, &ctx, &mut rng)?;
                        if unique
                            && !candidate.is_null()
                            && !seen.get_mut(&ci).unwrap().insert(candidate.canonical())
                        {
                            continue; // collision: retry with next attempt
                        }
                        produced = Some(candidate);
                        break;
                    }
                    let v = produced.ok_or_else(|| {
                        rerr(format!(
                            "{}.{}: could not generate a unique value after 1000 attempts",
                            t.name, t.columns[ci].name
                        ))
                    })?;
                    if v.is_null() && !t.columns[ci].nullable {
                        return Err(rerr(format!(
                            "{}.{}: generator produced NULL for a non-nullable column",
                            t.name, t.columns[ci].name
                        )));
                    }
                    if !(v == row[ci]) {
                        tr.cells_masked += 1;
                    }
                    out[ci] = v;
                }
            }
        }
        if let Some(sink) = sink.as_mut() {
            sink.write_row(&out)?;
        }
        tr.rows_written += 1;
    }
    if let Some(mut sink) = sink {
        sink.finish()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::{read_all_rows, DirDataset, DirDestination};
    use std::path::PathBuf;

    fn demo(dirname: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("altonic-engine-{dirname}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("schema.json"),
            r#"{
              "tables": [
                {"name": "customers", "columns": [
                  {"name": "id", "type": "integer"},
                  {"name": "email", "type": "text", "unique": true},
                  {"name": "first_name", "type": "text"},
                  {"name": "last_name", "type": "text"},
                  {"name": "plan", "type": "text"}],
                 "primary_key": ["id"]},
                {"name": "orders", "columns": [
                  {"name": "id", "type": "integer"},
                  {"name": "customer_id", "type": "integer", "nullable": false},
                  {"name": "total", "type": "float"}],
                 "primary_key": ["id"],
                 "foreign_keys": [
                   {"columns": ["customer_id"], "references_table": "customers", "references_columns": ["id"]}]},
                {"name": "audit_log", "columns": [
                  {"name": "id", "type": "integer"},
                  {"name": "note", "type": "text"}], "primary_key": ["id"]}
              ]
            }"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("customers.csv"),
            "id,email,first_name,last_name,plan\n\
             1,alice@corp.com,Alice,Adams,pro\n\
             2,bob@corp.com,Bob,Brown,free\n\
             3,carol@corp.com,Carol,Clark,free\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("orders.csv"),
            "id,customer_id,total\n10,1,99.5\n11,2,15.0\n12,1,42.0\n",
        )
        .unwrap();
        std::fs::write(dir.join("audit_log.csv"), "id,note\n1,secret note\n").unwrap();
        dir
    }

    fn demo_config() -> Config {
        Config::parse(
            r#"{
              "name": "t",
              "secret": "s3cret",
              "tables": {
                "customers": {
                  "columns": {
                    "id": {"generator": "integer_key", "options": {"consistency": "value"}},
                    "email": {"generator": "email", "options": {"consistency": "value"}},
                    "first_name": {"generator": "name", "options": {"part": "first", "link": "person"}},
                    "last_name": {"generator": "name", "options": {"part": "last", "link": "person"}}
                  }
                },
                "audit_log": {"mode": "truncate"}
              }
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn end_to_end_masking_with_fk_integrity() {
        let dir = demo("e2e");
        let src = DirDataset::open(&dir).unwrap();
        let out_dir = dir.join("out");
        let mut dest = DirDestination::create(&out_dir).unwrap();
        let config = demo_config();
        let report =
            generate(&src, &mut dest, &config, &GenerateOptions::default()).unwrap();

        let out = DirDataset::open(&out_dir).unwrap();
        let customers = read_all_rows(&out, "customers").unwrap();
        let orders = read_all_rows(&out, "orders").unwrap();
        let audit = read_all_rows(&out, "audit_log").unwrap();

        // Truncate mode: empty.
        assert!(audit.is_empty());

        // Emails masked, unique, none of the originals leak.
        let emails: Vec<&str> = customers.iter().map(|r| r[1].as_text().unwrap()).collect();
        assert_eq!(emails.len(), 3);
        assert!(emails.iter().all(|e| !e.contains("corp.com")));
        let unique: HashSet<&&str> = emails.iter().collect();
        assert_eq!(unique.len(), 3);

        // FK integrity: every orders.customer_id exists among masked customer ids.
        let ids: HashSet<String> = customers.iter().map(|r| r[0].canonical()).collect();
        for o in &orders {
            assert!(ids.contains(&o[1].canonical()), "dangling {:?}", o[1]);
        }
        // Order rows 10 and 12 both belonged to customer 1: still the same
        // (masked) customer.
        assert_eq!(orders[0][1], orders[2][1]);
        assert_ne!(orders[0][1], Value::Int(1)); // actually masked

        // Linked person: full-name coherence between first and last columns
        // is checked in generator tests; here check both changed.
        assert_ne!(customers[0][2], Value::Text("Alice".into()));

        // Passthrough column untouched.
        assert_eq!(customers[0][4], Value::Text("pro".into()));

        let creport = report.tables.iter().find(|t| t.table == "customers").unwrap();
        assert_eq!(creport.rows_written, 3);
        assert!(creport.cells_masked >= 9); // id, email, first, last on 3 rows (some names may collide by chance)

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deterministic_reruns() {
        let dir = demo("rerun");
        let src = DirDataset::open(&dir).unwrap();
        let config = demo_config();
        let out1 = dir.join("out1");
        let out2 = dir.join("out2");
        generate(&src, &mut DirDestination::create(&out1).unwrap(), &config, &GenerateOptions::default()).unwrap();
        generate(&src, &mut DirDestination::create(&out2).unwrap(), &config, &GenerateOptions::default()).unwrap();
        for f in ["customers.csv", "orders.csv", "schema.json"] {
            let a = std::fs::read(out1.join(f)).unwrap();
            let b = std::fs::read(out2.join(f)).unwrap();
            assert_eq!(a, b, "{f} differs between identical runs");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fk_column_with_generator_rejected() {
        let dir = demo("fkreject");
        let src = DirDataset::open(&dir).unwrap();
        let config = Config::parse(
            r#"{
              "secret": "s",
              "tables": {
                "customers": {"columns": {"id": {"generator": "integer_key"}}},
                "orders": {"columns": {"customer_id": {"generator": "integer_key"}}}
              }
            }"#,
        )
        .unwrap();
        let err = generate(
            &src,
            &mut DirDestination::create(dir.join("out")).unwrap(),
            &config,
            &GenerateOptions::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("inherits the key mapping"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn masking_fk_child_of_unmasked_parent_rejected() {
        let dir = demo("fkreject2");
        let src = DirDataset::open(&dir).unwrap();
        let config = Config::parse(
            r#"{
              "secret": "s",
              "tables": {
                "orders": {"columns": {"customer_id": {"generator": "integer_key"}}}
              }
            }"#,
        )
        .unwrap();
        let err = generate(
            &src,
            &mut DirDestination::create(dir.join("out")).unwrap(),
            &config,
            &GenerateOptions::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("break referential integrity"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn categorical_preserves_plan_values() {
        let dir = demo("categorical");
        let src = DirDataset::open(&dir).unwrap();
        let config = Config::parse(
            r#"{
              "secret": "s",
              "tables": {
                "customers": {"columns": {"plan": {"generator": "categorical"}}}
              }
            }"#,
        )
        .unwrap();
        let out_dir = dir.join("out");
        generate(&src, &mut DirDestination::create(&out_dir).unwrap(), &config, &GenerateOptions::default()).unwrap();
        let out = DirDataset::open(&out_dir).unwrap();
        let customers = read_all_rows(&out, "customers").unwrap();
        for r in &customers {
            let plan = r[4].as_text().unwrap();
            assert!(plan == "pro" || plan == "free", "unexpected category {plan}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subset_generation() {
        let dir = demo("subsetgen");
        let src = DirDataset::open(&dir).unwrap();
        let config = Config::parse(
            r#"{
              "secret": "s3cret",
              "tables": {
                "customers": {
                  "columns": {
                    "id": {"generator": "integer_key", "options": {"consistency": "value"}},
                    "email": {"generator": "email", "options": {"consistency": "value"}}
                  }
                }
              },
              "subset": {
                "enabled": true,
                "targets": [{"table": "orders", "where": "total > 50"}]
              }
            }"#,
        )
        .unwrap();
        let out_dir = dir.join("out");
        generate(&src, &mut DirDestination::create(&out_dir).unwrap(), &config, &GenerateOptions::default()).unwrap();
        let out = DirDataset::open(&out_dir).unwrap();
        let orders = read_all_rows(&out, "orders").unwrap();
        let customers = read_all_rows(&out, "customers").unwrap();
        let audit = read_all_rows(&out, "audit_log").unwrap();
        // Only order 10 (99.5) passes; its customer follows; audit truncated.
        assert_eq!(orders.len(), 1);
        assert_eq!(customers.len(), 1);
        assert!(audit.is_empty());
        // FK still intact after masking + subsetting.
        assert_eq!(orders[0][1], customers[0][0]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
