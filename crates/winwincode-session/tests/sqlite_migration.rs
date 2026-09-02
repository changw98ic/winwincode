// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use rusqlite::{Connection, params};
use serde_json::{Value, json};
use winwincode_session::SqliteSessionIdentityMigration;
use winwincode_session::migration::{MigrationError, MigrationOutcome};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    directory: PathBuf,
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "winwincode-session-migration-{name}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("test database directory");
        let path = directory.join("session-migration.sqlite3");
        Self { directory, path }
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn fixture() -> Vec<u8> {
    fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/session-identity-migration/legacy-verifier.json"),
    )
    .expect("legacy migration fixture")
}

fn changed_same_source(input: &[u8]) -> Vec<u8> {
    let mut value: Value = serde_json::from_slice(input).expect("legacy JSON");
    value["spec"]["goal"] = json!("A changed legacy body for the same durable source.");
    serde_json::to_vec(&value).expect("changed legacy JSON")
}

fn snapshot(outcome: &MigrationOutcome) -> &[u8] {
    match outcome {
        MigrationOutcome::Applied {
            canonical_snapshot, ..
        }
        | MigrationOutcome::AlreadyConsumed {
            canonical_snapshot, ..
        } => canonical_snapshot,
    }
}

fn source_key(outcome: &MigrationOutcome) -> &str {
    match outcome {
        MigrationOutcome::Applied { source_key, .. }
        | MigrationOutcome::AlreadyConsumed { source_key, .. } => source_key,
    }
}

fn table_counts(path: &Path) -> [u64; 3] {
    let connection = Connection::open(path).expect("inspect migration database");
    [
        "session_identity_migration_sources",
        "session_identity_migration_snapshots",
        "session_identity_migration_consumed",
    ]
    .map(|table| {
        let count = connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("migration table count");
        u64::try_from(count).expect("non-negative migration table count")
    })
}

fn install_write_abort_triggers(path: &Path) {
    let connection = Connection::open(path).expect("write guard connection");
    for table in [
        "session_identity_migration_sources",
        "session_identity_migration_snapshots",
        "session_identity_migration_consumed",
    ] {
        for operation in ["INSERT", "UPDATE", "DELETE"] {
            connection
                .execute_batch(&format!(
                    "CREATE TRIGGER reject_{table}_{operation}
                     BEFORE {operation} ON {table}
                     BEGIN SELECT RAISE(ABORT, 'repeat attempted a write'); END;"
                ))
                .expect("install repeat write guard");
        }
    }
}

#[test]
fn first_migration_atomically_persists_all_three_records_and_secure_modes() {
    let database = TestDatabase::new("first");
    let mut adapter =
        SqliteSessionIdentityMigration::open(&database.path).expect("SQLite migration adapter");
    let outcome = adapter.migrate(&fixture()).expect("first migration");

    assert!(matches!(outcome, MigrationOutcome::Applied { .. }));
    assert_eq!(table_counts(&database.path), [1, 1, 1]);
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

    let connection = Connection::open(&database.path).expect("inspect stored snapshot");
    let stored: (String, Vec<u8>, i64) = connection
        .query_row(
            "SELECT sources.source_key, snapshots.canonical_snapshot, consumed.consumed_marker
               FROM session_identity_migration_sources AS sources
               JOIN session_identity_migration_snapshots AS snapshots USING (source_key)
               JOIN session_identity_migration_consumed AS consumed USING (source_key)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("complete durable migration row");
    assert_eq!(stored.0, source_key(&outcome));
    assert_eq!(stored.1, snapshot(&outcome));
    assert_eq!(stored.2, 1);
}

#[test]
fn restart_reads_original_snapshot_and_repeat_performs_no_write() {
    let database = TestDatabase::new("restart");
    let input = fixture();
    let first = {
        let mut adapter = SqliteSessionIdentityMigration::open(&database.path)
            .expect("first SQLite migration adapter");
        adapter.migrate(&input).expect("first migration")
    };
    install_write_abort_triggers(&database.path);

    let mut restarted = SqliteSessionIdentityMigration::open(&database.path)
        .expect("restarted SQLite migration adapter");
    let repeated = restarted
        .migrate(&changed_same_source(&input))
        .expect("already consumed result");

    assert!(matches!(repeated, MigrationOutcome::AlreadyConsumed { .. }));
    assert_eq!(source_key(&first), source_key(&repeated));
    assert_eq!(snapshot(&first), snapshot(&repeated));
    assert_eq!(table_counts(&database.path), [1, 1, 1]);
}

#[test]
fn failures_on_second_and_third_write_roll_back_every_record() {
    for (name, table) in [
        ("snapshot", "session_identity_migration_snapshots"),
        ("consumed", "session_identity_migration_consumed"),
    ] {
        let database = TestDatabase::new(name);
        drop(
            SqliteSessionIdentityMigration::open(&database.path)
                .expect("initialize migration database"),
        );
        let connection = Connection::open(&database.path).expect("failure injector");
        connection
            .execute_batch(&format!(
                "CREATE TRIGGER fail_{name}
                 BEFORE INSERT ON {table}
                 BEGIN SELECT RAISE(ABORT, 'simulated migration crash'); END;"
            ))
            .expect("install migration failure");
        drop(connection);

        let failed = {
            let mut adapter = SqliteSessionIdentityMigration::open(&database.path)
                .expect("migration adapter with injected failure");
            adapter.migrate(&fixture())
        };
        assert!(matches!(failed, Err(MigrationError::Transaction { .. })));
        assert_eq!(table_counts(&database.path), [0, 0, 0]);

        let connection = Connection::open(&database.path).expect("remove failure injector");
        connection
            .execute(&format!("DROP TRIGGER fail_{name}"), [])
            .expect("drop migration failure");
        drop(connection);
        let mut restarted = SqliteSessionIdentityMigration::open(&database.path)
            .expect("restarted migration adapter");
        assert!(matches!(
            restarted.migrate(&fixture()).expect("retry migration"),
            MigrationOutcome::Applied { .. }
        ));
        assert_eq!(table_counts(&database.path), [1, 1, 1]);
    }
}

#[test]
fn concurrent_consumers_commit_exactly_once() {
    let database = TestDatabase::new("concurrent");
    drop(
        SqliteSessionIdentityMigration::open(&database.path)
            .expect("initialize migration database"),
    );
    let barrier = Arc::new(Barrier::new(2));
    let input = Arc::new(fixture());
    let handles = (0..2)
        .map(|_| {
            let path = database.path.clone();
            let barrier = Arc::clone(&barrier);
            let input = Arc::clone(&input);
            thread::spawn(move || {
                let mut adapter = SqliteSessionIdentityMigration::open(path)
                    .expect("concurrent migration adapter");
                barrier.wait();
                adapter.migrate(&input).expect("concurrent migration")
            })
        })
        .collect::<Vec<_>>();
    let outcomes = handles
        .into_iter()
        .map(|handle| handle.join().expect("migration thread"))
        .collect::<Vec<_>>();

    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, MigrationOutcome::Applied { .. }))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, MigrationOutcome::AlreadyConsumed { .. }))
            .count(),
        1
    );
    assert_eq!(snapshot(&outcomes[0]), snapshot(&outcomes[1]));
    assert_eq!(table_counts(&database.path), [1, 1, 1]);
}

#[test]
fn foreign_keys_and_unique_source_markers_reject_partial_duplicate_state() {
    let database = TestDatabase::new("constraints");
    drop(
        SqliteSessionIdentityMigration::open(&database.path)
            .expect("initialize migration database"),
    );
    let connection = Connection::open(&database.path).expect("constraint connection");
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .expect("enable foreign keys");
    assert!(
        connection
            .execute(
                "INSERT INTO session_identity_migration_snapshots
                    (source_key, canonical_snapshot, canonical_sha256)
                 VALUES (?1, ?2, ?3)",
                params!["missing-source", b"{}", "0".repeat(64)],
            )
            .is_err()
    );
}

#[test]
fn restart_rejects_a_corrupted_durable_snapshot_digest() {
    let database = TestDatabase::new("corrupt-digest");
    let input = fixture();
    {
        let mut adapter =
            SqliteSessionIdentityMigration::open(&database.path).expect("migration database");
        adapter.migrate(&input).expect("first migration");
    }
    let connection = Connection::open(&database.path).expect("corruption fixture connection");
    connection
        .execute(
            "UPDATE session_identity_migration_snapshots
                SET canonical_sha256 = ?1",
            ["f".repeat(64)],
        )
        .expect("corrupt stored digest");
    drop(connection);

    let mut restarted =
        SqliteSessionIdentityMigration::open(&database.path).expect("restarted adapter");
    assert!(matches!(
        restarted.migrate(&input),
        Err(MigrationError::CorruptState { .. })
    ));
    assert_eq!(table_counts(&database.path), [1, 1, 1]);
}
