// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;
use std::sync::{Arc, Barrier, mpsc};
use std::time::{Duration, Instant};

use winwincode_storage::{SqliteStorage, StorageError};

#[derive(Debug)]
enum OpenError {
    Storage(StorageError),
    Sql(rusqlite::Error),
    Injected,
}
impl std::fmt::Display for OpenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "{error}"),
            Self::Sql(error) => write!(formatter, "{error}"),
            Self::Injected => formatter.write_str("injected initializer failure"),
        }
    }
}
impl From<StorageError> for OpenError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}
impl From<rusqlite::Error> for OpenError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sql(error)
    }
}

struct Directory(PathBuf);
impl Directory {
    fn new(label: &str) -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "wwc-sidecar-{label}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn initialize(connection: &rusqlite::Connection) -> Result<(), OpenError> {
    connection.execute_batch(
        "PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS facts (id INTEGER PRIMARY KEY);",
    )?;
    Ok(())
}

#[test]
fn concurrent_fresh_sidecar_openings_share_the_real_initialization_boundary() {
    let root = Directory::new("concurrent");
    for round in 0..64 {
        let path = root.0.join(format!("mirror-{round}.sqlite3"));
        let barrier = Arc::new(Barrier::new(2));
        std::thread::scope(|scope| {
            let threads: Vec<_> = (0..2)
                .map(|_| {
                    let path = path.clone();
                    let barrier = barrier.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        SqliteStorage::open_sidecar(&path, initialize)
                    })
                })
                .collect();
            for thread in threads {
                let connection = thread.join().unwrap().unwrap();
                let journal: String = connection
                    .query_row("PRAGMA journal_mode", [], |row| row.get(0))
                    .unwrap();
                assert_eq!(journal, "wal");
            }
        });
    }
}

#[test]
fn another_database_does_not_wait_for_an_unrelated_initializer() {
    let root = Directory::new("independent");
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let first = root.0.join("first.sqlite3");
        let held = scope.spawn(move || {
            SqliteStorage::open_sidecar(first, |connection| {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(8)).unwrap();
                initialize(connection)
            })
        });
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let second = root.0.join("second.sqlite3");
        let (done_tx, done_rx) = mpsc::channel();
        scope.spawn(move || {
            let result = SqliteStorage::open_sidecar(second, initialize);
            done_tx.send(result.is_ok()).unwrap();
        });
        let independent = done_rx.recv_timeout(Duration::from_secs(2));
        release_tx.send(()).unwrap();
        held.join().unwrap().unwrap();
        assert!(independent.unwrap());
    });
}

#[test]
fn waiting_for_the_same_initializer_is_bounded_by_the_shared_deadline() {
    let root = Directory::new("deadline");
    let path = root.0.join("held.sqlite3");
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let held_path = path.clone();
        let held = scope.spawn(move || {
            SqliteStorage::open_sidecar(held_path, |connection| {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(8)).unwrap();
                initialize(connection)
            })
        });
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let began = Instant::now();
        let result = SqliteStorage::open_sidecar(&path, initialize);
        let elapsed = began.elapsed();
        release_tx.send(()).unwrap();
        let held_result = held.join().unwrap();
        assert!(matches!(result, Err(OpenError::Storage(_))));
        assert!(matches!(held_result, Err(OpenError::Storage(_))));
        assert!(elapsed >= Duration::from_millis(4_800));
        assert!(elapsed < Duration::from_secs(8));
    });
}

#[test]
fn initializer_typed_error_is_preserved_and_the_next_open_can_proceed() {
    let root = Directory::new("typed");
    let path = root.0.join("typed.sqlite3");
    let result = SqliteStorage::open_sidecar(&path, |_| Err(OpenError::Injected));
    assert!(matches!(result, Err(OpenError::Injected)));
    SqliteStorage::open_sidecar(&path, initialize).unwrap();
}

#[test]
fn runtime_busy_timeout_is_restored_after_initializer_configuration() {
    let root = Directory::new("timeout");
    let connection = SqliteStorage::open_sidecar(root.0.join("timeout.sqlite3"), |connection| {
        connection.busy_timeout(Duration::from_millis(1))?;
        initialize(connection)
    })
    .unwrap();
    let timeout: i64 = connection
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .unwrap();
    assert_eq!(timeout, 5_000);
}

#[test]
fn sqlite_writer_lock_during_schema_preparation_has_a_bounded_deadline() {
    let root = Directory::new("sql-lock");
    let path = root.0.join("lock.sqlite3");
    let held = rusqlite::Connection::open(&path).unwrap();
    held.execute_batch("CREATE TABLE facts(id INTEGER PRIMARY KEY); BEGIN IMMEDIATE;")
        .unwrap();
    let began = Instant::now();
    let result = SqliteStorage::open_sidecar(&path, |connection| {
        connection.execute_batch("INSERT INTO facts VALUES(1);")?;
        Ok::<_, OpenError>(())
    });
    let elapsed = began.elapsed();
    held.execute_batch("ROLLBACK;").unwrap();
    match result {
        Err(OpenError::Sql(rusqlite::Error::SqliteFailure(error, _))) => {
            assert_eq!(error.code, rusqlite::ErrorCode::DatabaseBusy);
        }
        other => panic!("expected bounded SQLite writer refusal, got {other:?}"),
    }
    assert!(elapsed >= Duration::from_millis(4_800));
    assert!(elapsed < Duration::from_secs(8));
}

#[test]
fn expired_caller_deadline_never_creates_the_database_or_enters_initializer() {
    let root = Directory::new("expired-caller");
    let path = root.0.join("not-created").join("mirror.sqlite3");
    let called = std::cell::Cell::new(false);
    let deadline = Instant::now()
        .checked_sub(Duration::from_millis(1))
        .unwrap();
    let result = SqliteStorage::open_sidecar_until(&path, deadline, |connection| {
        called.set(true);
        initialize(connection)
    });
    assert!(matches!(result, Err(OpenError::Storage(_))));
    assert!(!called.get());
    assert!(!path.exists());
    assert!(!path.parent().unwrap().exists());
}

#[test]
fn caller_deadline_bounds_sqlite_wait_and_is_not_reset_by_reopening() {
    let root = Directory::new("short-caller");
    let path = root.0.join("locked.sqlite3");
    let held = rusqlite::Connection::open(&path).unwrap();
    held.execute_batch("CREATE TABLE facts(id INTEGER PRIMARY KEY); BEGIN IMMEDIATE;")
        .unwrap();
    let called = std::cell::Cell::new(0);
    let began = Instant::now();
    let deadline = began + Duration::from_millis(400);
    let result = SqliteStorage::open_sidecar_until(&path, deadline, |connection| {
        called.set(called.get() + 1);
        connection.execute_batch("INSERT INTO facts VALUES(1);")?;
        Ok::<_, OpenError>(())
    });
    let elapsed = began.elapsed();
    match result {
        Err(OpenError::Sql(rusqlite::Error::SqliteFailure(error, _))) => {
            assert_eq!(error.code, rusqlite::ErrorCode::DatabaseBusy);
        }
        other => panic!("expected the caller-bounded writer refusal, got {other:?}"),
    }
    assert_eq!(called.get(), 1);
    assert!(elapsed >= Duration::from_millis(350));
    assert!(elapsed < Duration::from_secs(2));
    let retry_began = Instant::now();
    let retry = SqliteStorage::open_sidecar_until(&path, deadline, |connection| {
        called.set(called.get() + 1);
        initialize(connection)
    });
    let retry_elapsed = retry_began.elapsed();
    held.execute_batch("ROLLBACK;").unwrap();
    assert!(matches!(retry, Err(OpenError::Storage(_))));
    assert_eq!(called.get(), 1);
    assert!(retry_elapsed < Duration::from_secs(1));
    let rows: i64 = held
        .query_row("SELECT COUNT(*) FROM facts", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 0);
}
