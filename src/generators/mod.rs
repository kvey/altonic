//! Generator engine: trait, factory/registry, and seeding discipline
//! (SPEC §5.1, §6).
//!
//! The engine (not the generator) decides the RNG seed for every cell:
//!   consistent:      HMAC(secret, family ‖ canonical(input))
//!   linked:          HMAC(secret, "link" ‖ label ‖ linked source values)
//!   non-consistent:  HMAC(secret, run_seed ‖ table ‖ column ‖ row_ordinal)
//! and on uniqueness collisions re-derives with an attempt counter.
//! Generators are pure functions of (input, ctx, rng).

mod composite;
mod data;
mod datetime;
mod info;
mod keys;
mod numeric;
mod strings;

// Shared with the sensitivity scanner.
pub use data::{FIRST_NAMES, LAST_NAMES};
pub use info::luhn_valid;

use crate::json::Json;
use crate::rng::Rng;
use crate::schema::{Column, Table};

/// Compile a user-supplied pattern, reporting the failure against the
/// generator that asked for it. The `regex` crate's message already carries
/// the offending position.
fn compile(generator: &str, pattern: &str) -> Result<regex::Regex> {
    regex::Regex::new(pattern)
        .map_err(|e| verr(format!("generator {generator:?}: invalid regex {pattern:?}: {e}")))
}
use crate::value::{ColumnType, Row, Value};
use crate::{verr, Result};

/// Per-cell context handed to generators.
pub struct CellCtx<'a> {
    pub table: &'a Table,
    pub row: &'a Row,
    pub row_ordinal: usize,
    /// Uniqueness retry counter (0 on first attempt).
    pub attempt: u32,
    pub secret: &'a [u8],
}

impl CellCtx<'_> {
    pub fn col_value(&self, name: &str) -> Option<Value> {
        self.table.column_index(name).map(|i| self.row[i].clone())
    }
}

/// Column statistics collected in the analysis pass for distribution
/// generators (`categorical`, `continuous`).
#[derive(Debug, Clone, Default)]
pub struct ColumnStats {
    /// Distinct non-null values with occurrence counts (insertion order).
    pub freqs: Vec<(Value, u64)>,
    /// All non-null numeric values, sorted ascending.
    pub numbers: Vec<f64>,
}

pub trait Generator {
    fn generate(&self, input: &Value, ctx: &CellCtx, rng: &mut Rng) -> Result<Value>;

    /// Whether the engine must collect column stats before generation.
    fn needs_stats(&self) -> bool {
        false
    }

    fn set_stats(&mut self, _stats: ColumnStats) {}

    /// Whether retry-on-collision can make outputs unique (large output space).
    fn supports_uniqueness(&self) -> bool {
        false
    }

    /// Output is independent of the input value (inherently private).
    fn is_data_free(&self) -> bool {
        false
    }
}

/// A configured generator: the built impl plus the cross-cutting options the
/// engine interprets (consistency, linking).
pub struct BuiltGenerator {
    pub gen: Box<dyn Generator>,
    /// Seeding family — the generator name, so equal inputs map equally
    /// across any column using the same generator kind.
    pub family: String,
    /// Value-consistent seeding (SPEC §5.1).
    pub consistent: bool,
    /// Link label: columns in one table sharing a label share an entity seed.
    pub link: Option<String>,
}

/// Option-reading helpers over the raw JSON options object.
pub struct Opts<'a> {
    gen_name: &'a str,
    json: &'a Json,
}

impl<'a> Opts<'a> {
    pub fn new(gen_name: &'a str, json: &'a Json) -> Opts<'a> {
        Opts { gen_name, json }
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        self.json.get(key)
    }

    pub fn str(&self, key: &str) -> Result<Option<&str>> {
        match self.json.get(key) {
            None => Ok(None),
            Some(Json::Str(s)) => Ok(Some(s)),
            Some(_) => Err(self.bad(key, "a string")),
        }
    }

    pub fn str_or(&self, key: &str, default: &'a str) -> Result<&str> {
        Ok(self.str(key)?.unwrap_or(default))
    }

    pub fn f64(&self, key: &str) -> Result<Option<f64>> {
        match self.json.get(key) {
            None => Ok(None),
            Some(j) => j.as_f64().map(Some).ok_or_else(|| self.bad(key, "a number")),
        }
    }

    pub fn i64(&self, key: &str) -> Result<Option<i64>> {
        match self.json.get(key) {
            None => Ok(None),
            Some(j) => j.as_i64().map(Some).ok_or_else(|| self.bad(key, "an integer")),
        }
    }

    pub fn bool_or(&self, key: &str, default: bool) -> Result<bool> {
        match self.json.get(key) {
            None => Ok(default),
            Some(Json::Bool(b)) => Ok(*b),
            Some(_) => Err(self.bad(key, "a boolean")),
        }
    }

    fn bad(&self, key: &str, want: &str) -> crate::Error {
        verr(format!("generator {:?}: option {key:?} must be {want}", self.gen_name))
    }

    /// Reject unknown option keys so typos fail fast. `known` excludes the
    /// cross-cutting keys handled by the factory.
    pub fn check_known(&self, known: &[&str]) -> Result<()> {
        const CROSS: &[&str] = &["consistency", "link"];
        if let Some(obj) = self.json.as_object() {
            for (k, _) in obj {
                if !known.contains(&k.as_str()) && !CROSS.contains(&k.as_str()) {
                    return Err(verr(format!(
                        "generator {:?}: unknown option {k:?} (known: {})",
                        self.gen_name,
                        known.join(", ")
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Names of all registered generators (for error messages and docs).
pub const GENERATOR_NAMES: &[&str] = &[
    // keys
    "integer_key",
    "uuid_key",
    "alphanumeric_key",
    "numeric_string_key",
    "sequential_integer",
    // info types
    "name",
    "email",
    "phone",
    "address",
    "business_name",
    "ssn",
    "ip_address",
    "mac_address",
    "url",
    "hostname",
    "file_name",
    "geo",
    "credit_card",
    // datetime
    "timestamp_shift",
    "date_truncation",
    "random_timestamp",
    // numeric
    "random_integer",
    "random_double",
    "noise",
    "continuous",
    "algebraic",
    // strings & misc
    "categorical",
    "custom_categorical",
    "character_scramble",
    "character_substitution",
    "constant",
    "find_and_replace",
    "null",
    "random_boolean",
    "random_hash",
    "random_uuid",
    "passthrough",
    // composite
    "regex_mask",
    "json_mask",
    "conditional",
];

/// Build a generator from its config. Validates options and column-type
/// compatibility. `column` is the column it is being attached to.
pub fn build(name: &str, options: &Json, table: &Table, column: &Column) -> Result<BuiltGenerator> {
    let opts = Opts::new(name, options);
    let consistent = match opts.str("consistency")? {
        None | Some("none") => false,
        Some("value") => true,
        Some(other) => {
            return Err(verr(format!(
                "generator {name:?}: consistency must be \"none\" or \"value\", got {other:?}"
            )))
        }
    };
    let link = opts.str("link")?.map(str::to_string);

    let gen: Box<dyn Generator> = match name {
        "integer_key" => keys::integer_key(&opts, column)?,
        "uuid_key" => keys::uuid_key(&opts, column)?,
        "alphanumeric_key" => keys::alphanumeric_key(&opts, column)?,
        "numeric_string_key" => keys::numeric_string_key(&opts, column)?,
        "sequential_integer" => keys::sequential_integer(&opts, column)?,
        "name" => info::name(&opts, column)?,
        "email" => info::email(&opts, column)?,
        "phone" => info::phone(&opts, column)?,
        "address" => info::address(&opts, column)?,
        "business_name" => info::business_name(&opts, column)?,
        "ssn" => info::ssn(&opts, column)?,
        "ip_address" => info::ip_address(&opts, column)?,
        "mac_address" => info::mac_address(&opts, column)?,
        "url" => info::url(&opts, column)?,
        "hostname" => info::hostname(&opts, column)?,
        "file_name" => info::file_name(&opts, column)?,
        "geo" => info::geo(&opts, column)?,
        "credit_card" => info::credit_card(&opts, column)?,
        "timestamp_shift" => datetime::timestamp_shift(&opts, column)?,
        "date_truncation" => datetime::date_truncation(&opts, column)?,
        "random_timestamp" => datetime::random_timestamp(&opts, column)?,
        "random_integer" => numeric::random_integer(&opts, column)?,
        "random_double" => numeric::random_double(&opts, column)?,
        "noise" => numeric::noise(&opts, column)?,
        "continuous" => numeric::continuous(&opts, column)?,
        "algebraic" => numeric::algebraic(&opts, table, column)?,
        "categorical" => strings::categorical(&opts, column)?,
        "custom_categorical" => strings::custom_categorical(&opts, column)?,
        "character_scramble" => strings::character_scramble(&opts, column)?,
        "character_substitution" => strings::character_substitution(&opts, column)?,
        "constant" => strings::constant(&opts, column)?,
        "find_and_replace" => strings::find_and_replace(&opts, column)?,
        "null" => strings::null_gen(&opts, column)?,
        "random_boolean" => strings::random_boolean(&opts, column)?,
        "random_hash" => strings::random_hash(&opts, column)?,
        "random_uuid" => strings::random_uuid(&opts, column)?,
        "passthrough" => strings::passthrough(&opts)?,
        "regex_mask" => composite::regex_mask(&opts, table, column)?,
        "json_mask" => composite::json_mask(&opts, table, column)?,
        "conditional" => composite::conditional(&opts, table, column)?,
        _ => {
            return Err(verr(format!(
                "unknown generator {name:?} (available: {})",
                GENERATOR_NAMES.join(", ")
            )))
        }
    };

    Ok(BuiltGenerator { gen, family: name.to_string(), consistent, link })
}

/// Shared type check helper: require the column to be one of the given types.
pub(crate) fn require_type(
    gen: &str,
    column: &Column,
    ok: impl Fn(ColumnType) -> bool,
    expected: &str,
) -> Result<()> {
    if ok(column.ty) {
        Ok(())
    } else {
        Err(verr(format!(
            "generator {gen:?} is not compatible with column {:?} of type {} (needs {expected})",
            column.name,
            column.ty.name()
        )))
    }
}

/// Build a sub-generator for composite generators. Sub-generators run against
/// synthetic Text inputs, so type checks use a relaxed text column.
pub(crate) fn build_sub(spec: &Json, table: &Table, parent_col: &Column) -> Result<BuiltGenerator> {
    let name = spec
        .get("generator")
        .and_then(|g| g.as_str())
        .ok_or_else(|| verr("composite sub-generator missing \"generator\""))?;
    let options = spec.get("options").cloned().unwrap_or(Json::Object(Vec::new()));
    let relaxed = Column {
        name: parent_col.name.clone(),
        ty: ColumnType::Text,
        nullable: true,
        unique: false,
        db_type: None,
    };
    build(name, &options, table, &relaxed)
}

#[cfg(test)]
pub mod test_util {
    use super::*;
    use crate::schema::Schema;

    pub fn test_table() -> Table {
        let j = crate::json::Json::parse(
            r#"{"tables": [{"name": "t", "columns": [
                {"name": "c", "type": "text"},
                {"name": "n", "type": "integer"},
                {"name": "f", "type": "float"},
                {"name": "b", "type": "boolean"},
                {"name": "d", "type": "timestamp"},
                {"name": "u", "type": "uuid"},
                {"name": "j", "type": "json"}
            ]}]}"#,
        )
        .unwrap();
        Schema::from_json(&j).unwrap().tables[0].clone()
    }

    pub fn col(table: &Table, name: &str) -> Column {
        table.column(name).unwrap().clone()
    }

    /// Run a generator the way the engine would, with a fixed seed.
    pub fn run_gen(
        b: &BuiltGenerator,
        input: Value,
        table: &Table,
        row: Row,
    ) -> Value {
        let ctx = CellCtx {
            table,
            row: &row,
            row_ordinal: 0,
            attempt: 0,
            secret: b"test-secret",
        };
        let digest = if b.consistent {
            crate::crypto::hmac_parts(ctx.secret, &[b.family.as_bytes(), input.canonical().as_bytes()])
        } else {
            crate::crypto::hmac_parts(ctx.secret, &[b"run", b"t", b"c", &0u64.to_be_bytes()])
        };
        let mut rng = Rng::from_digest(&digest);
        b.gen.generate(&input, &ctx, &mut rng).unwrap()
    }

    pub fn quick(name: &str, options_json: &str, input: &str) -> Value {
        let table = test_table();
        let c = col(&table, "c");
        let opts = crate::json::Json::parse(options_json).unwrap();
        let b = build(name, &opts, &table, &c).unwrap();
        run_gen(&b, Value::Text(input.to_string()), &table, vec![Value::Null; 7])
    }
}
