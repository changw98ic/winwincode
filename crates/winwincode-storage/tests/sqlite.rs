use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;
use winwincode_storage::{NewOutboxEvent, ProductStateStorage, SqliteStorage, StateCommit};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-storage-{name}-{}-{suffix}",
        std::process::id()
    ))
}

#[test]
fn startup_rejects_a_database_from_a_newer_schema_version() {
    let root = temporary_directory("newer-schema");
    fs::create_dir_all(&root).expect("test directory should exist");
    let database_path = root.join("control-plane.sqlite3");
    let connection = Connection::open(&database_path).expect("test database should open");
    connection
        .pragma_update(None, "user_version", 2)
        .expect("test schema version should be written");
    connection.close().expect("test database should close");

    let Err(error) = SqliteStorage::open(&root) else {
        panic!("a newer schema must not be silently downgraded");
    };

    assert!(error.to_string().contains("unsupported schema version 2"));
    fs::remove_dir_all(root).expect("rejected database should have no open connection");
}

#[test]
fn sqlite_adapter_uses_no_dynamic_savepoint_or_sql_identifier_path() {
    let source = include_str!("../src/lib.rs");
    for forbidden in [
        ".savepoint(",
        "savepoint_with_name",
        "new_unchecked",
        "unchecked_transaction",
        "execute(&format!",
        "execute_batch(&format!",
        "prepare(&format!",
        "query_row(&format!",
    ] {
        assert!(
            !source.contains(forbidden),
            "SQLite adapter introduced forbidden dynamic SQL path: {forbidden}"
        );
    }
    assert!(source.contains("params!["));
}

#[test]
fn sql_looking_business_values_remain_bound_values() {
    let root = temporary_directory("bound-values");
    let mut storage = SqliteStorage::open(&root).expect("SQLite storage should open");
    let stream_id = "stream'); DROP TABLE product_state; --";
    let event_id = "event'); DROP TABLE outbox; --";
    let commit = StateCommit::new(
        "request'); DROP TABLE command_receipts; --",
        stream_id,
        0,
        b"state".to_vec(),
        vec![NewOutboxEvent::new(
            event_id,
            "topic'); PRAGMA writable_schema=ON; --",
            b"event".to_vec(),
        )],
    );

    storage
        .commit(&commit)
        .expect("SQL-looking values should commit as data");

    let state = storage
        .load_state(stream_id)
        .expect("the state table should remain readable")
        .expect("the SQL-looking stream id should be stored literally");
    assert_eq!((state.revision, state.payload), (1, b"state".to_vec()));
    let events = storage
        .pending_events()
        .expect("the outbox table should remain readable");
    assert_eq!(events[0].event_id, event_id);
    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}
