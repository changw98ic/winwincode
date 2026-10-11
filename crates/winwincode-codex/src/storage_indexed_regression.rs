// SPDX-License-Identifier: Apache-2.0
//! Migration and transaction fault cases use the actual private `SQLite` store.
use super::*;

fn root(label: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "wwc-indexed-{label}-{}-{stamp}",
        std::process::id()
    ))
}
fn frame(sequence: u64) -> ReplayFrame {
    ReplayFrame::new(
        format!("event-{sequence}"),
        sequence,
        format!("digest-{sequence}"),
        format!("body-{sequence}").into_bytes(),
    )
}
fn install_legacy(store: &AdapterStore, stream: &ReplayStreamKey, snapshot: &ReplaySnapshot) {
    let conn = store.lock().unwrap();
    // Construct an old-layout owned DB; the fixture never disables a real runtime guard.
    conn.execute_batch("DROP TRIGGER runtime_replay_block_legacy_insert;DROP TRIGGER runtime_replay_block_legacy_update;").unwrap();
    conn.execute(
        "INSERT INTO runtime_replay VALUES (?1,?2)",
        params![
            stream.as_str(),
            serde_json::to_vec(&StoredReplaySnapshot::from(snapshot)).unwrap()
        ],
    )
    .unwrap();
}

#[test]
fn indexed_runtime_migrates_legacy_snapshot_once_and_reopens_with_original_evidence() {
    let path = root("migration");
    let mut store = AdapterStore::open(&path).unwrap();
    let stream = ReplayStreamKey::new("legacy-runtime");
    let original = ReplaySnapshot {
        ack_sequence: 2,
        highest_sequence: 4,
        events: (1..=4).map(frame).collect(),
    };
    install_legacy(&store, &stream, &original);
    drop(store);
    store = AdapterStore::open(&path).unwrap();
    assert_eq!(
        ReplayStore::load(&mut store, &stream).unwrap().unwrap(),
        original
    );
    store.append(&stream, 4, &frame(5)).unwrap();
    store.record_acknowledgement(&stream, 2, 5).unwrap();
    let snapshot = ReplayStore::load(&mut store, &stream).unwrap().unwrap();
    assert_eq!(snapshot.events[..4], original.events);
    assert_eq!(snapshot.ack_sequence, 5);
    drop(store);
    let mut reopened = AdapterStore::open(&path).unwrap();
    assert_eq!(
        ReplayStore::load(&mut reopened, &stream).unwrap().unwrap(),
        snapshot
    );
    let conn = reopened.lock().unwrap();
    let legacy: i64 = conn
        .query_row(
            "SELECT count(*) FROM runtime_replay WHERE stream_key=?1",
            [stream.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(legacy, 0);
    assert!(
        conn.execute(
            "INSERT INTO runtime_replay VALUES ('old-process',?1)",
            [b"{}".as_slice()]
        )
        .is_err()
    );
    drop(conn);
    drop(reopened);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn indexed_runtime_rejects_corrupt_legacy_and_rolls_back_frame_and_watermark_together() {
    let path = root("rollback");
    let mut store = AdapterStore::open(&path).unwrap();
    let bad = ReplayStreamKey::new("bad-legacy");
    let corrupt = ReplaySnapshot {
        ack_sequence: 0,
        highest_sequence: 3,
        events: vec![frame(1), frame(3)],
    };
    install_legacy(&store, &bad, &corrupt);
    assert!(ReplayStore::load(&mut store, &bad).is_err());
    assert_eq!(
        store
            .lock()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM runtime_replay_head WHERE stream_key=?1",
                [bad.as_str()],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .lock()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM runtime_replay WHERE stream_key=?1",
                [bad.as_str()],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    let stream = ReplayStreamKey::new("fault-runtime");
    store.append(&stream, 0, &frame(1)).unwrap();
    store.lock().unwrap().execute_batch("CREATE TRIGGER fail_runtime_head BEFORE UPDATE OF highest_sequence ON runtime_replay_head BEGIN SELECT RAISE(ABORT,'owned fault window');END;").unwrap();
    assert!(store.append(&stream, 1, &frame(2)).is_err());
    let snapshot = ReplayStore::load(&mut store, &stream).unwrap().unwrap();
    assert_eq!(snapshot.highest_sequence, 1);
    assert_eq!(snapshot.events, vec![frame(1)]);
    store
        .lock()
        .unwrap()
        .execute_batch("DROP TRIGGER fail_runtime_head;")
        .unwrap();
    store.append(&stream, 1, &frame(2)).unwrap();
    assert!(store.append(&stream, 2, &frame(4)).is_err());
    assert!(store.record_acknowledgement(&stream, 0, 3).is_err());
    store.record_acknowledgement(&stream, 0, 2).unwrap();
    assert!(store.record_acknowledgement(&stream, 2, 1).is_err());
    store
        .lock()
        .unwrap()
        .execute(
            "DELETE FROM runtime_replay_frame WHERE stream_key=?1 AND sequence=2",
            [stream.as_str()],
        )
        .unwrap();
    assert!(store.load_watermarks(&stream).is_err());
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn indexed_post_action_migration_reopens_and_failed_migration_has_no_marker() {
    let path = root("post-action");
    let store = AdapterStore::open(&path).unwrap();
    let trace = serde_json::json!({"sourceKey":"source-1","sequence":1,"retained":false,"eventId":"xevt_00000000000000000000000001","source":"shell","operation":"execute","outcome":"succeeded","actions":["require_verification"],"occurredAt":"2026-10-10T00:00:00.000Z"});
    let record = serde_json::json!({"postActionTraces":[trace.clone()],"other":"original"});
    store.lock().unwrap().execute_batch("DROP TRIGGER post_action_block_legacy_insert;DROP TRIGGER post_action_block_legacy_update;").unwrap();
    store.save_run("legacy-run", &record).unwrap();
    store
        .lock()
        .unwrap()
        .execute(
            "DELETE FROM codex_store_migration WHERE migration_key='post-action-indexed-v1'",
            [],
        )
        .unwrap();
    drop(store);
    let reopened = AdapterStore::open(&path).unwrap();
    assert_eq!(
        reopened
            .load_run::<serde_json::Value>("legacy-run")
            .unwrap()
            .unwrap(),
        record
    );
    let mut retained = trace.clone();
    retained["retained"] = true.into();
    reopened
        .save_post_action_trace("legacy-run", &retained)
        .unwrap();
    let mut changed = retained.clone();
    changed["eventId"] = "other".into();
    assert!(
        reopened
            .save_post_action_trace("legacy-run", &changed)
            .is_err()
    );
    assert!(
        reopened
            .save_post_action_trace("legacy-run", &trace)
            .is_err()
    );
    drop(reopened);
    let reopened = AdapterStore::open(&path).unwrap();
    assert_eq!(
        reopened
            .load_run::<serde_json::Value>("legacy-run")
            .unwrap()
            .unwrap()["postActionTraces"],
        serde_json::json!([retained])
    );
    assert!(reopened.save_run("old-binary", &record).is_err());
    reopened.lock().unwrap().execute_batch("DROP TRIGGER post_action_block_legacy_insert;DROP TRIGGER post_action_block_legacy_update;").unwrap();
    reopened
        .save_run(
            "corrupt-run",
            &serde_json::json!({"postActionTraces":[{"sourceKey":"broken","retained":false}]}),
        )
        .unwrap();
    reopened
        .lock()
        .unwrap()
        .execute(
            "DELETE FROM codex_store_migration WHERE migration_key='post-action-indexed-v1'",
            [],
        )
        .unwrap();
    drop(reopened);
    assert!(AdapterStore::open(&path).is_err());
    let conn = Connection::open(path.join(DATABASE_FILE)).unwrap();
    assert_eq!(conn.query_row("SELECT count(*) FROM codex_store_migration WHERE migration_key='post-action-indexed-v1'",[],|row|row.get::<_,i64>(0)).unwrap(),0);
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM post_action_trace WHERE run_key='corrupt-run'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    drop(conn);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn indexed_runtime_empty_reads_do_not_create_heads_and_legacy_compaction_is_preserved() {
    let path = root("read-boundary");
    let mut store = AdapterStore::open(&path).unwrap();
    let missing = ReplayStreamKey::new("missing-runtime");
    assert!(ReplayStore::load(&mut store, &missing).unwrap().is_none());
    assert_eq!(
        store.load_watermarks(&missing).unwrap(),
        ReplaySnapshot::default()
    );
    assert_eq!(
        store
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM runtime_replay_head", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let compacted = ReplayStreamKey::new("legacy-compacted");
    let original = ReplaySnapshot {
        ack_sequence: 4,
        highest_sequence: 4,
        events: Vec::new(),
    };
    install_legacy(&store, &compacted, &original);
    assert_eq!(
        ReplayStore::load(&mut store, &compacted).unwrap().unwrap(),
        original
    );
    store.append(&compacted, 4, &frame(5)).unwrap();
    let snapshot = ReplayStore::load(&mut store, &compacted).unwrap().unwrap();
    assert_eq!(snapshot.events, vec![frame(5)]);
    store.record_acknowledgement(&compacted, 4, 5).unwrap();
    store
        .lock()
        .unwrap()
        .execute(
            "DELETE FROM runtime_replay_frame WHERE stream_key=?1",
            [compacted.as_str()],
        )
        .unwrap();
    assert!(store.load_watermarks(&compacted).is_err());
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

fn valid_post_action_trace() -> serde_json::Value {
    serde_json::json!({"sourceKey":"source-1","sequence":1,"retained":false,"eventId":"xevt_00000000000000000000000001","source":"shell","operation":"execute","outcome":"succeeded","actions":["require_verification"],"occurredAt":"2026-10-10T00:00:00.000Z"})
}

#[test]
fn indexed_post_action_rejects_duplicate_legacy_sources_or_sequences_with_atomic_rollback() {
    for case in ["source", "sequence"] {
        let path = root(case);
        let store = AdapterStore::open(&path).unwrap();
        let trace = valid_post_action_trace();
        let mut second = trace.clone();
        if case == "source" {
            second["sequence"] = 2.into();
        } else {
            second["sourceKey"] = "source-2".into();
        }
        second["eventId"] = "xevt_00000000000000000000000002".into();
        let legacy = serde_json::json!({"postActionTraces":[trace,second],"other":"original"});
        store.lock().unwrap().execute_batch("DROP TRIGGER post_action_block_legacy_insert; DROP TRIGGER post_action_block_legacy_update;").unwrap();
        store.save_run("old-run", &legacy).unwrap();
        store
            .lock()
            .unwrap()
            .execute(
                "DELETE FROM codex_store_migration WHERE migration_key='post-action-indexed-v1'",
                [],
            )
            .unwrap();
        drop(store);
        assert!(AdapterStore::open(&path).is_err());
        let conn = Connection::open(path.join(DATABASE_FILE)).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM post_action_trace", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(conn.query_row("SELECT count(*) FROM codex_store_migration WHERE migration_key='post-action-indexed-v1'", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
        let bytes: Vec<u8> = conn
            .query_row(
                "SELECT record_json FROM codex_run WHERE run_key='old-run'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            legacy
        );
        drop(conn);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[test]
fn indexed_post_action_full_read_rejects_column_and_record_identity_drift() {
    let path = root("trace-identity");
    let store = AdapterStore::open(&path).unwrap();
    store
        .save_run("owned-run", &serde_json::json!({"other":"original"}))
        .unwrap();
    let trace = valid_post_action_trace();
    store.save_post_action_trace("owned-run", &trace).unwrap();
    store
        .lock()
        .unwrap()
        .execute(
            "UPDATE post_action_trace SET sequence=2 WHERE run_key='owned-run'",
            [],
        )
        .unwrap();
    assert!(store.load_run::<serde_json::Value>("owned-run").is_err());
    store
        .lock()
        .unwrap()
        .execute(
            "UPDATE post_action_trace SET sequence=1 WHERE run_key='owned-run'",
            [],
        )
        .unwrap();
    let mut changed = trace.clone();
    changed["sourceKey"] = "other".into();
    store
        .lock()
        .unwrap()
        .execute(
            "UPDATE post_action_trace SET record_json=?1 WHERE run_key='owned-run'",
            [serde_json::to_vec(&changed).unwrap()],
        )
        .unwrap();
    assert!(store.load_run::<serde_json::Value>("owned-run").is_err());
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn indexed_runtime_exact_conflict_query_has_constant_sqlite_work() {
    use crate::storage_mechanism_regression as measure;
    let mut samples = Vec::new();
    for size in [1_000_u64, 10_000] {
        let path = root("exact-runtime-query");
        let mut store = AdapterStore::open(&path).unwrap();
        let stream = ReplayStreamKey::new("exact-runtime-query");
        for sequence in 1..=size {
            store
                .append(&stream, sequence - 1, &frame(sequence))
                .unwrap();
        }
        measure::reset();
        let snapshot = store
            .load_indexed(
                &stream,
                ReplayReadQuery::Conflict {
                    event_id: "event-1",
                    sequence: size,
                },
            )
            .unwrap()
            .unwrap();
        let metrics = measure::finish();
        assert_eq!(snapshot.events.len(), 2);
        assert_eq!(snapshot.events[0].sequence, 1);
        assert_eq!(snapshot.events[1].sequence, size);
        assert!(
            metrics.replay_vm_steps <= 100,
            "exact index seeks must not scan the stream"
        );
        samples.push(metrics.replay_vm_steps);
        let exact = store
            .load_indexed(
                &stream,
                ReplayReadQuery::Conflict {
                    event_id: "event-1",
                    sequence: 1,
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            exact.events.len(),
            1,
            "one row matching both keys is returned once"
        );
        eprintln!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({"id":"runtime-exact-query","size":size,"metrics":metrics})
        );
        drop(store);
        std::fs::remove_dir_all(path).unwrap();
    }
    assert!(samples[1] <= samples[0] + 10);
}

#[test]
fn indexed_post_action_migration_marker_and_legacy_guards_commit_together() {
    let path = root("post-action-guards");
    let store = AdapterStore::open(&path).unwrap();
    let record = serde_json::json!({"postActionTraces": [valid_post_action_trace()]});
    store.lock().unwrap().execute_batch("DROP TRIGGER post_action_block_legacy_insert; DROP TRIGGER post_action_block_legacy_update; DELETE FROM codex_store_migration WHERE migration_key='post-action-indexed-v1';").unwrap();
    store.save_run("legacy-run", &record).unwrap();
    store.lock().unwrap().execute_batch("CREATE TRIGGER fail_post_action_marker BEFORE INSERT ON codex_store_migration WHEN NEW.migration_key='post-action-indexed-v1' BEGIN SELECT RAISE(ABORT,'owned post-action migration crash window'); END;").unwrap();
    drop(store);
    assert!(AdapterStore::open(&path).is_err());
    let conn = Connection::open(path.join(DATABASE_FILE)).unwrap();
    assert_eq!(conn.query_row("SELECT count(*) FROM codex_store_migration WHERE migration_key='post-action-indexed-v1'", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
    assert_eq!(conn.query_row("SELECT count(*) FROM sqlite_master WHERE type='trigger' AND name LIKE 'post_action_block_legacy_%'", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM post_action_trace WHERE run_key='legacy-run'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    conn.execute_batch("DROP TRIGGER fail_post_action_marker;")
        .unwrap();
    drop(conn);
    let reopened = AdapterStore::open(&path).unwrap();
    assert_eq!(
        reopened
            .load_run::<serde_json::Value>("legacy-run")
            .unwrap()
            .unwrap(),
        record
    );
    assert!(reopened.save_run("old-writer", &record).is_err());
    reopened.lock().unwrap().execute_batch("DROP TRIGGER post_action_block_legacy_insert; DROP TRIGGER post_action_block_legacy_update;").unwrap();
    drop(reopened);
    let repaired = AdapterStore::open(&path).unwrap();
    assert!(
        repaired.save_run("old-writer", &record).is_err(),
        "an existing marker also installs missing guards"
    );
    drop(repaired);
    std::fs::remove_dir_all(path).unwrap();
}
