# Altonic — Specification

A pure-Rust database data cleaning (de-identification) and replication tool, built from
first principles as an open re-implementation of the core ideas behind **Tonic Structural**
(tonic.ai): take a production dataset, detect sensitive data, replace it with realistic
synthetic values, optionally shrink it to a coherent subset, and replicate the result to a
destination — all while preserving schema, relationships, and statistical shape.

This document is both the research summary of what Tonic Structural does and the normative
specification for what Altonic implements.

---

## 1. Background research: what Tonic Structural does

Tonic Structural is a test-data-management platform. Its pipeline, distilled:

1. **Connect** a *source* database and a *destination* database inside a *workspace*.
2. **Scan** the source schema and data for PII/PHI ("sensitivity scan"), producing
   per-column recommendations of which *generator* to apply.
3. **Configure** the workspace: per-table *table modes*, per-column *generators* (with
   options such as consistency, linking, differential privacy), and optionally a
   *subsetting* configuration.
4. **Generate**: read the source, transform every row according to the configuration, and
   write the destination. Referential integrity is preserved; schema is replicated.

### 1.1 Table modes

| Mode | Behavior |
|------|----------|
| **De-identify** (default) | Copy all rows; apply the configured generator to each column (default generator is *Passthrough*, i.e. copy). |
| **Truncate** | Recreate the table empty in the destination (schema and constraints preserved, no data). |
| **Preserve destination** | Leave existing destination data untouched; do not copy from source. |
| **Incremental** | Only process rows whose "updated" column exceeds the destination's max (Postgres/MySQL/SQL Server in Tonic). |

### 1.2 Generators

Tonic ships ~60 generators grouped roughly into: **key** generators (integer key,
alphanumeric key, UUID key…), **information-type** generators (name, email, address, phone,
SSN, IP/MAC, IBAN, business name…), **datetime** generators (timestamp shift, date
truncation, random timestamp, event timestamps), **numeric** generators (random
integer/double, continuous distribution-preserving, noise, algebraic, cross-table sum),
**string** generators (categorical, custom categorical, character scramble, constant,
find-and-replace…), and **composite** generators that descend into structured values
(JSON mask, regex mask, XML/HTML/CSV mask, conditional).

Cross-cutting **generator characteristics**:

- **Consistency** — the same source value always maps to the same output value, across
  columns, tables, and runs. This is the mechanism that keeps joins working when keys are
  masked, and keeps "alice@example.com" mapping to the same fake email everywhere.
  Consistency can be *self-consistent* (seeded by the input value) and is keyed by a
  workspace secret ("statistics seed") so that outputs are stable per-workspace but not
  globally predictable.
- **Linking** — several columns share one underlying synthetic entity so their values are
  jointly coherent (first + last name of one person; street/city/state/zip of one address).
- **Uniqueness** — some generators can guarantee unique outputs for unique-constrained
  columns.
- **Data-free** — output is independent of the input value (inherently private).
- **Differential privacy** — for distribution-fitting generators (categorical,
  continuous), noise the fitted distribution so no single row is attributable.
- **Format-preserving** — output has the same shape/alphabet/length as the input.

### 1.3 Subsetting

Tonic's subsetter produces a small database that is *referentially closed*:

- **Target tables** define the rows you want: a percentage of rows or a SQL `WHERE` filter.
- **Downstream/required records**: rows referenced via foreign keys by anything already in
  the subset are pulled in transitively (a child row must have its parent).
- **Upstream/optional records**: rows that *reference* subset rows (e.g. the orders of an
  included customer) can optionally be added, with their own filters; adding them triggers
  another downstream pass so the result stays closed.
- **Lookup tables** are copied in full.
- **Out-of-subset tables** are truncated by default, or processed by their table mode.
- Circular FK chains and self-references must terminate.

### 1.4 Sensitivity scan

Tonic scans column names and sampled values to flag PII (names, emails, addresses, SSNs,
phone numbers, dates of birth…) and recommends an appropriate generator per column, which
users can bulk-apply.

---

## 2. Altonic: scope and principles

### 2.1 Goals

- Implement the full core loop — **scan → configure → subset → de-identify → replicate** —
  in Rust, from first principles.
- **Zero external dependencies.** Everything (JSON, CSV, regex, SHA-256/HMAC,
  deterministic PRNG) is implemented in this repository. `cargo build` needs only the
  Rust standard library. This keeps the trusted computing base auditable, which matters
  for a tool whose whole purpose is handling sensitive data.
- **Determinism as a feature.** Every run is a pure function of
  `(source data, config, secret)`. Same inputs → byte-identical output. This makes
  de-identification reviewable and CI-friendly.
- **Referential integrity always.** Masked keys keep joins intact; subsets are closed
  under FK traversal.

### 2.2 Non-goals (v1)

- A web UI, workspace multi-tenancy, or Tonic's proprietary ML-based synthesis.
- Differential-privacy accounting with formal ε budgets (we implement the mechanism
  hooks — noised categorical sampling — but do not claim formal guarantees).
- TLS for database connections (§7.2 lists the exact Postgres feature envelope).

---

## 3. Data model

### 3.1 Values

Every cell is a `Value`:

```
Value ::= Null | Bool(bool) | Int(i64) | Float(f64) | Text(String)
```

Column *types* (declared in the schema) refine interpretation: `integer`, `float`,
`boolean`, `text`, `uuid`, `date`, `timestamp`, `json`. CSV cells are parsed into typed
values on read and rendered canonically on write. An empty CSV cell is `Null`
(a literal empty string is `""`, written as `""`).

### 3.2 Schema

`schema.json` at the dataset root:

```json
{
  "tables": [
    {
      "name": "customers",
      "columns": [
        {"name": "id",    "type": "integer", "nullable": false},
        {"name": "email", "type": "text",    "unique": true}
      ],
      "primary_key": ["id"],
      "foreign_keys": [
        {"columns": ["state_code"], "references_table": "states", "references_columns": ["code"]}
      ]
    }
  ]
}
```

- Composite primary/foreign keys are supported.
- Self-referencing FKs (e.g. `employees.manager_id → employees.id`) are supported and
  must be nullable somewhere to terminate.
- Each table `t` is stored as `t.csv` with a header row matching the column order.

---

## 4. Workspace configuration

A single JSON file, the analog of a Tonic workspace:

```json
{
  "name": "demo",
  "secret": "workspace-secret-used-for-consistency",
  "seed": 42,
  "preserve_referential_integrity": true,
  "tables": {
    "customers": {
      "mode": "de_identify",
      "columns": {
        "email":      {"generator": "email", "options": {"consistency": "value"}},
        "first_name": {"generator": "name",  "options": {"part": "first", "link": "person"}},
        "last_name":  {"generator": "name",  "options": {"part": "last",  "link": "person"}}
      }
    },
    "audit_log": {"mode": "truncate"}
  },
  "subset": {
    "enabled": true,
    "targets": [
      {"table": "orders", "percent": 10.0},
      {"table": "customers", "where": "state_code == 'CA' or vip == true"}
    ],
    "lookup_tables": ["states", "currencies"],
    "upstream": [
      {"table": "orders", "where": "total > 100"}
    ],
    "out_of_subset": "truncate"
  }
}
```

Semantics:

- `secret` keys all consistent generation (HMAC key). Changing it re-keys every masked
  value. It should be treated like a credential; `seed` only affects *non*-consistent
  randomness, letting you re-roll data-free values without breaking consistency.
- `mode` ∈ `de_identify` (default) | `truncate` | `preserve_destination`.
- Unlisted tables default to `de_identify`; unlisted columns default to `passthrough`.
- `preserve_referential_integrity` (default `true`): when a PK column has a generator,
  every FK column that (transitively) references it automatically inherits the same
  generator with consistency forced on, so masked keys still join. Configuring a
  *different* generator on such an FK column is rejected at validation time.

### 4.1 Validation

Config validation fails fast, before any generation, on: unknown table/column/generator
names, options of the wrong type, generators incompatible with the column type, unique
columns whose generator cannot honor uniqueness, FK/PK generator conflicts (above), subset
targets on missing tables, and `where` expressions that do not parse or reference unknown
columns.

---

## 5. The generation pipeline

```
load schema ─▶ validate config ─▶ [subset: compute row inclusion] ─▶
[analysis pass: column stats for distribution generators] ─▶
per-table transform ─▶ write destination ─▶ report
```

1. **Subset** (if enabled) computes an *included set* of row indices per table (§8).
2. **Analysis pass**: generators that reproduce distributions (`categorical`,
   `continuous`) declare a need for column statistics; the engine streams the source once
   to collect them (value frequencies; numeric min/max/mean/stddev). Statistics are
   collected over the *subset* rows so the output distribution matches the shipped data.
3. **Transform pass**: for each table, in schema order, rows are read, filtered by the
   included set, transformed cell-by-cell, checked against unique constraints, and written.
4. **Report**: per-table rows in/out, cells masked, wall time; nonzero exit on any error.

### 5.1 Determinism and consistency — the seeding discipline

All randomness flows from one of two roots:

- **Consistent** values: `seed = HMAC-SHA256(secret, generator_family ‖ normalized_input)`.
  The seed depends *only* on the input value (and the workspace secret) — not on row
  order, table, or column — so the same value maps identically everywhere, and re-runs
  are stable. `generator_family` scopes the mapping (an email and a name derived from the
  same string need not correlate).
- **Non-consistent** values: `seed = HMAC-SHA256(secret, seed ‖ table ‖ column ‖ row_ordinal)`
  — still deterministic per run configuration, but decoupled from the input value.

The 256-bit HMAC output seeds a **SplitMix64→Xoshiro256\*\*** PRNG that drives the
generator. No generator may call a global/OS RNG.

**Consistency scopes** (per-column option): `none` (default), `value` (self-consistent,
as above). Because seeds are value-keyed, consistency automatically holds across columns,
tables, and runs, matching Tonic's "consistent across databases" behavior.

**Linking**: columns sharing a `link` label within a table row resolve one shared entity
seed — `HMAC(secret, link_label ‖ linked_source_values_in_column_order)` — then each
column projects the entity (first name, last name; street/city/state/zip) from that one
seed. Linked columns are therefore jointly coherent and jointly consistent.

**Uniqueness**: for unique columns, the engine tracks emitted values; on collision it
re-derives the seed with an appended attempt counter (up to 1000 attempts, then error).
Only generators flagged `supports_uniqueness` may be configured on unique columns.

---

## 6. Generator catalog

Column type compatibility, consistency support (C), uniqueness support (U), data-free (DF),
and format-preserving (FP) per generator. All generators treat `Null` as passthrough
unless noted.

### Key generators

| Generator | Types | C | U | Notes |
|---|---|---|---|---|
| `integer_key` | integer | ✓ | ✓ | Random 63-bit positive int keyed to input. |
| `uuid_key` | uuid/text | ✓ | ✓ | Deterministic v4-format UUID. |
| `alphanumeric_key` | text | ✓ | ✓ | FP: digits→digits, letters→letters, case & punctuation preserved. |
| `numeric_string_key` | text | ✓ | ✓ | FP: each digit remapped. |
| `sequential_integer` | integer | – | ✓ | `start`, `step`; ordinal within table. |

### Information types

| Generator | Types | C | U | DF | Notes |
|---|---|---|---|---|---|
| `name` | text | ✓ | – | ✓ | `part`: `first`\|`last`\|`full`; linkable. |
| `email` | text | ✓ | ✓ | ✓ | `preserve_domain` option; unique via `+n` tag. |
| `phone` | text | ✓ | – | ✓ | FP: preserves the input's format/punctuation, replaces digits (US NANP-plausible). |
| `address` | text | ✓ | – | ✓ | `part`: `street`\|`city`\|`state`\|`zip`\|`full`; linkable for coherent addresses. |
| `business_name` | text | ✓ | – | ✓ | Compositional ("Apex Data Systems"). |
| `ssn` | text | ✓ | ✓ | ✓ | Valid-format, non-issuable area codes avoided; preserves dash style. |
| `ip_address` | text | ✓ | – | ✓ | v4/v6 by input shape; stays in documentation ranges option. |
| `mac_address` | text | ✓ | – | ✓ | Preserves separator style. |
| `url` / `hostname` | text | ✓ | – | ✓ | Synthetic domain; path structure preserved for `url`. |
| `file_name` | text | ✓ | – | ✓ | Preserves extension. |
| `geo` | float/text | ✓ | – | ✓ | `part`: `lat`\|`lon`; plausible coordinates. |
| `credit_card` | text | ✓ | – | ✓ | Luhn-valid, brand-prefix preserving, FP on grouping. |

### Datetime

| Generator | Types | C | Notes |
|---|---|---|---|
| `timestamp_shift` | date/timestamp | ✓ | Shift by random amount in `±days` window; consistent per value or per `link`. |
| `date_truncation` | date/timestamp | – | Truncate to `year`\|`month`\|`day`\|`hour`. |
| `random_timestamp` | date/timestamp | ✓ | Uniform in `[min,max]`. |

### Numeric

| Generator | Types | C | Notes |
|---|---|---|---|
| `random_integer` / `random_double` | numeric | ✓ | Uniform in `[min,max]`. |
| `noise` | numeric | ✓ | Multiplicative/additive noise, `scale` option. |
| `continuous` | numeric | – | Analysis-pass generator: samples the source column's empirical distribution (rank-shuffled), preserving marginal shape. |
| `algebraic` | numeric | – | Expression over row columns, e.g. `quantity * unit_price`. |

### String / categorical

| Generator | Types | C | U | Notes |
|---|---|---|---|---|
| `categorical` | any | – | – | Analysis-pass: redraws from the column's own value-frequency distribution; optional `dp_epsilon`-style noising of counts. |
| `custom_categorical` | any | ✓ | – | Draw from user-supplied `values`. |
| `character_scramble` | text | ✓ | – | FP: letters→letters (case kept), digits→digits, everything else kept. |
| `character_substitution` | text | ✓ | – | FP: keyed per-character substitution cipher. |
| `constant` | any | – | – | Fixed `value`. |
| `find_and_replace` | text | – | – | Literal or `regex: true` find, with `$1` group refs. |
| `null` | any | – | – | Always NULL. |
| `random_boolean` | boolean | ✓ | – | |
| `random_hash` | text | ✓ | ✓ | Hex SHA-256 of input under secret (i.e. keyed pseudonymization). |
| `random_uuid` | uuid/text | ✓ | ✓ | |
| `passthrough` | any | – | – | Identity (the default). |

### Composite

| Generator | Notes |
|---|---|
| `regex_mask` | Match `pattern` (Altonic regex, §10.3) against the text; each capture group is re-generated by a configured sub-generator; non-captured text is preserved. |
| `json_mask` | Parse cell as JSON; apply sub-generators at JSON paths (`a.b[*].c`); everything else preserved. |
| `conditional` | Ordered list of `{when: <expr>, generator: …}` cases evaluated against the row, with optional default. |

Sub-generators inherit the parent cell's consistency seeding (seeded by the *matched
fragment*, so fragments are consistent independently).

---

## 7. Connectors

```rust
trait SourceConnector {
    fn schema(&self) -> &Schema;
    fn read_table(&self, table: &str) -> Result<RowIter>;   // streaming, re-openable
}
trait DestinationConnector {
    fn write_schema(&mut self, schema: &Schema) -> Result<()>;
    fn write_table(&mut self, table: &str, rows: RowIter) -> Result<()>;
    fn has_table_data(&self, table: &str) -> bool;           // for preserve_destination
}
```

Two connector pairs ship. The engine is written against the traits only; re-openable
streaming reads are required because subsetting and the analysis pass make multiple
passes. `DestinationConnector::finalize` runs once after all tables are written, for
deferred constraints.

### 7.1 Directory dataset

`DirDataset` / `DirDestination` over a directory of CSVs plus `schema.json` — the
portable interchange format, and the reference implementation for tests.

### 7.2 Postgres (pure Rust, wire protocol from first principles)

`PgSource` / `PgDestination` speak the Postgres frontend/backend protocol v3 directly
over TCP — no libpq, no driver crate:

- **Auth**: cleartext, `md5` (MD5 per RFC 1321, in-repo), and **SCRAM-SHA-256**
  (RFC 5802/7677: PBKDF2-HMAC-SHA256, base64, nonces from `/dev/urandom`; the server
  signature is verified). GSSAPI and TLS (hence `SCRAM-SHA-256-PLUS`) are out of scope;
  URLs demanding `sslmode=require` are rejected rather than silently downgraded.
- **Reads**: schema introspection via `information_schema` (tables, columns with
  `format_type` exact types, PK/UNIQUE constraints, FKs with correct composite-key
  pairing via `position_in_unique_constraint`), then `COPY … TO STDOUT` text-format
  streaming — one dedicated connection per open row stream. Sessions are pinned to UTC
  so `timestamptz` text is stable.
- **Type mapping**: `int2/4/8→integer`, `float4/8/numeric→float`, `bool`, `uuid`,
  `date`, `timestamp(tz)`, `json(b)`; everything else flows through as `text`
  (maskable by any text generator). Each column also records its exact source type
  (`db_type`, §3.2) so the replica keeps the original shape —
  `character varying(40)` stays `character varying(40)`, not `text`.
- **Writes**: per-table `DROP TABLE IF EXISTS … CASCADE` + `CREATE TABLE` using the
  original types, `NOT NULL`, `PRIMARY KEY`, and single-column `UNIQUE` constraints;
  data loads via `COPY … FROM STDIN`; **foreign keys are added in `finalize`** after
  all tables have data, so table load order never matters. DDL runs lazily per table,
  which is what makes `preserve_destination` leave existing tables untouched.
- **Envelope (v1)**: one schema per run (`?schema=`, default `public`); sequences /
  identity defaults, non-constraint indexes, views, triggers, extensions, and
  multi-column UNIQUE constraints are not replicated (`serial` columns arrive as their
  underlying int type). Exotic column types replicate by exact `db_type` and pass
  through as text.

URLs: `postgres://user:password@host:port/dbname[?schema=…&sslmode=disable]`; the
password may come from `PGPASSWORD`. Live integration tests are gated on
`ALTONIC_PG_TEST_SRC` / `ALTONIC_PG_TEST_DST`.

---

## 8. Subsetting algorithm

Terminology: an FK edge points **child → parent** (`orders.customer_id → customers.id`).
"Downstream" of a row are the parent rows it references (directly or transitively);
"upstream" are the child rows referencing it.

```
Inputs:  schema graph, targets, lookup set, upstream rules
Output:  Included(t) ⊆ rows(t) for every table t

1. Seed:      for each target (t, filter):
                Included(t) ← rows matching filter
              filter is `percent p` — deterministic hash sampling:
                  include row r iff HMAC(secret, "subset" ‖ t ‖ pk(r)) < p·2^64
                  (stable across runs; approximately p of rows) —
              or `where expr` — row-predicate evaluation (§10.2).
2. Lookups:   for each lookup table l: Included(l) ← all rows.
3. Close ↓:   worklist of (table, row) pairs; for each included row, for each FK
              on its table with non-null key values, locate the parent row by key
              index and include it if new; repeat to fixpoint. Cycles terminate
              because inclusion is monotone and finite; self-FK chains walk until
              a null or an already-included row.
4. Upstream:  for each upstream rule (t, filter): include rows of t that reference
              an included parent AND satisfy filter (filter defaults to true).
              FKs into lookup tables do not count as references here — lookups
              are included wholesale, and treating them as anchors would pull
              in every child row.
5. Close ↓ again (upstream rows may reference new parents). Steps 4–5 run once
              per upstream rule set, then a final downstream fixpoint.
6. Emit:      tables with no included rows and not lookup/target/related are
              out-of-subset → truncated (default) or processed per table mode.
```

Row identity is the PK when present, else the full-row hash. Key lookup indexes
(`(table, key columns) → row ordinal`) are built lazily per referenced key. The subsetter
reports per-table `selected / total` counts and which step contributed rows.

Correctness invariant (tested): after subsetting, for every FK and every included child
row with non-null FK values, the referenced parent row is included.

---

## 9. Sensitivity scan

Two signal sources, combined per column:

- **Name heuristics**: normalized column name (snake/camel/kebab folded) matched against a
  curated pattern table (`email`, `first_name`, `ssn`, `dob`, `phone`, `address`, `lat`,
  `password`, `api_key`, …).
- **Value heuristics**: sample up to N rows (default 1000); classify values with the
  regex engine and validators — email syntax, NANP phone shapes, SSN shape, Luhn-passing
  card numbers, IPv4/v6, UUID, ISO dates, US zips, names against the embedded dictionaries.

Each detector yields `(pii_type, confidence)`; name and value confidences combine (name
match dominates on conflict, value match alone is capped at "likely"). Output: a report
(text or JSON) of `column → {type, confidence, recommended generator + options}` and
`--apply <config>` writes/merges the recommendations into a workspace config, marking
applied entries so hand-edits are never overwritten silently.

---

## 10. First-principles infrastructure

Implemented in-repo, no external crates:

1. **JSON** (§10.1): recursive-descent parser + serializer, UTF-8, `\uXXXX` escapes and
   surrogate pairs; used for schema, config, reports, and `json_mask`.
2. **CSV**: RFC 4180 (quoted fields, embedded quotes/newlines/commas), streaming reader
   and writer.
3. **Regex** (§10.3): backtracking engine — literals, `.`, classes `[a-z0-9_]` (negated,
   ranges), escapes `\d \w \s \D \W \S`, anchors `^ $`, quantifiers `* + ? {m} {m,} {m,n}`
   (greedy + lazy `?` variants), alternation, capture groups, non-capturing `(?:…)`.
   Bounded input sizes (cells), so backtracking is acceptable; nesting depth capped.
4. **Crypto**: SHA-256 (FIPS 180-4) and HMAC-SHA256 (RFC 2104), verified against
   published test vectors in unit tests. Used for consistency seeds, hash sampling, and
   `random_hash`. (Purpose is *keyed pseudonymization*, not encryption at rest.)
5. **PRNG**: SplitMix64 seed-expander feeding Xoshiro256\*\*; uniform ints/floats without
   modulo bias (Lemire rejection), Gaussian via Box–Muller.
6. **Expressions** (§10.2): predicate/arithmetic language for `where`, `conditional`, and
   `algebraic`: literals, column refs, `== != < <= > >= + - * / %`, `and or not`, `in
   (…)`, `like` (SQL `%_` wildcards), `is null`. Pratt parser → typed eval over a row.

### 10.1 JSON grammar notes
Strict RFC 8259 with one extension: object member order is preserved (needed for stable
report output).

### 10.2 Expression language
`where` clauses evaluate per-row with `Value` semantics: comparisons across Int/Float
coerce numerically; `Null` propagates SQL-style (comparisons with null are false, `is
null` tests explicitly).

### 10.3 Regex dialect
No backreferences, no lookaround (keeps the engine simple and bounded). `regex_mask`
requires at least one capture group; group 0 rewriting is not supported.

---

## 11. CLI

```
altonic init <dir>                       scaffold demo dataset + config
altonic schema  --source <src>           print schema summary
altonic scan    --source <src> [--sample N] [--json] [--apply <config>]
altonic subset  --source <src> --config <file> [--json] [--verify]
altonic generate --source <src> --dest <dst> --config <file>
                 [--no-subset] [--dry-run] [--report <file>]
```

`<src>`/`<dst>` are dataset directories or `postgres://` URLs, in any combination
(pg→pg replication, pg→dir snapshots, dir→pg loads).

`generate` is the full pipeline; `--dry-run` runs everything except destination writes.
Exit codes: 0 success, 1 validation error, 2 runtime error.

## 12. Security considerations

- The workspace `secret` is the pseudonymization key: anyone holding it plus the tool can
  replay the mapping (dictionary attack on low-entropy inputs). Treat it as a secret;
  rotate to unlink datasets.
- Consistent generation is deterministic pseudonymization, **not** anonymization of
  high-signal quasi-identifier combinations; subsetting + masking reduces but does not
  formally bound re-identification risk. `categorical`'s count-noising is a mechanism,
  not a certified DP implementation.
- `passthrough` is the default generator by design (Tonic behaves the same); the scan
  exists to make un-reviewed passthrough visible. `scan --strict` (report nonzero exit if
  any likely-PII column is passthrough) is the CI guard.

## 13. Testing strategy

- Unit tests per primitive (SHA-256/HMAC official vectors; JSON/CSV round-trips; regex
  behavior table; PRNG determinism; expression eval).
- Generator tests: determinism, consistency (same value ⇒ same output across
  columns/tables), format preservation, uniqueness under forced collisions.
- Subsetter tests: closure invariant on cyclic and self-referencing schemas, percent
  sampling stability, upstream + re-closure.
- End-to-end: demo dataset (customers/orders/order_items/products/states + audit_log),
  full pipeline, asserting FK integrity of output, masking of all flagged columns, and
  byte-identical re-runs.

## 14. Future work

- Postgres: TLS, sequence/identity and index/view/trigger replication, multi-schema
  runs, incremental mode. MySQL connector behind the §7 traits.
- Cross-table sum, event-timestamp sequences, XML/HTML masks.
- Formal DP accounting; workspace inheritance; parallel per-table execution.
