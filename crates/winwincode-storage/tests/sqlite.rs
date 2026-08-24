use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;
use winwincode_domain::{RequestId, Sha256Digest};
use winwincode_storage::{
    NewOutboxEvent, ProductStateStorage, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey,
    SqliteStorage, StateCommit, StorageErrorKind,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-storage-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn receipt_identity(actor: &str, scope: &str, request_id: &str) -> ReceiptIdentity {
    ReceiptIdentity::new(
        ReceiptActorKey::from_encoded(actor.as_bytes().to_vec()).expect("actor key"),
        ReceiptScopeKey::from_encoded(scope.as_bytes().to_vec()).expect("scope key"),
        RequestId(request_id.to_owned()),
    )
    .expect("receipt identity")
}

fn state_commit(
    identity: (&str, &str, &str),
    digest: &str,
    stream_id: &str,
    expected_revision: u64,
    state: &[u8],
    event_id: &str,
) -> StateCommit {
    StateCommit::new(
        receipt_identity(identity.0, identity.1, identity.2),
        Sha256Digest(digest.to_owned()),
        stream_id,
        expected_revision,
        state.to_vec(),
        vec![NewOutboxEvent::new(
            event_id,
            "control-plane.state.changed",
            b"event".to_vec(),
        )],
    )
}

fn create_v1_fixture(database_path: &Path) {
    let connection = Connection::open(database_path).expect("v1 database should open");
    connection
        .execute_batch(
            "CREATE TABLE product_state (
                 stream_id TEXT PRIMARY KEY NOT NULL,
                 revision INTEGER NOT NULL CHECK (revision > 0),
                 payload BLOB NOT NULL
             );
             CREATE TABLE command_receipts (
                 request_id TEXT PRIMARY KEY NOT NULL,
                 command_signature BLOB NOT NULL,
                 stream_id TEXT NOT NULL,
                 revision INTEGER NOT NULL CHECK (revision > 0)
             );
             CREATE TABLE outbox (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 event_id TEXT UNIQUE NOT NULL,
                 request_id TEXT NOT NULL,
                 topic TEXT NOT NULL,
                 payload BLOB NOT NULL,
                 published INTEGER NOT NULL DEFAULT 0 CHECK (published IN (0, 1))
             );
             CREATE INDEX outbox_pending_sequence ON outbox (published, sequence);
             INSERT INTO product_state VALUES ('legacy-stream', 1, X'6C6567616379');
             INSERT INTO command_receipts VALUES (
                 'req_01J00000000000000000000002', X'010203', 'legacy-stream', 1
             );
             INSERT INTO outbox
                 (event_id, request_id, topic, payload, published)
                 VALUES (
                     'legacy-published-event', 'req_01J00000000000000000000002',
                     'control-plane.state.changed', X'7075626C6973686564', 1
                 );
             INSERT INTO outbox
                 (event_id, request_id, topic, payload, published)
                 VALUES (
                     'legacy-event', 'req_01J00000000000000000000002',
                     'control-plane.state.changed', X'6576656E74', 0
                 );
             PRAGMA user_version = 1;",
        )
        .expect("v1 fixture should be created");
    connection.close().expect("v1 fixture should close");
}

fn assert_v2_receipt_schema(database_path: &Path) {
    let connection = Connection::open(database_path).expect("migrated database should open");
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("schema version should be readable");
    assert_eq!(version, 2);
    let receipt_columns = {
        let mut statement = connection
            .prepare("PRAGMA table_info(command_receipts)")
            .expect("receipt schema should be readable");
        statement
            .query_map([], |row| row.get::<_, String>(1))
            .expect("receipt columns should be readable")
            .collect::<Result<Vec<_>, _>>()
            .expect("receipt columns should collect")
    };
    assert_eq!(
        receipt_columns,
        [
            "actor_key",
            "scope_key",
            "request_id",
            "command_digest",
            "stream_id",
            "revision",
        ]
    );
    let legacy_digest: String = connection
        .query_row(
            "SELECT command_digest FROM command_receipts WHERE stream_id = 'legacy-stream'",
            [],
            |row| row.get(0),
        )
        .expect("legacy receipt should have one migrated digest");
    assert!(legacy_digest.starts_with("sha256:"));
    assert_eq!(legacy_digest.len(), 71);
    connection.close().expect("migrated database should close");
}

#[test]
fn startup_rejects_a_database_from_a_newer_schema_version() {
    let root = temporary_directory("newer-schema");
    fs::create_dir_all(&root).expect("test directory should exist");
    let database_path = root.join("control-plane.sqlite3");
    let connection = Connection::open(&database_path).expect("test database should open");
    connection
        .pragma_update(None, "user_version", 3)
        .expect("test schema version should be written");
    connection.close().expect("test database should close");

    let Err(error) = SqliteStorage::open(&root) else {
        panic!("a newer schema must not be silently downgraded");
    };

    assert!(error.to_string().contains("unsupported schema version 3"));
    fs::remove_dir_all(root).expect("rejected database should have no open connection");
}

#[test]
fn startup_migrates_v1_receipts_once_without_a_legacy_runtime_lookup_path() {
    let root = temporary_directory("v1-receipt-migration");
    fs::create_dir_all(&root).expect("test directory should exist");
    let database_path = root.join("control-plane.sqlite3");
    create_v1_fixture(&database_path);

    let mut storage = SqliteStorage::open(&root).expect("v1 storage should migrate to v2");
    assert_eq!(
        storage
            .load_state("legacy-stream")
            .expect("migrated state read")
            .expect("migrated state")
            .payload,
        b"legacy"
    );
    assert_eq!(
        storage.pending_events().expect("migrated outbox read"),
        [winwincode_storage::OutboxEvent {
            sequence: 2,
            event_id: "legacy-event".to_owned(),
            topic: "control-plane.state.changed".to_owned(),
            payload: b"event".to_vec(),
        }]
    );

    storage
        .commit(&state_commit(
            (
                "user:new",
                "organization:new",
                "req_01J00000000000000000000002",
            ),
            &format!("sha256:{}", "4".repeat(64)),
            "new-stream",
            0,
            b"new",
            "new-event",
        ))
        .expect("a canonical identity may reuse a v1 globally-scoped request id");
    Box::new(storage).close().expect("storage should close");

    assert_v2_receipt_schema(&database_path);
    fs::remove_dir_all(root).expect("database directory should be released");
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
        receipt_identity(
            "actor'); DROP TABLE command_receipts; --",
            "scope'); DROP TABLE outbox; --",
            "request'); DROP TABLE command_receipts; --",
        ),
        Sha256Digest(format!("sha256:{}", "0".repeat(64))),
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

#[test]
fn receipt_identity_scopes_one_request_id_by_actor_and_full_scope() {
    let root = temporary_directory("receipt-scope");
    let mut storage = SqliteStorage::open(&root).expect("SQLite storage should open");
    let request_id = "req_01J00000000000000000000000";
    let digest = format!("sha256:{}", "1".repeat(64));

    let first = storage
        .commit(&state_commit(
            (
                "user:one",
                "repository:org:workspace:project:repository-one",
                request_id,
            ),
            &digest,
            "stream-one",
            0,
            b"one",
            "event-one",
        ))
        .expect("first identity should commit");
    let second_actor = storage
        .commit(&state_commit(
            (
                "user:two",
                "repository:org:workspace:project:repository-one",
                request_id,
            ),
            &digest,
            "stream-two",
            0,
            b"two",
            "event-two",
        ))
        .expect("another actor may reuse the request id");
    let second_scope = storage
        .commit(&state_commit(
            (
                "user:one",
                "repository:org:workspace:project:repository-two",
                request_id,
            ),
            &digest,
            "stream-three",
            0,
            b"three",
            "event-three",
        ))
        .expect("another full scope may reuse the request id");

    assert!(!first.idempotent_replay);
    assert!(!second_actor.idempotent_replay);
    assert!(!second_scope.idempotent_replay);
    assert_eq!(
        storage
            .load_state("stream-three")
            .expect("third stream read")
            .expect("third stream state")
            .payload,
        b"three"
    );

    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn receipt_identity_replays_only_the_same_digest_and_rejects_a_changed_digest() {
    let root = temporary_directory("receipt-digest");
    let mut storage = SqliteStorage::open(&root).expect("SQLite storage should open");
    let request_id = "req_01J00000000000000000000001";
    let first_digest = format!("sha256:{}", "2".repeat(64));
    let changed_digest = format!("sha256:{}", "3".repeat(64));
    let first = state_commit(
        ("user:one", "project:org:workspace:project", request_id),
        &first_digest,
        "stream-one",
        0,
        b"one",
        "event-original",
    );

    storage.commit(&first).expect("first commit should succeed");
    let replay = storage
        .commit(&state_commit(
            ("user:one", "project:org:workspace:project", request_id),
            &first_digest,
            "ignored-retry-stream",
            99,
            b"ignored retry state",
            "ignored-retry-event",
        ))
        .expect("the same receipt identity and digest should replay");
    assert!(replay.idempotent_replay);
    assert_eq!(replay.stream_id, "stream-one");
    assert_eq!(replay.event_ids, ["event-original"]);

    let error = storage
        .commit(&state_commit(
            ("user:one", "project:org:workspace:project", request_id),
            &changed_digest,
            "stream-one",
            1,
            b"changed",
            "event-changed",
        ))
        .expect_err("the same receipt identity cannot represent another digest");
    assert_eq!(error.kind(), StorageErrorKind::RequestConflict);

    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}
