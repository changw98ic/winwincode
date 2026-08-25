use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use rusqlite::Connection;
use winwincode_domain::{
    ControlPlaneEventId, DeliveryId, ProductSessionId, RequestId, Sha256Digest,
};
use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, NewOutboxEvent,
    ProductStateStorage, ProjectionEventCursor, ProjectionEventStream, ProjectionEventStreamKey,
    ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey, SqliteStorage, StateCommit,
    StorageErrorKind,
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
        vec![NewOutboxEvent::internal(
            event_id,
            "control-plane.state.changed",
            b"event".to_vec(),
        )],
    )
}

fn projection_commit(
    request_id: &str,
    state_stream: &str,
    event_id: &str,
    stream: ProjectionEventStream,
) -> StateCommit {
    StateCommit::new(
        receipt_identity("actor:projection", "scope:repository-one", request_id),
        Sha256Digest(format!("sha256:{}", "d".repeat(64))),
        state_stream,
        0,
        b"projection-state".to_vec(),
        vec![NewOutboxEvent::projection(
            ControlPlaneEventId(event_id.to_owned()),
            "projection.invalidated",
            b"{}".to_vec(),
            stream,
        )],
    )
}

fn projection_key(stream: ProjectionEventStream) -> ProjectionEventStreamKey {
    ProjectionEventStreamKey::new(
        ReceiptScopeKey::from_encoded(b"scope:repository-one".to_vec()).expect("scope key"),
        stream,
    )
    .expect("projection stream key")
}

fn write_interleaved_projection_events(
    storage: &mut SqliteStorage,
    delivery_one: &ProjectionEventStream,
    delivery_two: &ProjectionEventStream,
    session_one: &ProjectionEventStream,
    session_two: &ProjectionEventStream,
) -> ProjectionEventCursor {
    let writes = [
        (
            "req_projection_00000001",
            "state:d1:1",
            "evt_delivery_one_0001",
            delivery_one,
        ),
        (
            "req_projection_00000002",
            "state:d2:1",
            "evt_delivery_two_0001",
            delivery_two,
        ),
        (
            "req_projection_00000003",
            "state:s1:1",
            "evt_session_one_0001",
            session_one,
        ),
        (
            "req_projection_00000004",
            "state:d1:2",
            "evt_delivery_one_0002",
            delivery_one,
        ),
        (
            "req_projection_00000005",
            "state:s2:1",
            "evt_session_two_0001",
            session_two,
        ),
        (
            "req_projection_00000006",
            "state:s1:2",
            "evt_session_one_0002",
            session_one,
        ),
    ];
    let mut first_delivery_cursor = None;
    for (request_id, state_stream, event_id, stream) in writes {
        let receipt = storage
            .commit(&projection_commit(
                request_id,
                state_stream,
                event_id,
                stream.clone(),
            ))
            .expect("projection event should commit");
        let cursor = receipt.events[0]
            .projection_cursor
            .clone()
            .expect("projection cursor");
        if event_id == "evt_delivery_one_0001" {
            first_delivery_cursor = Some(cursor);
        }
    }
    first_delivery_cursor.expect("first Delivery cursor")
}

fn assert_projection_stream_heads(
    storage: &SqliteStorage,
    expected: &[(ProjectionEventStream, u64, &str)],
) {
    for (stream, sequence, event_id) in expected {
        let cursor = storage
            .load_projection_event_cursor(&projection_key(stream.clone()), None)
            .expect("latest stream cursor");
        assert_eq!(cursor.sequence(), *sequence);
        assert_eq!(cursor.event_id().expect("event id").0, *event_id);
    }
}

fn journal_key() -> AggregateJournalKey {
    AggregateJournalKey::new("delivery", "dlv_01J00000000000000000000000")
        .expect("aggregate journal key")
}

fn journal_create(digest: &str, payload: &[u8]) -> AggregateJournalPublication {
    AggregateJournalPublication::Create {
        key: journal_key(),
        manifest: b"delivery-manifest".to_vec(),
        first_record: AggregateJournalRecord::new(1, digest, payload.to_vec()),
    }
}

fn journal_append(
    expected_tail_digest: &str,
    digest: &str,
    payload: &[u8],
) -> AggregateJournalPublication {
    AggregateJournalPublication::Append {
        key: journal_key(),
        expected_tail_sequence: 1,
        expected_tail_digest: expected_tail_digest.to_owned(),
        record: AggregateJournalRecord::new(2, digest, payload.to_vec()),
    }
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

fn create_v2_fixture(database_path: &Path) {
    let connection = Connection::open(database_path).expect("v2 database should open");
    connection
        .execute_batch(
            "CREATE TABLE product_state (
                 stream_id TEXT PRIMARY KEY NOT NULL,
                 revision INTEGER NOT NULL CHECK (revision > 0),
                 payload BLOB NOT NULL
             );
             CREATE TABLE command_receipts (
                 actor_key BLOB NOT NULL,
                 scope_key BLOB NOT NULL,
                 request_id TEXT NOT NULL,
                 command_digest TEXT NOT NULL,
                 stream_id TEXT NOT NULL,
                 revision INTEGER NOT NULL CHECK (revision > 0),
                 PRIMARY KEY (actor_key, scope_key, request_id)
             );
             CREATE TABLE outbox (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 event_id TEXT UNIQUE NOT NULL,
                 receipt_actor_key BLOB NOT NULL,
                 receipt_scope_key BLOB NOT NULL,
                 request_id TEXT NOT NULL,
                 topic TEXT NOT NULL,
                 payload BLOB NOT NULL,
                 published INTEGER NOT NULL DEFAULT 0 CHECK (published IN (0, 1)),
                 FOREIGN KEY (receipt_actor_key, receipt_scope_key, request_id)
                     REFERENCES command_receipts (actor_key, scope_key, request_id)
                     DEFERRABLE INITIALLY DEFERRED
             );
             CREATE INDEX outbox_pending_sequence ON outbox (published, sequence);
             INSERT INTO product_state VALUES ('v2-stream', 1, X'76322D7374617465');
             INSERT INTO command_receipts VALUES (
                 X'76322D6163746F72', X'76322D73636F7065',
                 'req_01J00000000000000000000009',
                 'sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',
                 'v2-stream', 1
             );
             INSERT INTO outbox
                 (event_id, receipt_actor_key, receipt_scope_key, request_id, topic, payload)
                 VALUES (
                     'v2-event', X'76322D6163746F72', X'76322D73636F7065',
                     'req_01J00000000000000000000009',
                     'control-plane.state.changed', X'76322D6576656E74'
                 );
             PRAGMA user_version = 2;",
        )
        .expect("v2 fixture should be created");
    connection.close().expect("v2 fixture should close");
}

fn assert_current_receipt_schema(database_path: &Path) {
    let connection = Connection::open(database_path).expect("migrated database should open");
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("schema version should be readable");
    assert_eq!(version, 4);
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
        .pragma_update(None, "user_version", 5)
        .expect("test schema version should be written");
    connection.close().expect("test database should close");

    let Err(error) = SqliteStorage::open(&root) else {
        panic!("a newer schema must not be silently downgraded");
    };

    assert!(error.to_string().contains("unsupported schema version 5"));
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
            projection_cursor: None,
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

    assert_current_receipt_schema(&database_path);
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn startup_migrates_v2_to_the_single_journal_schema_before_serving() {
    let root = temporary_directory("v2-journal-migration");
    fs::create_dir_all(&root).expect("test directory should exist");
    let database_path = root.join("control-plane.sqlite3");
    create_v2_fixture(&database_path);

    let storage = SqliteStorage::open(&root).expect("v2 storage should migrate to v4");
    assert_eq!(
        storage
            .load_state("v2-stream")
            .expect("migrated state read")
            .expect("migrated state")
            .payload,
        b"v2-state"
    );
    assert_eq!(
        storage.pending_events().expect("migrated outbox")[0].event_id,
        "v2-event"
    );
    assert!(
        storage
            .load_journal(&journal_key())
            .expect("new journal read")
            .is_none()
    );
    Box::new(storage).close().expect("storage should close");

    let connection = Connection::open(&database_path).expect("migrated database");
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("schema version");
    assert_eq!(version, 4);
    for table in ["aggregate_journals", "aggregate_journal_records"] {
        let exists: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .expect("journal table lookup");
        assert_eq!(exists, 1, "{table} must exist after v2 migration");
    }
    connection
        .close()
        .expect("inspection connection should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn interrupted_v2_migration_rolls_back_before_serving() {
    let root = temporary_directory("v2-migration-rollback");
    fs::create_dir_all(&root).expect("test directory should exist");
    let database_path = root.join("control-plane.sqlite3");
    create_v2_fixture(&database_path);
    let connection = Connection::open(&database_path).expect("v2 database");
    connection
        .execute_batch("CREATE VIEW aggregate_journal_records AS SELECT 1 AS incompatible_fixture;")
        .expect("migration blocker should install");
    connection.close().expect("v2 database should close");

    let error = SqliteStorage::open(&root)
        .err()
        .expect("migration failure must prevent storage startup");
    assert!(
        error
            .to_string()
            .contains("aggregate journal record schema is not canonical")
    );

    let connection = Connection::open(&database_path).expect("rolled back database");
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("rolled back version");
    assert_eq!(version, 2);
    let journal_table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema \
             WHERE type = 'table' AND name = 'aggregate_journals'",
            [],
            |row| row.get(0),
        )
        .expect("rolled back journal table lookup");
    assert_eq!(journal_table_count, 0);
    let state: Vec<u8> = connection
        .query_row(
            "SELECT payload FROM product_state WHERE stream_id = 'v2-stream'",
            [],
            |row| row.get(0),
        )
        .expect("v2 state must remain intact");
    assert_eq!(state, b"v2-state");
    connection
        .close()
        .expect("inspection connection should close");
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
fn projection_event_positions_are_stream_local_durable_and_exact() {
    let root = temporary_directory("projection-event-streams");
    let mut storage = SqliteStorage::open(&root).expect("SQLite storage should open");
    let delivery_one = ProjectionEventStream::Delivery(DeliveryId("delivery-one".into()));
    let delivery_two = ProjectionEventStream::Delivery(DeliveryId("delivery-two".into()));
    let session_one = ProjectionEventStream::ProductSession(ProductSessionId("session-one".into()));
    let session_two = ProjectionEventStream::ProductSession(ProductSessionId("session-two".into()));
    let first_delivery_cursor = write_interleaved_projection_events(
        &mut storage,
        &delivery_one,
        &delivery_two,
        &session_one,
        &session_two,
    );
    assert_projection_stream_heads(
        &storage,
        &[
            (delivery_one.clone(), 2, "evt_delivery_one_0002"),
            (delivery_two.clone(), 1, "evt_delivery_two_0001"),
            (session_one.clone(), 2, "evt_session_one_0002"),
            (session_two.clone(), 1, "evt_session_two_0001"),
        ],
    );
    let global = storage.pending_events().expect("global outbox");
    assert_eq!(
        global
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5, 6]
    );
    assert_eq!(
        global
            .iter()
            .map(|event| {
                event
                    .projection_cursor
                    .as_ref()
                    .expect("projection cursor")
                    .sequence()
            })
            .collect::<Vec<_>>(),
        vec![1, 1, 1, 2, 1, 2]
    );

    let first_delivery_key = projection_key(delivery_one);
    let error = storage
        .load_projection_event_cursor(&projection_key(delivery_two), Some(&first_delivery_cursor))
        .expect_err("another Delivery stream must reject this cursor");
    assert_eq!(error.kind(), StorageErrorKind::InvalidInput);
    let foreign_scope_key = ProjectionEventStreamKey::new(
        ReceiptScopeKey::from_encoded(b"scope:repository-two".to_vec()).expect("scope key"),
        session_one,
    )
    .expect("foreign scope stream key");
    let error = storage
        .load_projection_event_cursor(&foreign_scope_key, Some(&first_delivery_cursor))
        .expect_err("another repository scope must reject this cursor");
    assert_eq!(error.kind(), StorageErrorKind::InvalidInput);
    let different_kind_key = projection_key(session_two);
    let error = storage
        .load_projection_event_cursor(&different_kind_key, Some(&first_delivery_cursor))
        .expect_err("another resource stream kind must reject this cursor");
    assert_eq!(error.kind(), StorageErrorKind::InvalidInput);
    assert_eq!(
        storage
            .load_projection_event_cursor(&first_delivery_key, Some(&first_delivery_cursor),)
            .expect("retained historical cursor"),
        first_delivery_cursor
    );
    Box::new(storage).close().expect("storage should close");

    let storage = SqliteStorage::open(&root).expect("storage should restart");
    assert_eq!(
        storage
            .load_projection_event_cursor(&first_delivery_key, None)
            .expect("restarted stream head")
            .sequence(),
        2
    );
    assert_eq!(
        storage
            .load_projection_event_cursor(&first_delivery_key, Some(&first_delivery_cursor),)
            .expect("restarted retained cursor"),
        first_delivery_cursor
    );
    Box::new(storage).close().expect("storage should close");

    let connection =
        Connection::open(root.join("control-plane.sqlite3")).expect("retention fixture database");
    connection
        .execute(
            "DELETE FROM outbox WHERE event_id = 'evt_delivery_one_0001'",
            [],
        )
        .expect("old retained event can be compacted");
    connection.close().expect("fixture should close");
    let storage = SqliteStorage::open(&root).expect("storage after compaction");
    let error = storage
        .load_projection_event_cursor(&first_delivery_key, Some(&first_delivery_cursor))
        .expect_err("compacted exact cursor must expire");
    assert_eq!(error.kind(), StorageErrorKind::EventCursorExpired);

    let forged = ProjectionEventCursor::try_new(
        first_delivery_key.clone(),
        2,
        Some(ControlPlaneEventId("evt_delivery_one_fake".into())),
    )
    .expect("shape-valid forged cursor");
    let error = storage
        .load_projection_event_cursor(&first_delivery_key, Some(&forged))
        .expect_err("eventId mismatch is not retention loss");
    assert_eq!(error.kind(), StorageErrorKind::InvalidInput);

    let future = ProjectionEventCursor::try_new(
        first_delivery_key.clone(),
        3,
        Some(ControlPlaneEventId("evt_delivery_one_future".into())),
    )
    .expect("shape-valid future cursor");
    let error = storage
        .load_projection_event_cursor(&first_delivery_key, Some(&future))
        .expect_err("a cursor beyond the durable stream head was never retained");
    assert_eq!(error.kind(), StorageErrorKind::InvalidInput);

    let empty_key = projection_key(ProjectionEventStream::Delivery(DeliveryId(
        "delivery-never-written".into(),
    )));
    let never_issued = ProjectionEventCursor::try_new(
        empty_key.clone(),
        1,
        Some(ControlPlaneEventId("evt_delivery_never_written".into())),
    )
    .expect("shape-valid never-issued cursor");
    let error = storage
        .load_projection_event_cursor(&empty_key, Some(&never_issued))
        .expect_err("a positive cursor for an empty stream was never retained");
    assert_eq!(error.kind(), StorageErrorKind::InvalidInput);
    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
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
        vec![NewOutboxEvent::internal(
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
    assert_eq!(replay.events[0].event_id, "event-original");

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

#[test]
fn one_commit_makes_state_journal_receipt_and_outbox_authoritative_together() {
    let root = temporary_directory("atomic-journal-create");
    let mut storage = SqliteStorage::open(&root).expect("SQLite storage should open");
    let commit = state_commit(
        (
            "user:one",
            "repository:org:workspace:project:repository-one",
            "req_01J00000000000000000000003",
        ),
        &format!("sha256:{}", "5".repeat(64)),
        "delivery:one",
        0,
        b"canonical-delivery-v1",
        "job-event-one",
    )
    .with_journal_publication(journal_create("record-one", b"journal-record-one"));

    let receipt = storage
        .commit(&commit)
        .expect("one transaction should commit");

    assert_eq!(receipt.revision, 1);
    assert!(!receipt.idempotent_replay);
    assert_eq!(receipt.events.len(), 1);
    assert_eq!(receipt.events[0].event_id, "job-event-one");
    assert_eq!(receipt.events[0].payload, b"event");
    let state = storage
        .load_state("delivery:one")
        .expect("state read")
        .expect("committed state");
    assert_eq!(
        (state.revision, state.payload),
        (1, b"canonical-delivery-v1".to_vec())
    );
    let journal = storage
        .load_journal(&journal_key())
        .expect("journal read")
        .expect("committed journal");
    assert_eq!(journal.manifest, b"delivery-manifest");
    assert_eq!(journal.records.len(), 1);
    assert_eq!(journal.records[0].sequence, 1);
    assert_eq!(journal.records[0].digest, "record-one");
    assert_eq!(journal.records[0].payload, b"journal-record-one");

    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

fn install_failing_insert_trigger(database_path: &Path, table: &str) {
    let connection = Connection::open(database_path).expect("failure injection database");
    let trigger = match table {
        "product_state" => {
            "CREATE TRIGGER fail_product_state_insert BEFORE INSERT ON product_state \
             BEGIN SELECT RAISE(ABORT, 'injected product_state failure'); END;"
        }
        "aggregate_journal_records" => {
            "CREATE TRIGGER fail_aggregate_journal_records_insert \
             BEFORE INSERT ON aggregate_journal_records \
             BEGIN SELECT RAISE(ABORT, 'injected aggregate_journal_records failure'); END;"
        }
        "command_receipts" => {
            "CREATE TRIGGER fail_command_receipts_insert BEFORE INSERT ON command_receipts \
             BEGIN SELECT RAISE(ABORT, 'injected command_receipts failure'); END;"
        }
        "outbox" => {
            "CREATE TRIGGER fail_outbox_insert BEFORE INSERT ON outbox \
             BEGIN SELECT RAISE(ABORT, 'injected outbox failure'); END;"
        }
        _ => panic!("unsupported failure injection table"),
    };
    connection
        .execute_batch(trigger)
        .expect("failure trigger should install");
    connection.close().expect("failure injector should close");
}

fn table_count(database_path: &Path, table: &str) -> i64 {
    let connection = Connection::open(database_path).expect("inspection database");
    let query = match table {
        "product_state" => "SELECT COUNT(*) FROM product_state",
        "aggregate_journals" => "SELECT COUNT(*) FROM aggregate_journals",
        "aggregate_journal_records" => "SELECT COUNT(*) FROM aggregate_journal_records",
        "command_receipts" => "SELECT COUNT(*) FROM command_receipts",
        "outbox" => "SELECT COUNT(*) FROM outbox",
        "projection_event_stream_heads" => "SELECT COUNT(*) FROM projection_event_stream_heads",
        _ => panic!("unsupported inspection table"),
    };
    let count = connection
        .query_row(query, [], |row| row.get(0))
        .expect("table count");
    connection
        .close()
        .expect("inspection connection should close");
    count
}

#[test]
fn failure_at_each_atomic_member_rolls_back_every_member() {
    for failing_table in [
        "product_state",
        "aggregate_journal_records",
        "command_receipts",
        "outbox",
    ] {
        let root = temporary_directory(&format!("rollback-{failing_table}"));
        let storage = SqliteStorage::open(&root).expect("schema should be created");
        let database_path = storage.database_path().to_path_buf();
        Box::new(storage)
            .close()
            .expect("bootstrap storage should close");
        install_failing_insert_trigger(&database_path, failing_table);

        let mut storage = SqliteStorage::open(&root).expect("storage with trigger should open");
        let commit = state_commit(
            (
                "user:rollback",
                "repository:rollback",
                "req_01J00000000000000000000004",
            ),
            &format!("sha256:{}", "6".repeat(64)),
            "delivery:rollback",
            0,
            b"must-not-commit",
            "must-not-commit-event",
        )
        .with_journal_publication(journal_create(
            "must-not-commit-record",
            b"must-not-commit-record",
        ));

        let error = storage
            .commit(&commit)
            .expect_err("injected failure must abort the transaction");
        assert_eq!(error.kind(), StorageErrorKind::Adapter);
        Box::new(storage)
            .close()
            .expect("failed storage should close");

        for table in [
            "product_state",
            "aggregate_journals",
            "aggregate_journal_records",
            "command_receipts",
            "outbox",
        ] {
            assert_eq!(
                table_count(&database_path, table),
                0,
                "{failing_table} failure left a partial row in {table}"
            );
        }
        fs::remove_dir_all(root).expect("database directory should be released");
    }
}

#[test]
fn later_outbox_failure_rolls_back_the_first_projection_cursor() {
    let root = temporary_directory("projection-cursor-rollback");
    let storage = SqliteStorage::open(&root).expect("schema should be created");
    let database_path = storage.database_path().to_path_buf();
    Box::new(storage)
        .close()
        .expect("bootstrap storage should close");

    let connection = Connection::open(&database_path).expect("failure injection database");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_second_projection_event BEFORE INSERT ON outbox \
             WHEN NEW.event_id = 'evt_projection_rollback_0002' \
             BEGIN SELECT RAISE(ABORT, 'injected second event failure'); END;",
        )
        .expect("failure trigger should install");
    connection.close().expect("failure injector should close");

    let stream = ProjectionEventStream::Delivery(DeliveryId("delivery-rollback".into()));
    let mut storage = SqliteStorage::open(&root).expect("storage with trigger should open");
    let commit = StateCommit::new(
        receipt_identity(
            "actor:projection-rollback",
            "scope:repository-one",
            "req_projection_rollback_0001",
        ),
        Sha256Digest(format!("sha256:{}", "e".repeat(64))),
        "state:projection-rollback",
        0,
        b"must-not-commit".to_vec(),
        vec![
            NewOutboxEvent::projection(
                ControlPlaneEventId("evt_projection_rollback_0001".into()),
                "projection.invalidated",
                b"{}".to_vec(),
                stream.clone(),
            ),
            NewOutboxEvent::projection(
                ControlPlaneEventId("evt_projection_rollback_0002".into()),
                "projection.invalidated",
                b"{}".to_vec(),
                stream.clone(),
            ),
        ],
    );
    let error = storage
        .commit(&commit)
        .expect_err("the second outbox insert must abort the whole transaction");
    assert_eq!(error.kind(), StorageErrorKind::Adapter);
    let cursor = storage
        .load_projection_event_cursor(&projection_key(stream), None)
        .expect("rolled-back stream remains empty");
    assert_eq!(cursor.sequence(), 0);
    assert_eq!(cursor.event_id(), None);
    Box::new(storage).close().expect("storage should close");

    for table in [
        "product_state",
        "command_receipts",
        "outbox",
        "projection_event_stream_heads",
    ] {
        assert_eq!(
            table_count(&database_path, table),
            0,
            "late event failure left a partial row in {table}"
        );
    }
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn journal_tail_compare_and_append_allows_only_one_concurrent_winner() {
    let root = temporary_directory("journal-tail-cas");
    let mut bootstrap = SqliteStorage::open(&root).expect("SQLite storage should open");
    bootstrap
        .commit(
            &state_commit(
                (
                    "user:bootstrap",
                    "repository:bootstrap",
                    "req_01J00000000000000000000005",
                ),
                &format!("sha256:{}", "7".repeat(64)),
                "bootstrap",
                0,
                b"bootstrap",
                "bootstrap-event",
            )
            .with_journal_publication(journal_create("record-one", b"record-one")),
        )
        .expect("first journal record");
    Box::new(bootstrap).close().expect("bootstrap should close");

    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for ordinal in 0..2 {
        let root = root.clone();
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            let mut storage = SqliteStorage::open(&root).expect("contender storage should open");
            let stream_id = format!("contender-{ordinal}");
            let state = format!("state-{ordinal}");
            let event_id = format!("event-{ordinal}");
            let record_digest = format!("record-{ordinal}");
            let record_payload = format!("record-{ordinal}");
            let digest_nibble = if ordinal == 0 { "8" } else { "9" };
            let commit = state_commit(
                (
                    if ordinal == 0 { "user:one" } else { "user:two" },
                    "repository:race",
                    if ordinal == 0 {
                        "req_01J00000000000000000000006"
                    } else {
                        "req_01J00000000000000000000007"
                    },
                ),
                &format!("sha256:{}", digest_nibble.repeat(64)),
                &stream_id,
                0,
                state.as_bytes(),
                &event_id,
            )
            .with_journal_publication(journal_append(
                "record-one",
                &record_digest,
                record_payload.as_bytes(),
            ));
            barrier.wait();
            let result = storage.commit(&commit);
            Box::new(storage).close().expect("contender should close");
            result
        }));
    }
    barrier.wait();
    let results = workers
        .into_iter()
        .map(|worker| worker.join().expect("contender thread"))
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let error = results
        .iter()
        .find_map(|result| result.as_ref().err())
        .expect("one contender should lose");
    assert_eq!(error.kind(), StorageErrorKind::JournalConflict);

    let storage = SqliteStorage::open(&root).expect("inspection storage should open");
    let journal = storage
        .load_journal(&journal_key())
        .expect("journal read")
        .expect("journal");
    assert_eq!(journal.records.len(), 2);
    assert_eq!(storage.pending_events().expect("pending events").len(), 2);
    Box::new(storage)
        .close()
        .expect("inspection storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn scoped_request_replay_returns_the_original_event_without_duplicate_rows() {
    let root = temporary_directory("journal-request-replay");
    let mut storage = SqliteStorage::open(&root).expect("SQLite storage should open");
    let identity = (
        "user:one",
        "repository:one",
        "req_01J00000000000000000000008",
    );
    let digest = format!("sha256:{}", "a".repeat(64));
    storage
        .commit(
            &state_commit(
                identity,
                &digest,
                "delivery:original",
                0,
                b"original-state",
                "original-job-event",
            )
            .with_journal_publication(journal_create("record-one", b"original-record")),
        )
        .expect("original commit");

    let replay = storage
        .commit(
            &state_commit(
                identity,
                &digest,
                "delivery:retry-must-be-ignored",
                99,
                b"retry-state-must-be-ignored",
                "retry-event-must-be-ignored",
            )
            .with_journal_publication(journal_append(
                "record-one",
                "retry-record",
                b"retry-record-must-be-ignored",
            )),
        )
        .expect("same scoped command should replay");

    assert!(replay.idempotent_replay);
    assert_eq!(replay.stream_id, "delivery:original");
    assert_eq!(replay.revision, 1);
    assert_eq!(replay.events.len(), 1);
    assert_eq!(replay.events[0].event_id, "original-job-event");
    assert_eq!(replay.events[0].payload, b"event");
    assert_eq!(storage.pending_events().expect("pending events").len(), 1);
    let journal = storage
        .load_journal(&journal_key())
        .expect("journal read")
        .expect("journal");
    assert_eq!(journal.records.len(), 1);
    assert_eq!(journal.records[0].payload, b"original-record");
    assert!(
        storage
            .load_state("delivery:retry-must-be-ignored")
            .expect("retry state query")
            .is_none()
    );

    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn replay_only_commit_refuses_to_fill_a_journal_without_its_original_receipt() {
    let root = temporary_directory("partial-journal-replay");
    let storage = SqliteStorage::open(&root).expect("schema should be created");
    let database_path = storage.database_path().to_path_buf();
    Box::new(storage).close().expect("bootstrap storage close");
    let connection = Connection::open(&database_path).expect("partial journal fixture");
    connection
        .execute(
            "INSERT INTO aggregate_journals (aggregate_type, aggregate_id, manifest) \
             VALUES ('delivery', 'dlv_01J00000000000000000000000', X'6D616E6966657374')",
            [],
        )
        .expect("partial journal manifest");
    connection
        .execute(
            "INSERT INTO aggregate_journal_records \
             (aggregate_type, aggregate_id, sequence, digest, payload) \
             VALUES (
                 'delivery', 'dlv_01J00000000000000000000000', 1,
                 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',
                 X'7265636F7264'
             )",
            [],
        )
        .expect("partial journal record");
    connection.close().expect("fixture connection close");

    let mut storage = SqliteStorage::open(&root).expect("partial storage should open");
    let commit = state_commit(
        (
            "user:partial",
            "repository:partial",
            "req_01J00000000000000000000010",
        ),
        &format!("sha256:{}", "c".repeat(64)),
        "delivery:partial",
        0,
        b"recomputed-state-must-not-commit",
        "recomputed-event-must-not-commit",
    )
    .require_receipt_replay();

    let error = storage
        .commit(&commit)
        .expect_err("missing original receipt must fail closed");
    assert_eq!(error.kind(), StorageErrorKind::RequestReplayMissing);
    assert!(
        storage
            .load_state("delivery:partial")
            .expect("partial state read")
            .is_none()
    );
    assert!(
        storage
            .pending_events()
            .expect("partial outbox read")
            .is_empty()
    );
    let journal = storage
        .load_journal(&journal_key())
        .expect("partial journal read")
        .expect("partial journal remains for diagnosis");
    assert_eq!(journal.records.len(), 1);

    Box::new(storage).close().expect("storage should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}
