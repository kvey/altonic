# Altonic

A pure-Rust **Postgres data cleaning (de-identification), subsetting, and replication**
tool — an open, from-first-principles re-implementation of the core ideas behind
[Tonic Structural](https://www.tonic.ai/products/tonic-structural): point it at a
production Postgres database and it produces a replica of the **same shape** — exact
column types, primary keys, unique constraints, foreign keys — with all identifying
information stripped and replaced by realistic synthetic values, optionally shrunk to a
small but referentially-closed subset.

```console
$ altonic generate \
    --source postgres://app:...@prod-replica:5432/app \
    --dest   postgres://app:...@dev-db:5432/app_masked \
    --config workspace.json
```

**Zero external dependencies.** The Postgres wire protocol (SCRAM-SHA-256/MD5 auth,
COPY streaming), JSON, CSV, regex, SHA-256/HMAC/PBKDF2, MD5, base64, and the
deterministic PRNG are all implemented in this repository; `cargo build` needs only the
Rust standard library. See [SPEC.md](SPEC.md) for the full specification.

## Highlights

- **Live Postgres → Postgres replication, no libpq**: the frontend/backend protocol v3
  is implemented from the spec — startup, cleartext/MD5/**SCRAM-SHA-256** auth (server
  signature verified), simple queries, and `COPY TO STDOUT` / `COPY FROM STDIN`
  text-format streaming. Schema introspection captures exact types
  (`character varying(40)` stays `character varying(40)`), PKs, uniques, and
  composite-correct FKs; the destination is rebuilt with the same DDL shape and FKs are
  attached after load so table order never matters. Sources and destinations mix
  freely: pg→pg, pg→directory snapshot, directory→pg.

- **38 generators**: key masking (format-preserving integer/UUID/alphanumeric keys),
  realistic PII types (names, emails, phones, addresses, SSNs, Luhn-valid credit cards,
  IPs in documentation ranges…), datetime shift/truncation, distribution-preserving
  `continuous` and `categorical`, and composite `regex_mask` / `json_mask` /
  `conditional` that mask inside structured values.
- **Consistency**: the same input maps to the same output across columns, tables, and
  runs, keyed by a workspace secret (HMAC-seeded, never a global RNG).
- **Referential integrity by construction**: masked primary keys propagate to every
  foreign key that references them through injective key maps; configs that would break
  a join are rejected at validation time.
- **Patented-elsewhere subsetting, from scratch**: target tables by percentage or
  `WHERE` expression, transitive downstream closure over FKs (cycles and self-references
  handled), optional upstream pulls, lookup tables copied whole — with a closure
  verifier built in.
- **Sensitivity scan**: column-name and sampled-value heuristics (regex + Luhn +
  dictionaries) recommend generators; `--apply` writes them into your config and
  `--strict` gates CI on unmasked PII.
- **Determinism as a feature**: a run is a pure function of
  `(source, config, secret)` — reruns are byte-identical, and subset sampling is stable
  hash-based, not RNG-order-based.

## Quick start (Postgres)

```console
$ cargo build --release
$ export SRC="postgres://user:pass@localhost:5432/prod_copy"
$ export DST="postgres://user:pass@localhost:5432/masked"
$ target/release/altonic schema   --source "$SRC"                  # introspect
$ target/release/altonic scan     --source "$SRC" --apply ws.json  # detect PII, write config
$ target/release/altonic subset   --source "$SRC" --config ws.json --verify
$ target/release/altonic generate --source "$SRC" --dest "$DST" --config ws.json
```

Verified against a live Postgres 16 with SCRAM auth: a 200-customer / 500-order /
1200-item source replicates in ~1s with every `varchar(n)`/`char(n)`/`numeric`/`jsonb`/
`timestamptz` column type preserved, PK/unique/FK constraints recreated, zero FK
orphans, zero PII strings surviving (including inside `jsonb` via `json_mask`), and
byte-identical reruns. With subsetting enabled the same run ships 20% of orders plus
exactly the customers/items/lookups that keep the replica closed under foreign keys.

Limitations (v1, documented in SPEC §7.2): plain-TCP connections only (no TLS), one
schema per run, and sequences/identity defaults, secondary indexes, views, and triggers
are not replicated.

## Quick start (file datasets, no database needed)

```console
$ target/release/altonic init demo
$ target/release/altonic generate --source demo/source --dest demo/dest --config demo/altonic.json
```

`init` scaffolds a six-table demo dataset (customers/orders/order_items/products/states/
audit_log) and a working workspace config. `generate` output on the demo looks like:

```text
table                    mode                      rows in   rows out cells masked
states                   de_identify                     5          5            0
products                 de_identify                     4          4            0
customers                de_identify                     6          6           60
orders                   de_identify                     8          8           24
order_items              de_identify                    11         11            0
audit_log                truncate                        0          0            0
```

A **dataset** is a directory: `schema.json` (tables, columns, PKs, FKs, uniques) plus
one CSV per table — an interchange format any database can dump to and load from. The
engine is written against `SourceConnector`/`DestinationConnector` traits, so live
database connectors are additive work (see SPEC §7).

## Configuration sketch

```json
{
  "name": "demo",
  "secret": "workspace-consistency-key",
  "tables": {
    "customers": {
      "columns": {
        "id":         {"generator": "integer_key", "options": {"consistency": "value"}},
        "email":      {"generator": "email", "options": {"consistency": "value"}},
        "first_name": {"generator": "name", "options": {"part": "first", "link": "person"}},
        "last_name":  {"generator": "name", "options": {"part": "last",  "link": "person"}}
      }
    },
    "audit_log": {"mode": "truncate"}
  },
  "subset": {
    "enabled": true,
    "targets": [{"table": "orders", "percent": 10.0}],
    "lookup_tables": ["states", "products"],
    "upstream": [{"table": "order_items"}]
  }
}
```

`orders.customer_id` needs no configuration: it references the masked `customers.id`
and inherits its key mapping automatically. Columns sharing a `link` label (here
`person`) draw from one synthetic entity, so first and last names stay coherent.

## Layout

| Path | What |
|---|---|
| `SPEC.md` | Research summary of Tonic Structural + the normative Altonic spec |
| `src/crypto.rs`, `src/rng.rs` | SHA-256/HMAC (RFC-vector-tested), SplitMix64→Xoshiro256\*\* |
| `src/json.rs`, `src/csv.rs`, `src/regex.rs`, `src/expr.rs` | RFC 8259 JSON, RFC 4180 CSV, a memoized backtracking regex VM, and the row-expression language |
| `src/schema.rs`, `src/value.rs`, `src/config.rs` | Data model and workspace config |
| `src/connector.rs` | Connector traits + directory dataset implementation |
| `src/pg/` | Postgres: wire protocol, SCRAM/MD5 auth, COPY codec, introspection, connectors |
| `src/generators/` | Generator trait, seeding discipline, and the full catalog |
| `src/subset.rs` | Target/lookup/upstream subsetter with closure verification |
| `src/scan.rs` | PII detection and recommendations |
| `src/engine.rs` | Pipeline: plan → subset → key maps → analysis → transform |
| `src/cli.rs` | `init` / `schema` / `scan` / `subset` / `generate` |

## Testing

```console
$ cargo test        # 127 tests: crypto vectors (SHA-256, HMAC, PBKDF2/SCRAM RFC 7677,
                    # MD5, base64), regex behavior, COPY codec round-trips, closure
                    # invariants, FK-integrity end-to-end, byte-identical rerun checks

# Live Postgres integration (points at throwaway databases it may drop/recreate):
$ ALTONIC_PG_TEST_SRC=postgres://u:p@localhost/src \
  ALTONIC_PG_TEST_DST=postgres://u:p@localhost/dst cargo test --test pg
```

## Security notes

Consistent generation is *keyed pseudonymization*, not anonymization: the workspace
`secret` can replay the mapping, so treat it like a credential and rotate it to unlink
datasets. The `categorical` generator's count-noising is a mechanism hook, not a
certified differential-privacy implementation. Unconfigured columns pass through by
design — `scan --strict` exists to make that visible in CI. See SPEC §12.
