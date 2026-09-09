// SPDX-License-Identifier: Apache-2.0
use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use rusqlite::Connection;
use serde_json::Value;
use sha2::Digest as _;
use winwincode_delivery::{SqliteWorkRunMigration, WorkRunMigrationError, WorkRunMigrationOutcome};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    directory: PathBuf,
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "winwincode-session-workrun-{name}-{}-{sequence}",
            std::process::id()
        ));
        Self {
            path: directory.join("workrun-migration.sqlite3"),
            directory,
        }
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn fixture() -> Vec<u8> {
    fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/delivery-pre-workrun.json"),
    )
    .expect("frozen pre-WorkRun Delivery fixture")
}

fn snapshot(outcome: &WorkRunMigrationOutcome) -> &[u8] {
    match outcome {
        WorkRunMigrationOutcome::Applied {
            canonical_snapshot, ..
        }
        | WorkRunMigrationOutcome::AlreadyConsumed {
            canonical_snapshot, ..
        } => canonical_snapshot,
    }
}

fn source_key(outcome: &WorkRunMigrationOutcome) -> &str {
    match outcome {
        WorkRunMigrationOutcome::Applied { source_key, .. }
        | WorkRunMigrationOutcome::AlreadyConsumed { source_key, .. } => source_key,
    }
}

fn counts(path: &Path) -> (i64, i64) {
    let connection = Connection::open(path).expect("inspect database");
    (
        connection
            .query_row(
                "SELECT count(*) FROM workrun_migration_sources",
                [],
                |row| row.get(0),
            )
            .expect("source count"),
        connection
            .query_row(
                "SELECT count(*) FROM workrun_migration_snapshots",
                [],
                |row| row.get(0),
            )
            .expect("snapshot count"),
    )
}

#[test]
fn first_write_is_atomic_and_uses_secure_modes() {
    let database = TestDatabase::new("first");
    let mut migration = SqliteWorkRunMigration::open(&database.path).expect("open store");
    let outcome = migration.migrate(&fixture()).expect("migrate");
    assert!(matches!(outcome, WorkRunMigrationOutcome::Applied { .. }));
    assert_eq!(counts(&database.path), (1, 1));
    assert_eq!(
        fs::metadata(&database.directory)
            .expect("directory metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&database.path)
            .expect("database metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn existing_parent_permissions_are_not_changed() {
    let database = TestDatabase::new("existing-parent");
    fs::create_dir_all(&database.directory).expect("existing parent");
    fs::set_permissions(&database.directory, fs::Permissions::from_mode(0o755))
        .expect("set existing parent mode");
    let mut migration = SqliteWorkRunMigration::open(&database.path).expect("open store");
    migration.migrate(&fixture()).expect("migrate");
    assert_eq!(
        fs::metadata(&database.directory)
            .expect("directory metadata")
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
}

#[test]
fn restart_same_input_returns_original_receipt_without_new_rows() {
    let database = TestDatabase::new("restart-same");
    let input = fixture();
    let first = {
        let mut migration = SqliteWorkRunMigration::open(&database.path).expect("open store");
        migration.migrate(&input).expect("first migration")
    };
    let second = {
        let mut migration = SqliteWorkRunMigration::open(&database.path).expect("restart store");
        migration.migrate(&input).expect("replay migration")
    };
    assert!(matches!(
        second,
        WorkRunMigrationOutcome::AlreadyConsumed { .. }
    ));
    assert_eq!(source_key(&first), source_key(&second));
    assert_eq!(snapshot(&first), snapshot(&second));
    assert_eq!(counts(&database.path), (1, 1));
}

#[test]
fn same_source_changed_input_is_rejected_and_original_is_preserved() {
    let database = TestDatabase::new("conflict");
    let input = fixture();
    let mut migration = SqliteWorkRunMigration::open(&database.path).expect("open store");
    let first = migration.migrate(&input).expect("first migration");
    let mut changed: Value = serde_json::from_slice(&input).expect("fixture JSON");
    changed["spec"]["goal"] = Value::from("changed source bytes");
    // Keep the canonical id/revision so the source key collides.
    let changed = serde_json::to_vec(&changed).expect("changed fixture JSON");
    let mut restarted = SqliteWorkRunMigration::open(&database.path).expect("restart store");
    let result = restarted.migrate(&changed);
    assert!(matches!(
        result,
        Err(WorkRunMigrationError::CorruptState(_))
    ));
    assert_eq!(counts(&database.path), (1, 1));
    assert!(!snapshot(&first).is_empty());
}

#[test]
fn tampered_input_digest_and_output_digest_fail_closed() {
    let database = TestDatabase::new("tamper");
    let input = fixture();
    let mut migration = SqliteWorkRunMigration::open(&database.path).expect("open store");
    let first = migration.migrate(&input).expect("first migration");
    let key = source_key(&first).to_owned();
    drop(migration);
    let connection = Connection::open(&database.path).expect("tamper database");
    connection
        .execute(
            "UPDATE workrun_migration_snapshots SET canonical_sha256 = ?1 WHERE source_key = ?2",
            ("0".repeat(64), &key),
        )
        .expect("tamper output digest");
    drop(connection);
    let mut restarted = SqliteWorkRunMigration::open(&database.path).expect("restart store");
    assert!(matches!(
        restarted.migrate(&input),
        Err(WorkRunMigrationError::CorruptState(_))
    ));
}

#[test]
fn changed_snapshot_with_recomputed_hash_is_rejected() {
    let database = TestDatabase::new("tamper-bytes");
    let input = fixture();
    let mut migration = SqliteWorkRunMigration::open(&database.path).expect("open store");
    let first = migration.migrate(&input).expect("first migration");
    let key = source_key(&first).to_owned();
    drop(migration);
    let connection = Connection::open(&database.path).expect("tamper database");
    let mut changed = snapshot(&first).to_vec();
    changed.push(b' ');
    let digest = format!("{:x}", sha2::Sha256::digest(&changed));
    connection
        .execute(
            "UPDATE workrun_migration_snapshots
                SET canonical_snapshot = ?1, canonical_sha256 = ?2
              WHERE source_key = ?3",
            (&changed, &digest, &key),
        )
        .expect("tamper snapshot bytes");
    drop(connection);
    let mut restarted = SqliteWorkRunMigration::open(&database.path).expect("restart store");
    assert!(matches!(
        restarted.migrate(&input),
        Err(WorkRunMigrationError::CorruptState(_))
    ));
}

#[test]
fn partial_receipts_are_rejected() {
    let database = TestDatabase::new("partial");
    let mut migration = SqliteWorkRunMigration::open(&database.path).expect("open store");
    drop(migration);
    let connection = Connection::open(&database.path).expect("partial database");
    let (source_key, _) =
        winwincode_delivery::workrun_migration::convert_canonical_delivery(&fixture())
            .expect("source key");
    connection
        .execute(
            "INSERT INTO workrun_migration_sources(source_key,schema_version,input_sha256,consumed)
             VALUES (?1, 'winwincode.delivery-canonical-to-workrun.v1', ?2, 1)",
            (&source_key, "0".repeat(64)),
        )
        .expect("partial source");
    drop(connection);
    migration = SqliteWorkRunMigration::open(&database.path).expect("restart store");
    assert!(matches!(
        migration.migrate(&fixture()),
        Err(WorkRunMigrationError::CorruptState(_))
    ));
}

#[test]
fn failure_before_snapshot_commit_leaves_no_consumed_receipt_and_retries() {
    let database = TestDatabase::new("rollback");
    let mut migration = SqliteWorkRunMigration::open(&database.path).expect("open store");
    drop(migration);
    let connection = Connection::open(&database.path).expect("failure injector");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_workrun_snapshot
             BEFORE INSERT ON workrun_migration_snapshots
             BEGIN SELECT RAISE(ABORT, 'simulated migration failure'); END;",
        )
        .expect("install failure trigger");
    drop(connection);
    migration = SqliteWorkRunMigration::open(&database.path).expect("open with trigger");
    assert!(matches!(
        migration.migrate(&fixture()),
        Err(WorkRunMigrationError::Transaction(_))
    ));
    assert_eq!(counts(&database.path), (0, 0));
    let connection = Connection::open(&database.path).expect("remove failure injector");
    connection
        .execute("DROP TRIGGER fail_workrun_snapshot", [])
        .expect("drop failure trigger");
    drop(connection);
    let mut restarted = SqliteWorkRunMigration::open(&database.path).expect("retry store");
    assert!(matches!(
        restarted.migrate(&fixture()),
        Ok(WorkRunMigrationOutcome::Applied { .. })
    ));
    assert_eq!(counts(&database.path), (1, 1));
}
