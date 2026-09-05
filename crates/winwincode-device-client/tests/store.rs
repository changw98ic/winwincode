// SPDX-License-Identifier: Apache-2.0

//! Durable round trips for the local device-client store: schema migration,
//! path-mapping CRUD, the client outbox, and the server-to-client inbox
//! cursor. The temporary-directory infrastructure mirrors
//! `crates/winwincode-storage/tests/sqlite.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;
use winwincode_client_port::exchange::{FrameCodec, FrameOutbox, OutboxSession};
use winwincode_client_port::messages::{
    CLIENT_CONTROL_PORT_SCHEMA_VERSION, ClientRepositoryRemovedPayload, ClientToServerEnvelope,
    ClientToServerMessage, CommandContext,
};
use winwincode_device_client::{
    ClientInboxCursorUpdate, DeviceStore, DeviceStoreErrorKind, PathMappingRecord,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-device-client-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn open_store(name: &str) -> (PathBuf, DeviceStore) {
    let root = temporary_directory(name);
    let store = DeviceStore::open(&root).expect("device store should open");
    (root, store)
}

fn envelope(message_id: &str, client_instance_id: &str, sequence: u64) -> ClientToServerEnvelope {
    ClientToServerEnvelope {
        schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
        message_id: message_id.to_owned(),
        client_node_id: "node_local".to_owned(),
        client_instance_id: client_instance_id.to_owned(),
        sequence,
        occurred_at: "2026-09-04T00:00:00.000Z".to_owned(),
        message: ClientToServerMessage::RepositoryRemoved(ClientRepositoryRemovedPayload {
            command: CommandContext {
                expected_revision: 3,
                idempotency_key: format!("idem-{message_id}"),
            },
            repository_binding_id: "binding-one".to_owned(),
        }),
    }
}

#[test]
fn open_migrates_the_full_local_schema_and_round_trips_it() {
    assert_eq!(winwincode_device_client::CLIENT_STORE_SCHEMA_VERSION, 2);
    let (root, mut store) = open_store("schema-round-trip");
    let database_path = store.database_path().to_path_buf();
    let canonical_root = fs::canonicalize(&root).expect("root should canonicalize");
    assert_eq!(
        database_path.parent().map(Path::to_path_buf),
        Some(canonical_root),
        "the store must canonicalize its data directory before opening"
    );
    assert_eq!(
        database_path.file_name(),
        Some(std::ffi::OsStr::new("device-client.sqlite3"))
    );
    store
        .put_path_mapping(&PathMappingRecord {
            repository_binding_id: "binding-schema".to_owned(),
            canonical_path: "/Users/dev/project-a".to_owned(),
            git_common_directory: Some("/Users/dev/project-a/.git".to_owned()),
            last_canonicalized_at: Some("2026-09-04T00:00:00.000Z".to_owned()),
            local_state: "ready".to_owned(),
        })
        .expect("path mapping write before restart");
    store.close().expect("store should close");

    let store = DeviceStore::open(&root).expect("restarted store should open");
    let mapping = store
        .path_mapping("binding-schema")
        .expect("restarted path mapping read")
        .expect("path mapping should survive the restart");
    assert_eq!(mapping.canonical_path, "/Users/dev/project-a");
    store.close().expect("restarted store should close");

    let connection = Connection::open(&database_path).expect("migrated database should open");
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("schema version should be readable");
    assert_eq!(version, 2);
    assert_canonical_local_tables(&connection);
    connection.close().expect("inspection close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

fn assert_canonical_local_tables(connection: &Connection) {
    for (table, pragma, expected_columns) in CANONICAL_LOCAL_TABLES {
        let mut statement = connection.prepare(pragma).expect("pragma should prepare");
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .expect("columns should be readable")
            .collect::<Result<Vec<_>, _>>()
            .expect("columns should collect");
        assert_eq!(
            columns, *expected_columns,
            "{table} columns must be canonical"
        );
    }
}

const CANONICAL_LOCAL_TABLES: &[(&str, &str, &[&str])] = &[
    (
        "device_identity",
        "PRAGMA table_info(device_identity)",
        &[
            "device_id",
            "client_node_id",
            "public_client_id",
            "display_name",
            "platform",
            "architecture",
            "client_version",
            "current_instance_id",
            "created_at",
            "revision",
        ],
    ),
    (
        "device_credential",
        "PRAGMA table_info(device_credential)",
        &[
            "device_id",
            "credential_secret",
            "credential_digest",
            "credential_generation",
            "rotated_at",
        ],
    ),
    (
        "server_profile",
        "PRAGMA table_info(server_profile)",
        &[
            "server_profile_id",
            "base_url",
            "display_name",
            "created_at",
            "last_connected_at",
        ],
    ),
    (
        "repository_path_mapping",
        "PRAGMA table_info(repository_path_mapping)",
        &[
            "repository_binding_id",
            "canonical_path",
            "git_common_directory",
            "last_canonicalized_at",
            "local_state",
        ],
    ),
    (
        "repository_local_state",
        "PRAGMA table_info(repository_local_state)",
        &[
            "repository_binding_id",
            "dirty_state",
            "availability",
            "head_commit",
            "last_scanned_at",
            "updated_at",
        ],
    ),
    (
        "occupancy_mirror",
        "PRAGMA table_info(occupancy_mirror)",
        &[
            "occupancy_lease_id",
            "repository_binding_id",
            "user_id",
            "state",
            "fencing_token",
            "acquired_at",
            "expires_at",
            "updated_at",
        ],
    ),
    (
        "worker_process_registry",
        "PRAGMA table_info(worker_process_registry)",
        &[
            "worker_session_id",
            "worker_id",
            "worker_instance_id",
            "pid",
            "process_start_identity",
            "repository_binding_id",
            "occupancy_lease_id",
            "launch_grant_id",
            "data_directory",
            "state",
            "last_observed_at",
        ],
    ),
    (
        "worker_launch_receipts",
        "PRAGMA table_info(worker_launch_receipts)",
        &[
            "launch_grant_id",
            "worker_session_id",
            "ack_status",
            "idempotency_key",
            "receipt_payload",
            "received_at",
        ],
    ),
    (
        "candidate_local_refs",
        "PRAGMA table_info(candidate_local_refs)",
        &[
            "candidate_id",
            "worker_session_id",
            "repository_binding_id",
            "local_git_ref",
            "local_state",
            "created_at",
        ],
    ),
    (
        "client_outbox",
        "PRAGMA table_info(client_outbox)",
        &[
            "outbox_sequence",
            "message_id",
            "client_node_id",
            "client_instance_id",
            "envelope_sequence",
            "kind",
            "payload",
            "occurred_at",
            "published",
        ],
    ),
    (
        "client_inbox_cursor",
        "PRAGMA table_info(client_inbox_cursor)",
        &[
            "server_profile_id",
            "last_sequence",
            "last_message_id",
            "updated_at",
        ],
    ),
];

#[test]
fn startup_rejects_a_database_from_a_newer_schema_version() {
    let (root, store) = open_store("newer-schema");
    let database_path = store.database_path().to_path_buf();
    store.close().expect("store should close");
    let newer_version = winwincode_device_client::CLIENT_STORE_SCHEMA_VERSION + 1;
    let connection = Connection::open(&database_path).expect("test database should open");
    connection
        .pragma_update(None, "user_version", newer_version)
        .expect("test schema version should be written");
    connection.close().expect("test database should close");

    let Err(error) = DeviceStore::open(&root) else {
        panic!("a newer schema must not be silently downgraded");
    };
    assert!(
        error
            .to_string()
            .contains(&format!("unsupported schema version {newer_version}"))
    );
    fs::remove_dir_all(root).expect("rejected database should have no open connection");
}

#[test]
fn path_mapping_crud_round_trips_local_only_rows() {
    let (root, mut store) = open_store("path-mapping-crud");
    let first = PathMappingRecord {
        repository_binding_id: "binding-b".to_owned(),
        canonical_path: "/Users/dev/project-b".to_owned(),
        git_common_directory: None,
        last_canonicalized_at: None,
        local_state: "registered".to_owned(),
    };
    let second = PathMappingRecord {
        repository_binding_id: "binding-a".to_owned(),
        canonical_path: "/Users/dev/project-a".to_owned(),
        git_common_directory: Some("/Users/dev/project-a/.git".to_owned()),
        last_canonicalized_at: Some("2026-09-04T00:00:00.000Z".to_owned()),
        local_state: "ready".to_owned(),
    };
    store.put_path_mapping(&first).expect("first insert");
    store.put_path_mapping(&second).expect("second insert");

    let loaded = store
        .path_mapping("binding-b")
        .expect("read")
        .expect("row should exist");
    assert_eq!(loaded, first);
    assert!(
        store
            .path_mapping("binding-missing")
            .expect("read")
            .is_none()
    );
    assert_eq!(
        store.path_mappings().expect("list"),
        [second.clone(), first.clone()]
    );

    let moved = PathMappingRecord {
        repository_binding_id: "binding-b".to_owned(),
        canonical_path: "/Volumes/work/project-b-moved".to_owned(),
        git_common_directory: Some("/Volumes/work/project-b-moved/.git".to_owned()),
        last_canonicalized_at: Some("2026-09-04T01:00:00.000Z".to_owned()),
        local_state: "ready".to_owned(),
    };
    store.put_path_mapping(&moved).expect("upsert replaces");
    assert_eq!(
        store.path_mapping("binding-b").expect("read").expect("row"),
        moved
    );
    assert_eq!(store.path_mappings().expect("list").len(), 2);

    assert!(
        store
            .delete_path_mapping("binding-b")
            .expect("delete existing")
    );
    assert!(
        !store
            .delete_path_mapping("binding-b")
            .expect("delete missing")
    );
    assert!(store.path_mapping("binding-b").expect("read").is_none());

    let invalid = PathMappingRecord {
        repository_binding_id: "binding-c".to_owned(),
        canonical_path: String::new(),
        git_common_directory: None,
        last_canonicalized_at: None,
        local_state: "ready".to_owned(),
    };
    let error = store
        .put_path_mapping(&invalid)
        .expect_err("an empty canonical path must be rejected");
    assert_eq!(error.kind(), DeviceStoreErrorKind::InvalidInput);

    store.close().expect("store should close");
    let store = DeviceStore::open(&root).expect("restarted store should open");
    let remaining = store.path_mappings().expect("restarted list");
    assert_eq!(remaining, [second]);
    store.close().expect("restarted store should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn outbox_append_publish_and_sender_sequence_rules_round_trip() {
    let (root, mut store) = open_store("outbox-round-trip");
    let first = envelope("msg-one", "inst-a", 1);
    let second = envelope("msg-two", "inst-a", 2);
    let other_instance = envelope("msg-three", "inst-b", 1);

    assert_eq!(
        store
            .append_outbox_envelope(&first, "client.repository.removed")
            .expect("first append"),
        1
    );
    store
        .append_outbox_envelope(&other_instance, "client.hello")
        .expect("other sender append");
    store
        .append_outbox_envelope(&second, "client.repository.removed")
        .expect("second append");

    let pending = store.pending_outbox_envelopes().expect("pending read");
    assert_eq!(pending.len(), 3);
    assert_eq!(pending[0].message_id, "msg-one");
    assert_eq!(pending[0].outbox_sequence, 1);
    assert_eq!(pending[0].envelope_sequence, 1);
    assert_eq!(pending[0].kind, "client.repository.removed");
    assert_eq!(pending[1].message_id, "msg-three");
    assert_eq!(pending[2].envelope_sequence, 2);
    let decoded: ClientToServerEnvelope =
        serde_json::from_slice(&pending[0].payload).expect("stored payload should decode");
    assert_eq!(decoded, first);

    let duplicate = store
        .append_outbox_envelope(
            &envelope("msg-one", "inst-a", 3),
            "client.repository.removed",
        )
        .expect_err("a duplicate message id must conflict");
    assert_eq!(duplicate.kind(), DeviceStoreErrorKind::Conflict);

    let backwards = store
        .append_outbox_envelope(
            &envelope("msg-four", "inst-a", 2),
            "client.repository.removed",
        )
        .expect_err("a non-advancing sender sequence must conflict");
    assert_eq!(backwards.kind(), DeviceStoreErrorKind::Conflict);

    let mut foreign_schema = envelope("msg-five", "inst-a", 3);
    foreign_schema.schema_version = "other/v9".to_owned();
    let foreign = store
        .append_outbox_envelope(&foreign_schema, "client.hello")
        .expect_err("a foreign schema version must be rejected");
    assert_eq!(foreign.kind(), DeviceStoreErrorKind::InvalidInput);

    store
        .mark_outbox_published("msg-one")
        .expect("publish acknowledgement");
    store
        .mark_outbox_published("msg-three")
        .expect("publish acknowledgement");
    let pending = store.pending_outbox_envelopes().expect("pending read");
    assert_eq!(
        pending
            .iter()
            .map(|entry| entry.message_id.as_str())
            .collect::<Vec<_>>(),
        ["msg-two"]
    );
    let unknown = store
        .mark_outbox_published("msg-missing")
        .expect_err("an unknown message id must not be found");
    assert_eq!(unknown.kind(), DeviceStoreErrorKind::NotFound);

    store.close().expect("store should close");
    let store = DeviceStore::open(&root).expect("restarted store should open");
    let pending = store.pending_outbox_envelopes().expect("restarted pending");
    assert_eq!(
        pending
            .iter()
            .map(|entry| entry.message_id.as_str())
            .collect::<Vec<_>>(),
        ["msg-two"],
        "published state must survive the restart"
    );
    store.close().expect("restarted store should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn inbox_cursor_is_strictly_forward_only_and_durable() {
    let (root, mut store) = open_store("inbox-cursor");
    let update = |last_sequence: u64, last_message_id: Option<&str>| ClientInboxCursorUpdate {
        server_profile_id: "server-one".to_owned(),
        last_sequence,
        last_message_id: last_message_id.map(str::to_owned),
        updated_at: "2026-09-04T00:00:00.000Z".to_owned(),
    };

    assert!(store.inbox_cursor("server-one").expect("read").is_none());
    store
        .advance_inbox_cursor(&update(0, None))
        .expect("initial");
    store
        .advance_inbox_cursor(&update(5, Some("srv-msg-5")))
        .expect("forward advance");
    let cursor = store
        .inbox_cursor("server-one")
        .expect("read")
        .expect("cursor should exist");
    assert_eq!(cursor.last_sequence, 5);
    assert_eq!(cursor.last_message_id.as_deref(), Some("srv-msg-5"));

    let backwards = store
        .advance_inbox_cursor(&update(4, Some("srv-msg-4")))
        .expect_err("a backwards cursor update must conflict");
    assert_eq!(backwards.kind(), DeviceStoreErrorKind::Conflict);
    let diverging = store
        .advance_inbox_cursor(&update(5, Some("srv-msg-other")))
        .expect_err("a diverging same-position update must conflict");
    assert_eq!(diverging.kind(), DeviceStoreErrorKind::Conflict);
    store
        .advance_inbox_cursor(&update(5, Some("srv-msg-5")))
        .expect("an idempotent same-position update must be accepted");

    store.close().expect("store should close");
    let mut store = DeviceStore::open(&root).expect("restarted store should open");
    let cursor = store
        .inbox_cursor("server-one")
        .expect("restarted read")
        .expect("cursor should survive the restart");
    assert_eq!(cursor.last_sequence, 5);
    store
        .advance_inbox_cursor(&update(7, Some("srv-msg-7")))
        .expect("advance after restart");
    assert_eq!(
        store
            .inbox_cursor("server-one")
            .expect("read")
            .expect("cursor")
            .last_sequence,
        7
    );
    store.close().expect("restarted store should close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn sqlite_adapter_uses_no_dynamic_savepoint_or_sql_identifier_path() {
    for source in [
        include_str!("../src/store.rs"),
        include_str!("../src/identity.rs"),
    ] {
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
    }
    assert!(include_str!("../src/store.rs").contains("params!["));
}

#[test]
fn frame_outbox_maps_client_outbox_rows_to_stored_frames() {
    let (root, mut store) = open_store("outbox-trait");
    let codec = FrameCodec::default();
    let session = OutboxSession::new();
    store
        .bind_outbox_stream("node_local", "inst-a")
        .expect("bind the sender stream");

    // An unbound stream is rejected before any mutation.
    let mut unbound = DeviceStore::open(&root).expect("second store handle");
    assert_eq!(
        FrameOutbox::load(&mut unbound)
            .expect_err("an unbound stream must be rejected")
            .kind(),
        DeviceStoreErrorKind::InvalidInput
    );
    unbound.close().expect("second store handle close");

    // The stream starts empty and appends persist the exact frame bytes.
    assert_eq!(session.next_sequence(&mut store).expect("next"), 1);
    let stored = codec
        .encode_envelope(&envelope("msg-one", "inst-a", 1))
        .expect("seal frame");
    session.enqueue(&mut store, 1, &stored).expect("append");
    let stored = codec
        .encode_envelope(&envelope("msg-two", "inst-a", 2))
        .expect("seal frame");
    session.enqueue(&mut store, 2, &stored).expect("append");

    let snapshot = FrameOutbox::load(&mut store)
        .expect("load")
        .expect("the stream has rows");
    assert_eq!(snapshot.ack_sequence, 0);
    assert_eq!(snapshot.highest_sequence, 2);
    assert_eq!(
        snapshot
            .frames
            .iter()
            .map(|frame| frame.message_id.as_str())
            .collect::<Vec<_>>(),
        ["msg-one", "msg-two"]
    );
    assert_eq!(snapshot.frames[0].sequence, 1);
    assert!(
        snapshot.frames[0].payload_digest.starts_with("sha256:"),
        "the digest is re-derived from the stored envelope payload"
    );
    let decoded: ClientToServerEnvelope = codec
        .decode(&snapshot.frames[0].frame)
        .expect("stored bytes decode");
    assert_eq!(decoded.message_id, "msg-one");

    // A stale compare-and-append base is rejected.
    let stored = codec
        .encode_envelope(&envelope("msg-three", "inst-a", 3))
        .expect("seal frame");
    assert!(
        session.enqueue(&mut store, 0, &stored).is_err(),
        "append must verify the expected high-water mark"
    );

    store.close().expect("store close");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn frame_outbox_acknowledgement_compaction_and_cursors_stay_durable() {
    let (root, mut store) = open_store("outbox-trait-cursors");
    let session = OutboxSession::new();
    store
        .bind_outbox_stream("node_local", "inst-a")
        .expect("bind the sender stream");
    let codec = FrameCodec::default();
    for (message_id, sequence) in [("msg-one", 1), ("msg-two", 2)] {
        let stored = codec
            .encode_envelope(&envelope(message_id, "inst-a", sequence))
            .expect("seal frame");
        session
            .enqueue(&mut store, sequence, &stored)
            .expect("append");
    }

    // The peer acknowledgement marks rows published; compaction moves them
    // out of the delivery set while both durable cursors survive.
    session.acknowledge(&mut store, 1).expect("acknowledge");
    assert_eq!(
        store
            .pending_outbox_envelopes()
            .expect("pending")
            .iter()
            .map(|entry| entry.message_id.as_str())
            .collect::<Vec<_>>(),
        ["msg-two"],
        "only the acknowledged prefix left the delivery set"
    );
    session.compact_confirmed(&mut store).expect("compact");
    let snapshot = FrameOutbox::load(&mut store)
        .expect("load")
        .expect("cursors survive compaction");
    assert_eq!(snapshot.ack_sequence, 1);
    assert_eq!(snapshot.highest_sequence, 2);
    assert_eq!(
        snapshot
            .frames
            .iter()
            .map(|frame| frame.message_id.as_str())
            .collect::<Vec<_>>(),
        ["msg-two"],
        "unconfirmed frames stay retained"
    );

    // Cursors survive a full close and reopen, even with rows retained.
    store.close().expect("store close");
    let mut store = DeviceStore::open(&root).expect("restarted store");
    store
        .bind_outbox_stream("node_local", "inst-a")
        .expect("rebind the stream");
    let snapshot = FrameOutbox::load(&mut store)
        .expect("restarted load")
        .expect("snapshot survives the restart");
    assert_eq!(snapshot.ack_sequence, 1);
    assert_eq!(snapshot.highest_sequence, 2);
    assert_eq!(session.next_sequence(&mut store).expect("next"), 3);

    // The enrollment adoption re-keys the acknowledged prefix onto the
    // assigned node at the same sequences and rebinds the stream: the next
    // sequence continues where the assigned stream expects it (the server
    // credited the acknowledged enroll sequence), and pending placeholder
    // rows are untouched.
    store
        .adopt_enrolled_stream("node_local", "cnd_ASSIGNEDNODE1A1A1A1A1A1")
        .expect("adopt the enrolled stream");
    let adopted_snapshot = FrameOutbox::load(&mut store)
        .expect("adopted load")
        .expect("the assigned stream has the acknowledged prefix");
    assert_eq!(adopted_snapshot.ack_sequence, 1);
    assert_eq!(adopted_snapshot.highest_sequence, 1);
    assert!(
        adopted_snapshot.frames.is_empty(),
        "the acknowledged prefix was copied as confirmed: {adopted_snapshot:?}"
    );
    assert_eq!(session.next_sequence(&mut store).expect("next"), 2);
    let rows = store.pending_outbox_envelopes().expect("all pending rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].client_node_id, "node_local");
    assert_eq!(rows[0].message_id, "msg-two");
    assert_eq!(rows[0].client_instance_id, "inst-a");
    let replayed_adoption =
        store.adopt_enrolled_stream("node_local", "cnd_ASSIGNEDNODE1A1A1A1A1A1");
    assert!(
        replayed_adoption.is_err(),
        "a stream that already has rows is never adopted twice"
    );

    // Another node's rows are never visible to this stream.
    store
        .append_outbox_envelope(&envelope("msg-other", "inst-b", 1), "client.hello")
        .expect("other node append");
    let mut other_node = DeviceStore::open(&root).expect("third store handle");
    other_node
        .bind_outbox_stream("node_other", "inst-a")
        .expect("bind the other node");
    assert!(
        FrameOutbox::load(&mut other_node).expect("load").is_none(),
        "rows of a foreign node stay out of this stream"
    );
    other_node.close().expect("third store handle close");

    store.close().expect("restarted store close");
    fs::remove_dir_all(root).expect("database directory should be released");
}
