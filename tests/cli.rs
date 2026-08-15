//! Integration tests driving the compiled `altonic` binary end to end
//! against the scaffolded demo workspace.

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_altonic")
}

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("altonic-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(bin()).args(args).output().expect("spawn altonic");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn path(p: &Path) -> &str {
    p.to_str().unwrap()
}

#[test]
fn full_workflow() {
    let dir = tmp("workflow");
    let source = dir.join("source");
    let config = dir.join("altonic.toml");
    let dest = dir.join("dest");

    // init
    let (code, stdout, stderr) = run(&["init", path(&dir)]);
    assert_eq!(code, 0, "init failed: {stderr}");
    assert!(stdout.contains("Scaffolded"));

    // schema
    let (code, stdout, _) = run(&["schema", "--source", path(&source)]);
    assert_eq!(code, 0);
    assert!(stdout.contains("customers"));
    assert!(stdout.contains("FK -> customers"));

    // scan: audit_log.actor_email is deliberately unconfigured → strict fails
    let (code, stdout, _) = run(&["scan", "--source", path(&source), "--config", path(&config)]);
    assert_eq!(code, 0);
    assert!(stdout.contains("actor_email"));
    let (code, _, stderr) = run(&[
        "scan", "--source", path(&source), "--config", path(&config), "--strict",
    ]);
    assert_eq!(code, 1, "strict scan should fail on unconfigured PII");
    assert!(stderr.contains("actor_email"));

    // scan --apply onto a fresh config, then strict passes with it
    let applied = dir.join("applied.toml");
    let (code, _, stderr) = run(&[
        "scan", "--source", path(&source), "--apply", path(&applied),
    ]);
    assert_eq!(code, 0, "{stderr}");
    let (code, _, _) = run(&[
        "scan", "--source", path(&source), "--config", path(&applied), "--strict",
    ]);
    assert_eq!(code, 0, "strict scan should pass once recommendations applied");

    // check: scaffolded config leaves audit_log.actor_email unprotected
    // (warning only — audit_log is truncated? no: truncate makes it n/a, so
    // the scaffold is actually clean; verify exit 0 and the coverage table).
    let (code, stdout, stderr) = run(&[
        "check", "--source", path(&source), "--config", path(&config),
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("inherited-key"), "{stdout}");

    // check catches drift: config referencing a column that does not exist.
    let stale = dir.join("stale.toml");
    std::fs::write(
        &stale,
        "secret = \"s\"\n[tables.customers.columns.ghost]\ngenerator = \"null\"\n",
    )
    .unwrap();
    let (code, stdout, stderr) = run(&[
        "check", "--source", path(&source), "--config", path(&stale), "--sample", "0",
    ]);
    assert_eq!(code, 1, "drift must fail: {stdout}{stderr}");
    assert!(stdout.contains("drift.column_missing"), "{stdout}");

    // check --strict flags unprotected PII (empty config, PII everywhere).
    let empty = dir.join("empty.toml");
    std::fs::write(&empty, "secret = \"s\"\n").unwrap();
    let (code, stdout, _) = run(&[
        "check", "--source", path(&source), "--config", path(&empty), "--strict",
    ]);
    assert_eq!(code, 1, "{stdout}");
    assert!(stdout.contains("pii.unprotected"), "{stdout}");

    // subset preview with verification
    let (code, stdout, stderr) = run(&[
        "subset", "--source", path(&source), "--config", path(&config), "--verify",
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("Closure verified"));

    // generate (subset disabled by default in the scaffold)
    let (code, stdout, stderr) = run(&[
        "generate", "--source", path(&source), "--dest", path(&dest), "--config", path(&config),
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("Generation finished"));

    // Original PII must not appear anywhere in the destination.
    let customers = std::fs::read_to_string(dest.join("customers.csv")).unwrap();
    for leaked in ["alice.anderson", "545-11-2211", "(415) 555-0132", "12 Bay St"] {
        assert!(!customers.contains(leaked), "PII leaked: {leaked}");
    }
    // audit_log truncated to header only.
    let audit = std::fs::read_to_string(dest.join("audit_log.csv")).unwrap();
    assert_eq!(audit.lines().count(), 1);

    // Destination is itself a valid dataset: schema loads, FKs intact.
    let (code, _, stderr) = run(&["schema", "--source", path(&dest)]);
    assert_eq!(code, 0, "destination not re-readable: {stderr}");

    // dry-run must not write
    let dest2 = dir.join("dest2");
    let (code, _, _) = run(&[
        "generate", "--source", path(&source), "--dest", path(&dest2),
        "--config", path(&config), "--dry-run",
    ]);
    assert_eq!(code, 0);
    assert!(!dest2.join("customers.csv").exists());

    // validation errors exit 1
    let (code, _, stderr) = run(&["generate", "--source", path(&source)]);
    assert_eq!(code, 1);
    assert!(stderr.contains("--dest"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn deterministic_across_processes() {
    let dir = tmp("determinism");
    run(&["init", path(&dir)]);
    let source = dir.join("source");
    let config = dir.join("altonic.toml");
    let (a, b) = (dir.join("a"), dir.join("b"));
    run(&["generate", "--source", path(&source), "--dest", path(&a), "--config", path(&config)]);
    run(&["generate", "--source", path(&source), "--dest", path(&b), "--config", path(&config)]);
    for f in ["customers.csv", "orders.csv", "order_items.csv", "schema.json"] {
        assert_eq!(
            std::fs::read(a.join(f)).unwrap(),
            std::fs::read(b.join(f)).unwrap(),
            "{f} not deterministic"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
