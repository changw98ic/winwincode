// SPDX-License-Identifier: Apache-2.0
use crate::runtime_event_transaction::{
    LEDGER_READ_INTERLEAVE, hydrate_runtime_ledger, load_ledger, runtime_stream_id_for_projection,
};

fn indexed_runtime_fixture(seed: u64) -> (Fixture, RuntimeEventMessage, String) {
    let mut fixture = Fixture::open("indexed-runtime-faults", seed);
    let (_, job, lease, _) = install_workrun_dispatch_for_terminal(&mut fixture, seed);
    let binding = workrun_binding(&job, &lease, seed, seed);
    fixture
        .accept(
            &ExecutionPortMessage::SessionBindingMessage(binding.clone()),
            binding.sent_at.clone(),
        )
        .unwrap();
    let message = RuntimeEventMessage {
        codex_thread_id: binding.codex_thread_id,
        event: ExecutionEventRecord {
            category: ExecutionEventCategory::Activity,
            event_id: ExecutionEventId(canonical_id("xevt", seed + 1_000)),
            occurred_at: Instant("2027-01-15T08:00:03.000Z".into()),
            payload: None,
            sequence: ExecutionSequence(1),
            summary: "owned indexed runtime fixture".into(),
        },
        kind: RuntimeEventMessageKind::RuntimeEvent,
        lease,
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 1_000)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:04.000Z".into()),
        session_identity: binding.session_identity,
        worker_session_id: binding.worker_session_id,
    };
    fixture
        .accept(
            &ExecutionPortMessage::RuntimeEventMessage(message.clone()),
            message.sent_at.clone(),
        )
        .unwrap();
    let scope_key = crate::repository_scope_key(&fixture.scope).unwrap();
    let stream = runtime_stream_id_for_projection(&scope_key, &message.lease.job_id);
    (fixture, message, stream)
}
fn assert_indexed_accepted(fixture: &mut Fixture, message: &RuntimeEventMessage) {
    let result = fixture
        .accept(
            &ExecutionPortMessage::RuntimeEventMessage(message.clone()),
            message.sent_at.clone(),
        )
        .unwrap();
    let [ExecutionPortMessage::RuntimeAckMessage(ack)] = result.as_slice() else {
        panic!("runtime ack")
    };
    assert_eq!(ack.status, LeaseWriteStatus::Accepted);
}

fn assert_indexed_runtime_interleaved_commit(advance_during_resolution: bool) {
    let seed = if advance_during_resolution {
        8_610_000
    } else {
        8_600_000
    };
    let (mut fixture, mut message, _) = indexed_runtime_fixture(seed);
    message.event.sequence = ExecutionSequence(2);
    message.event.event_id = ExecutionEventId(canonical_id("xevt", seed + 2_000));
    message.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 2_000));
    let mut winner = message.clone();
    winner.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 2_001));
    winner.event.summary = "concurrent committed runtime event".into();
    if advance_during_resolution {
        winner.event.event_id = ExecutionEventId(canonical_id("xevt", seed + 2_001));
    }
    let mut later = message.clone();
    later.event.sequence = ExecutionSequence(3);
    later.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 2_002));
    let mut writer = Fixture {
        root: fixture.root.clone(),
        control_plane: ControlPlane::start_local(
            ControlPlaneConfig::local(&fixture.root),
            Box::new(RecordingPublisher),
        )
        .unwrap(),
        storage: SqliteStorage::open(&fixture.root).unwrap(),
        scope: fixture.scope.clone(),
    };
    LEDGER_READ_INTERLEAVE.with_borrow_mut(|hook| {
        *hook = Some(Box::new(move || {
            assert_indexed_accepted(&mut writer, &winner);
            if advance_during_resolution {
                LEDGER_READ_INTERLEAVE.with_borrow_mut(|hook| {
                    *hook = Some(Box::new(move || {
                        assert_indexed_accepted(&mut writer, &later);
                        Box::new(writer.storage).close().unwrap();
                        writer.control_plane.shutdown().unwrap();
                    }));
                });
            } else {
                Box::new(writer.storage).close().unwrap();
                writer.control_plane.shutdown().unwrap();
            }
        }));
    });
    let response = fixture
        .accept(
            &ExecutionPortMessage::RuntimeEventMessage(message.clone()),
            message.sent_at.clone(),
        )
        .expect("concurrent append must return an ACK, never a storage error");
    let [ExecutionPortMessage::RuntimeAckMessage(ack)] = response.as_slice() else {
        panic!("runtime ack")
    };
    assert_eq!(ack.status, LeaseWriteStatus::RejectedConflict);
    assert_eq!(ack.ack_sequence.0, 2);
    assert_eq!(
        ack.error.as_ref().unwrap().code,
        winwincode_execution_port::generated::ExecutionPortErrorCode::MessageConflict
    );
    assert!(LEDGER_READ_INTERLEAVE.with_borrow(Option::is_none));
    fixture.close();
}

#[test]
fn indexed_runtime_commit_after_header_read_returns_conflict_ack() {
    assert_indexed_runtime_interleaved_commit(false);
}

#[test]
fn indexed_runtime_commit_during_race_resolution_returns_conflict_ack() {
    assert_indexed_runtime_interleaved_commit(true);
}

#[test]
fn indexed_runtime_v1_history_migrates_atomically_and_reopens_without_rewriting_evidence() {
    for size in [1_u64, 8, 100, 200, 400] {
        let (mut fixture, mut message, stream) = indexed_runtime_fixture(8_200_000);
        for sequence in 2..=size {
            message.event.sequence = ExecutionSequence(i64::try_from(sequence).unwrap());
            message.event.event_id = ExecutionEventId(canonical_id("xevt", 8_201_000 + sequence));
            message.message_id = ExecutionMessageId(canonical_id("xmsg", 8_201_000 + sequence));
            assert_indexed_accepted(&mut fixture, &message);
        }
        let mut legacy = load_ledger(&fixture.storage, &stream).unwrap().unwrap();
        hydrate_runtime_ledger(&fixture.storage, &mut legacy).unwrap();
        legacy.schema_version = 1;
        legacy.event_chain_digest.clear();
        let conn = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        tx.execute(
            "UPDATE product_state SET payload=?2 WHERE stream_id=?1",
            rusqlite::params![&stream, serde_json::to_vec(&legacy).unwrap()],
        )
        .unwrap();
        for prefix in [
            format!("runtime-ledger-record:{}:", message.lease.job_id.0),
            format!("runtime-ledger-identity:{}:", message.lease.job_id.0),
        ] {
            tx.execute(
                "DELETE FROM product_state WHERE substr(stream_id,1,length(?1))=?1",
                [prefix],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        assert_eq!(
            load_ledger(&fixture.storage, &stream)
                .unwrap()
                .unwrap()
                .schema_version,
            1
        );
        message.event.sequence = ExecutionSequence(i64::try_from(size + 1).unwrap());
        message.event.event_id = ExecutionEventId(canonical_id("xevt", 8_201_001 + size));
        message.message_id = ExecutionMessageId(canonical_id("xmsg", 8_201_001 + size));
        assert_indexed_accepted(&mut fixture, &message);
        let mut migrated = load_ledger(&fixture.storage, &stream).unwrap().unwrap();
        assert_eq!(migrated.schema_version, 2);
        assert!(migrated.events.is_empty());
        hydrate_runtime_ledger(&fixture.storage, &mut migrated).unwrap();
        assert_eq!(
            &migrated.events[..usize::try_from(size).unwrap()],
            legacy.events.as_slice()
        );
        assert_eq!(migrated.events.len(), usize::try_from(size + 1).unwrap());
        let reopened = SqliteStorage::open(&fixture.root).unwrap();
        let mut again = load_ledger(&reopened, &stream).unwrap().unwrap();
        hydrate_runtime_ledger(&reopened, &mut again).unwrap();
        assert_eq!(again, migrated);
        Box::new(reopened).close().unwrap();
        drop(conn);
        fixture.close();
    }
}

#[test]
fn indexed_runtime_large_legacy_import_rolls_back_and_rejects_corrupt_source() {
    let (mut fixture, mut message, stream) = indexed_runtime_fixture(8_250_000);
    for sequence in 2..=100_u64 {
        message.event.sequence = ExecutionSequence(i64::try_from(sequence).unwrap());
        message.event.event_id = ExecutionEventId(canonical_id("xevt", 8_251_000 + sequence));
        message.message_id = ExecutionMessageId(canonical_id("xmsg", 8_251_000 + sequence));
        assert_indexed_accepted(&mut fixture, &message);
    }
    let mut legacy = load_ledger(&fixture.storage, &stream).unwrap().unwrap();
    hydrate_runtime_ledger(&fixture.storage, &mut legacy).unwrap();
    legacy.schema_version = 1;
    legacy.event_chain_digest.clear();
    let original = serde_json::to_vec(&legacy).unwrap();
    let conn = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
    let tx = conn.unchecked_transaction().unwrap();
    tx.execute(
        "UPDATE product_state SET payload=?2 WHERE stream_id=?1",
        rusqlite::params![&stream, &original],
    )
    .unwrap();
    for prefix in [
        format!("runtime-ledger-record:{}:", message.lease.job_id.0),
        format!("runtime-ledger-identity:{}:", message.lease.job_id.0),
    ] {
        tx.execute(
            "DELETE FROM product_state WHERE substr(stream_id,1,length(?1))=?1",
            [prefix],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    message.event.sequence = ExecutionSequence(101);
    message.event.event_id = ExecutionEventId(canonical_id("xevt", 8_251_101));
    message.message_id = ExecutionMessageId(canonical_id("xmsg", 8_251_101));
    let mut corrupt = legacy.clone();
    corrupt.events[50].event_digest = Sha256Digest(format!("sha256:{}", "a".repeat(64)));
    conn.execute(
        "UPDATE product_state SET payload=?2 WHERE stream_id=?1",
        rusqlite::params![&stream, serde_json::to_vec(&corrupt).unwrap()],
    )
    .unwrap();
    assert!(
        fixture
            .accept(
                &ExecutionPortMessage::RuntimeEventMessage(message.clone()),
                message.sent_at.clone()
            )
            .is_err()
    );
    conn.execute(
        "UPDATE product_state SET payload=?2 WHERE stream_id=?1",
        rusqlite::params![&stream, &original],
    )
    .unwrap();
    let failure_key = format!(
        "runtime-ledger-record:{}:{:020}",
        message.lease.job_id.0, 50
    );
    let trigger = format!(
        "CREATE TRIGGER fail_large_legacy_import BEFORE INSERT ON product_state WHEN NEW.stream_id='{failure_key}' BEGIN SELECT RAISE(ABORT,'owned legacy import crash window'); END;"
    );
    conn.execute_batch(&trigger).unwrap();
    assert!(
        fixture
            .accept(
                &ExecutionPortMessage::RuntimeEventMessage(message.clone()),
                message.sent_at.clone()
            )
            .is_err()
    );
    let unchanged = fixture.storage.load_state(&stream).unwrap().unwrap();
    assert_eq!(unchanged.payload, original);
    let prefix = format!("runtime-ledger-record:{}:", message.lease.job_id.0);
    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM product_state WHERE substr(stream_id,1,length(?1))=?1",
            [prefix],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 0,
        "partial import rows roll back with the legacy header"
    );
    conn.execute_batch("DROP TRIGGER fail_large_legacy_import;")
        .unwrap();
    assert_indexed_accepted(&mut fixture, &message);
    let mut migrated = load_ledger(&fixture.storage, &stream).unwrap().unwrap();
    hydrate_runtime_ledger(&fixture.storage, &mut migrated).unwrap();
    assert_eq!(migrated.events.len(), 101);
    assert_eq!(&migrated.events[..100], legacy.events.as_slice());
    drop(conn);
    fixture.close();
}

#[test]
fn indexed_runtime_state_receipt_and_evidence_rollback_as_one_transaction() {
    let (mut fixture, mut message, stream) = indexed_runtime_fixture(8_300_000);
    let conn = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
    message.event.sequence = ExecutionSequence(2);
    message.event.event_id = ExecutionEventId(canonical_id("xevt", 8_301_002));
    message.message_id = ExecutionMessageId(canonical_id("xmsg", 8_301_002));
    conn.execute_batch("CREATE TRIGGER fail_runtime_record BEFORE INSERT ON product_state WHEN NEW.stream_id LIKE 'runtime-ledger-identity:%' BEGIN SELECT RAISE(ABORT,'owned receipt crash window');END;").unwrap();
    assert!(
        fixture
            .accept(
                &ExecutionPortMessage::RuntimeEventMessage(message.clone()),
                message.sent_at.clone()
            )
            .is_err()
    );
    let mut header = load_ledger(&fixture.storage, &stream).unwrap().unwrap();
    assert_eq!(header.highest_sequence, 1);
    hydrate_runtime_ledger(&fixture.storage, &mut header).unwrap();
    assert_eq!(header.events.len(), 1);
    assert!(
        fixture
            .storage
            .load_state(&format!(
                "runtime-ledger-record:{}:{:020}",
                message.lease.job_id.0, 2
            ))
            .unwrap()
            .is_none()
    );
    conn.execute_batch("DROP TRIGGER fail_runtime_record;")
        .unwrap();
    assert_indexed_accepted(&mut fixture, &message);
    drop(conn);
    fixture.close();
}

#[test]
fn indexed_runtime_rejects_corrupt_marker_tail_and_chain_with_receipt_first_replay() {
    let (mut fixture, message, stream) = indexed_runtime_fixture(8_400_000);
    let conn = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
    let marker = format!(
        "runtime-ledger-identity:{}:{}",
        message.lease.job_id.0, message.event.event_id.0
    );
    let original = fixture
        .storage
        .load_state(&marker)
        .unwrap()
        .unwrap()
        .payload;
    let mut value: Value = serde_json::from_slice(&original).unwrap();
    value[0] = 2.into();
    conn.execute(
        "UPDATE product_state SET payload=?2 WHERE stream_id=?1",
        rusqlite::params![&marker, serde_json::to_vec(&value).unwrap()],
    )
    .unwrap();
    let mut retry = message.clone();
    retry.message_id = ExecutionMessageId(canonical_id("xmsg", 8_499_999));
    assert!(
        fixture
            .accept(
                &ExecutionPortMessage::RuntimeEventMessage(retry.clone()),
                retry.sent_at.clone()
            )
            .is_err()
    );
    let response = fixture
        .accept(
            &ExecutionPortMessage::RuntimeEventMessage(message.clone()),
            message.sent_at.clone(),
        )
        .unwrap();
    let [ExecutionPortMessage::RuntimeAckMessage(ack)] = response.as_slice() else {
        panic!("receipt")
    };
    assert_eq!(ack.status, LeaseWriteStatus::Duplicate);
    conn.execute(
        "UPDATE product_state SET payload=?2 WHERE stream_id=?1",
        rusqlite::params![&marker, original],
    )
    .unwrap();
    let key = format!("runtime-ledger-record:{}:{:020}", message.lease.job_id.0, 1);
    let original = fixture.storage.load_state(&key).unwrap().unwrap().payload;
    conn.execute("DELETE FROM product_state WHERE stream_id=?1", [&key])
        .unwrap();
    assert!(load_ledger(&fixture.storage, &stream).is_err());
    conn.execute(
        "INSERT INTO product_state VALUES (?1,1,?2)",
        rusqlite::params![&key, original],
    )
    .unwrap();
    let mut header = fixture.storage.load_state(&stream).unwrap().unwrap();
    let mut value: Value = serde_json::from_slice(&header.payload).unwrap();
    value["eventChainDigest"] = "malformed".into();
    conn.execute(
        "UPDATE product_state SET payload=?2 WHERE stream_id=?1",
        rusqlite::params![&stream, serde_json::to_vec(&value).unwrap()],
    )
    .unwrap();
    assert!(load_ledger(&fixture.storage, &stream).is_err());
    conn.execute(
        "UPDATE product_state SET payload=?2 WHERE stream_id=?1",
        rusqlite::params![&stream, &header.payload],
    )
    .unwrap();
    value = serde_json::from_slice(&header.payload).unwrap();
    value["eventChainDigest"] = format!("sha256:{}", "a".repeat(64)).into();
    header.payload = serde_json::to_vec(&value).unwrap();
    conn.execute(
        "UPDATE product_state SET payload=?2 WHERE stream_id=?1",
        rusqlite::params![&stream, header.payload],
    )
    .unwrap();
    assert!(load_ledger(&fixture.storage, &stream).is_err());
    drop(conn);
    fixture.close();
}

#[test]
fn indexed_runtime_missing_identity_marker_is_corruption_for_exact_sequence_and_full_audit() {
    let (mut fixture, message, stream) = indexed_runtime_fixture(8_500_000);
    let conn = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
    let marker = format!(
        "runtime-ledger-identity:{}:{}",
        message.lease.job_id.0, message.event.event_id.0
    );
    let original = fixture
        .storage
        .load_state(&marker)
        .unwrap()
        .unwrap()
        .payload;
    let mut header = load_ledger(&fixture.storage, &stream).unwrap().unwrap();
    conn.execute("DELETE FROM product_state WHERE stream_id=?1", [&marker])
        .unwrap();
    let mut retry = message.clone();
    retry.message_id = ExecutionMessageId(canonical_id("xmsg", 8_599_999));
    assert!(
        fixture
            .accept(
                &ExecutionPortMessage::RuntimeEventMessage(retry.clone()),
                retry.sent_at.clone()
            )
            .is_err()
    );
    assert!(load_ledger(&fixture.storage, &stream).is_err());
    assert!(hydrate_runtime_ledger(&fixture.storage, &mut header).is_err());
    conn.execute(
        "INSERT INTO product_state VALUES (?1,1,?2)",
        rusqlite::params![&marker, original],
    )
    .unwrap();
    let response = fixture
        .accept(
            &ExecutionPortMessage::RuntimeEventMessage(retry.clone()),
            retry.sent_at,
        )
        .unwrap();
    let [ExecutionPortMessage::RuntimeAckMessage(ack)] = response.as_slice() else {
        panic!("exact indexed replay")
    };
    assert_eq!(ack.status, LeaseWriteStatus::Duplicate);
    hydrate_runtime_ledger(&fixture.storage, &mut header).unwrap();
    drop(conn);
    fixture.close();
}
