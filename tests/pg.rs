//! Live Postgres integration tests. Gated on environment variables so the
//! suite passes without a server:
//!
//!   ALTONIC_PG_TEST_SRC=postgres://user:pass@host:port/srcdb \
//!   ALTONIC_PG_TEST_DST=postgres://user:pass@host:port/dstdb \
//!   cargo test --test pg
//!
//! The SRC database is dropped-and-reseeded by the test; DST tables are
//! replaced. Point these at throwaway databases only.

use altonic::config::Config;
use altonic::connector::{read_all_rows, SourceConnector};
use altonic::engine::{self, GenerateOptions};
use altonic::pg::{run_sql, PgDestination, PgSource};
use altonic::value::Value;
use std::collections::HashSet;

fn urls() -> Option<(String, String)> {
    match (std::env::var("ALTONIC_PG_TEST_SRC"), std::env::var("ALTONIC_PG_TEST_DST")) {
        (Ok(s), Ok(d)) if !s.is_empty() && !d.is_empty() => Some((s, d)),
        _ => {
            eprintln!("skipping: set ALTONIC_PG_TEST_SRC / ALTONIC_PG_TEST_DST to run");
            None
        }
    }
}

const SEED: &str = r#"
DROP TABLE IF EXISTS pgtest_orders, pgtest_customers CASCADE;
CREATE TABLE pgtest_customers (
  id bigint PRIMARY KEY,
  email varchar(120) NOT NULL UNIQUE,
  full_name varchar(80) NOT NULL,
  note text,
  vip boolean NOT NULL,
  joined timestamptz NOT NULL
);
CREATE TABLE pgtest_orders (
  id bigint PRIMARY KEY,
  customer_id bigint NOT NULL REFERENCES pgtest_customers(id),
  total numeric(10,2) NOT NULL
);
INSERT INTO pgtest_customers VALUES
  (1, 'alice@corp.example', 'Alice Anderson', E'tab\there \\ and\nnewline', true,  '2023-01-01 08:00:00+00'),
  (2, 'bob@corp.example',   'Bob Baker',      NULL,                          false, '2023-02-02 09:30:00+00'),
  (3, 'carol@corp.example', 'Carol Chen',     '',                            false, '2023-03-03 10:45:00+00');
INSERT INTO pgtest_orders VALUES (10, 1, 99.50), (11, 1, 15.00), (12, 3, 42.25);
"#;

fn test_config() -> Config {
    Config::parse(
        r#"{
          "name": "pgtest",
          "secret": "pgtest-secret",
          "tables": {
            "pgtest_customers": {
              "columns": {
                "id": {"generator": "integer_key", "options": {"consistency": "value"}},
                "email": {"generator": "email", "options": {"consistency": "value"}},
                "full_name": {"generator": "name", "options": {}}
              }
            }
          }
        }"#,
    )
    .unwrap()
}

#[test]
fn pg_round_trip_masking_and_fk() {
    let Some((src_url, dst_url)) = urls() else { return };
    run_sql(&src_url, SEED).unwrap();

    let src = PgSource::connect(&src_url).unwrap();
    // Introspection sees our tables with exact types.
    let t = src.schema().table("pgtest_customers").expect("introspected table");
    assert_eq!(
        t.column("email").unwrap().db_type.as_deref(),
        Some("character varying(120)")
    );
    assert!(t.column("email").unwrap().unique);
    assert_eq!(t.primary_key, vec!["id"]);
    let orders_t = src.schema().table("pgtest_orders").unwrap();
    assert_eq!(orders_t.foreign_keys.len(), 1);

    // Round-trip fidelity of tricky values (tabs, backslashes, newlines,
    // NULL vs empty string) through COPY OUT.
    let rows = read_all_rows(&src, "pgtest_customers").unwrap();
    let note_col = t.column_index("note").unwrap();
    let by_id: std::collections::HashMap<String, &Vec<Value>> =
        rows.iter().map(|r| (r[0].to_string(), r)).collect();
    assert_eq!(
        by_id["1"][note_col],
        Value::Text("tab\there \\ and\nnewline".to_string())
    );
    assert_eq!(by_id["2"][note_col], Value::Null);
    assert_eq!(by_id["3"][note_col], Value::Text(String::new()));

    // Full masked replication into DST.
    let mut dest = PgDestination::connect(&dst_url).unwrap();
    let report = engine::generate(&src, &mut dest, &test_config(), &GenerateOptions::default()).unwrap();
    let cust_report = report.tables.iter().find(|r| r.table == "pgtest_customers").unwrap();
    assert_eq!(cust_report.rows_written, 3);

    // Read the destination back through our own connector.
    let dst = PgSource::connect(&dst_url).unwrap();
    let customers = read_all_rows(&dst, "pgtest_customers").unwrap();
    let orders = read_all_rows(&dst, "pgtest_orders").unwrap();
    assert_eq!(customers.len(), 3);
    assert_eq!(orders.len(), 3);

    // Shape preserved.
    let dt = dst.schema().table("pgtest_customers").unwrap();
    assert_eq!(dt.column("email").unwrap().db_type.as_deref(), Some("character varying(120)"));
    assert!(dst.schema().table("pgtest_orders").unwrap().foreign_keys.len() == 1);

    // PII gone, uniqueness held.
    let emails: HashSet<String> =
        customers.iter().map(|r| r[1].as_text().unwrap().to_string()).collect();
    assert_eq!(emails.len(), 3);
    assert!(emails.iter().all(|e| !e.contains("corp.example")));
    assert!(customers.iter().all(|r| r[2].as_text() != Some("Alice Anderson")));

    // Note column was passthrough: exact bytes survived COPY IN too.
    let dnote = dt.column_index("note").unwrap();
    let dby_id: std::collections::HashMap<String, &Vec<Value>> =
        customers.iter().map(|r| (r[0].to_string(), r)).collect();
    let masked_ids: Vec<&String> = dby_id.keys().collect();
    assert!(masked_ids.iter().all(|id| !["1", "2", "3"].contains(&id.as_str())), "ids masked");
    assert!(customers
        .iter()
        .any(|r| r[dnote] == Value::Text("tab\there \\ and\nnewline".to_string())));
    assert!(customers.iter().any(|r| r[dnote] == Value::Null));
    assert!(customers.iter().any(|r| r[dnote] == Value::Text(String::new())));

    // FK integrity: masked orders.customer_id values all exist among masked
    // customer ids.
    let ids: HashSet<String> = customers.iter().map(|r| r[0].canonical()).collect();
    assert!(orders.iter().all(|o| ids.contains(&o[1].canonical())));

    // Determinism across a second run.
    let mut dest2 = PgDestination::connect(&dst_url).unwrap();
    engine::generate(&src, &mut dest2, &test_config(), &GenerateOptions::default()).unwrap();
    let dst2 = PgSource::connect(&dst_url).unwrap();
    let customers2 = read_all_rows(&dst2, "pgtest_customers").unwrap();
    let mut a = customers.clone();
    let mut b = customers2.clone();
    let key = |r: &Vec<Value>| r[0].canonical();
    a.sort_by_key(key);
    b.sort_by_key(key);
    assert_eq!(a, b, "re-run should be byte-identical");

    run_sql(&src_url, "DROP TABLE pgtest_orders, pgtest_customers CASCADE").unwrap();
    run_sql(&dst_url, "DROP TABLE IF EXISTS pgtest_orders, pgtest_customers CASCADE").unwrap();
}
