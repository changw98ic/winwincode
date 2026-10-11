// SPDX-License-Identifier: Apache-2.0

#[test]
#[ignore = "heavy mechanism regression: bounded indexed atomic renewal cost"]
fn m10_real_atomic_renewal_keeps_bounded_indexed_cost() {
    use crate::storage_mechanism_regression as measure;
    let mut totals = Vec::new();
    let mut capacity_work = Vec::new();
    for size in [100_u64, 200, 400] {
        let root = temporary_directory("authority-history-mechanism");
        let mut storage = SqliteStorage::open(&root).unwrap();
        let mut authority = prepare_authority(&mut storage, 1);
        let config = WorkerOutboundQueueConfig::default();
        let mut final_pair = None;
        measure::reset();
        for sequence in 1..=size {
            let expires = Instant(format!("2027-02-15T08:00:20.{sequence:03}Z"));
            let mut extended = authority.clone();
            extended.lease_expires_at = expires.clone();
            let frame = request(&extended, 1_000_000 + sequence, b"synthetic-renewal-frame");
            let slot = &authority.slot;
            let renewal = ExecutionLeaseRenewal {
                expires_at: expires,
                prior_expires_at: authority.lease_expires_at.clone(),
                fencing_token: slot.fencing_token.clone(),
                job_id: slot.job_id.clone(),
                lease_id: slot.lease_id.clone(),
                message_id: frame.message_id().clone(),
                request_id: RequestId(id("req", 1_000_000 + sequence)),
                sent_at: at(6),
                worker_id: slot.worker_id.clone(),
                worker_instance_id: slot.worker_instance_id.clone(),
                attempt: slot.attempt,
            };
            let receipt = storage
                .worker_outbound_queue(config)
                .unwrap()
                .renew_lease_and_enqueue(&renewal, &frame)
                .unwrap();
            assert_eq!(receipt.status, LeaseWriteStatus::Accepted);
            assert_eq!(receipt.lease.unwrap().expires_at, extended.lease_expires_at);
            authority = extended;
            final_pair = Some((renewal, frame));
        }
        let metrics = measure::finish();
        assert!(metrics.lease_receipt_rows <= 4 * size);
        assert!(metrics.capacity_vm_steps <= 256 * size);
        assert!(metrics.capacity_mutation_vm_steps <= 512 * size);
        let (renewal, frame) = final_pair.unwrap();
        measure::reset();
        assert_eq!(
            storage
                .worker_outbound_queue(config)
                .unwrap()
                .renew_lease_and_enqueue(&renewal, &frame)
                .unwrap()
                .status,
            LeaseWriteStatus::Duplicate
        );
        let replay = measure::finish();
        assert_eq!(replay.lease_receipt_rows, 0);
        assert_eq!(replay.lease_receipt_bytes, 0);
        assert_eq!(replay.receipt_vm_steps, 0);
        assert_eq!(replay.audit_vm_steps, 0);
        assert_eq!(replay.capacity_vm_steps, 0);
        assert_eq!(replay.capacity_mutation_vm_steps, 0);
        let connection = Connection::open(storage.database_path()).unwrap();
        let receipts: i64 = connection.query_row("SELECT COUNT(*) FROM execution_lease_request_receipts WHERE job_id=?1 AND operation IN ('claim','renew')", [&authority.slot.job_id.0], |row| row.get(0)).unwrap();
        assert_eq!(receipts, i64::try_from(size + 1).unwrap());
        let plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT response_json FROM execution_lease_request_receipts
             WHERE job_id = ?1 AND operation IN ('claim', 'renew')
               AND json_extract(response_json, '$.status') = 'accepted'
               AND json_extract(response_json, '$.lease.expires_at') = ?2",
                [&authority.slot.job_id.0, &authority.lease_expires_at.0],
                |row| row.get(3),
            )
            .unwrap();
        assert!(plan.contains("SEARCH") && plan.contains("execution_lease_accepted_period"));
        println!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({"id":"M10", "size":size, "metrics":metrics, "replay":replay, "retained_accepted_receipts":receipts, "plan":plan, "classification":"bounded indexed atomic renewal cost"})
        );
        totals.push(metrics.lease_receipt_bytes);
        capacity_work.push(metrics.capacity_vm_steps + metrics.capacity_mutation_vm_steps);
        drop(connection);
        Box::new(storage).close().unwrap();
        fs::remove_dir_all(root).unwrap();
    }
    assert!(totals[1] < 3 * totals[0]);
    assert!(totals[2] < 3 * totals[1]);
    assert!(capacity_work[1] < 3 * capacity_work[0]);
    assert!(capacity_work[2] < 3 * capacity_work[1]);
}

fn prepare_indexed_capacity_backlog(
    storage: &mut SqliteStorage,
    config: WorkerOutboundQueueConfig,
    size: u64,
) -> Vec<WorkerOutboundAuthority> {
    let mut authorities = Vec::new();
    for seed in 1..=(size / 1_000) {
        authorities.push(prepare_authority(storage, seed));
    }
    // All authorities and rows are admitted by the canonical real methods;
    // each authority stays below the unchanged default limit of 1,024.
    for sequence in 0..size {
        let authority = &authorities[usize::try_from(sequence / 1_000).unwrap()];
        let frame = request(authority, 2_000_000 + sequence, b"offline-frame");
        storage
            .worker_outbound_queue(config)
            .unwrap()
            .enqueue(&frame)
            .unwrap();
    }
    authorities
}

fn retained_capacity_message_totals(connection: &Connection) -> (i64, i64) {
    connection
        .query_row(
            "SELECT COUNT(*), SUM(length(payload)) FROM internal_worker_outbound_messages",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

fn retained_capacity_frame_rows(connection: &Connection) -> Vec<(String, String, String, Vec<u8>)> {
    connection
        .prepare("SELECT message_id, authority_digest, payload_digest, payload FROM internal_worker_outbound_messages ORDER BY sequence")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn prepare_failed_capacity_increment(
    storage: &mut SqliteStorage,
    authority: &WorkerOutboundAuthority,
    first: &WorkerOutboundEnqueueRequest,
) -> (
    Connection,
    ExecutionLeaseRenewal,
    WorkerOutboundEnqueueRequest,
) {
    let connection = Connection::open(storage.database_path()).unwrap();
    storage.worker_outbound_queue(config()).unwrap();
    connection.execute_batch(
        "CREATE TRIGGER fail_capacity_insert BEFORE UPDATE ON internal_worker_outbound_capacity_head
         WHEN NEW.pending_messages > OLD.pending_messages
         BEGIN SELECT RAISE(FAIL, 'injected capacity increment failure'); END;"
    ).unwrap();
    let (renewal, frame) = renewal_frame(authority);
    assert!(
        storage
            .worker_outbound_queue(config())
            .unwrap()
            .renew_lease_and_enqueue(&renewal, &frame)
            .is_err()
    );
    assert_eq!(capacity_totals(&connection), (0, 0, 0));
    let lease = storage
        .execution_registry()
        .unwrap()
        .load_lease(&renewal.job_id)
        .unwrap()
        .unwrap();
    assert_eq!(lease.expires_at, authority.lease_expires_at);
    let receipts: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM execution_lease_request_receipts WHERE job_id = ?1",
            [&authority.slot.job_id.0],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(receipts, 1);
    connection
        .execute_batch("DROP TRIGGER fail_capacity_insert;")
        .unwrap();
    storage
        .worker_outbound_queue(config())
        .unwrap()
        .enqueue(first)
        .unwrap();
    storage
        .worker_outbound_queue(config())
        .unwrap()
        .renew_lease_and_enqueue(&renewal, &frame)
        .unwrap();
    (connection, renewal, frame)
}

fn enqueue_raw_capacity_fixture_pair(
    storage: &mut SqliteStorage,
    first: &WorkerOutboundEnqueueRequest,
    second: &WorkerOutboundEnqueueRequest,
) {
    storage
        .worker_outbound_queue(config())
        .unwrap()
        .enqueue(first)
        .unwrap();
    storage
        .worker_outbound_queue(config())
        .unwrap()
        .enqueue(second)
        .unwrap();
}

#[test]
#[ignore = "heavy mechanism regression: fixed indexed capacity cost over 10k retained rows"]
fn storage_c01_real_enqueue_keeps_fixed_indexed_cost_across_authority_backlog() {
    use crate::storage_mechanism_regression as measure;
    let mut steps = Vec::new();
    let mut mutation_steps = Vec::new();
    for size in [1_000_u64, 10_000] {
        let root = temporary_directory("global-capacity-mechanism");
        let mut storage = SqliteStorage::open(&root).unwrap();
        let config = WorkerOutboundQueueConfig::default();
        let authorities = prepare_indexed_capacity_backlog(&mut storage, config, size);
        let frame = request(
            &authorities[0],
            3_000_000 + size,
            b"offline-capacity-target",
        );
        measure::reset();
        let receipt = storage
            .worker_outbound_queue(config)
            .unwrap()
            .enqueue(&frame)
            .unwrap();
        let metrics = measure::finish();
        assert!(!receipt.replayed);
        assert!(metrics.capacity_vm_steps <= 64);
        assert!(metrics.capacity_mutation_vm_steps <= 512);
        let connection = Connection::open(storage.database_path()).unwrap();
        let (rows, retained_bytes) = retained_capacity_message_totals(&connection);
        assert_eq!(rows, i64::try_from(size + 1).unwrap());
        assert_eq!(capacity_totals(&connection), (rows, retained_bytes, rows));
        let retained_bytes = usize::try_from(retained_bytes).unwrap();
        let low_cap = WorkerOutboundQueueConfig {
            max_frame_bytes: 256,
            max_retained_bytes: retained_bytes,
            ..config
        };
        let second = request(&authorities[0], 4_000_000 + size, b"next-capacity-frame");
        measure::reset();
        assert_eq!(
            storage
                .worker_outbound_queue(low_cap)
                .unwrap()
                .enqueue(&second)
                .unwrap_err()
                .code(),
            WorkerOutboundQueueErrorCode::CapacityExceeded
        );
        let rejection = measure::finish();
        assert!(rejection.capacity_vm_steps <= 64);
        assert_eq!(rejection.capacity_mutation_vm_steps, 0);
        measure::reset();
        assert!(
            storage
                .worker_outbound_queue(low_cap)
                .unwrap()
                .enqueue(&frame)
                .unwrap()
                .replayed
        );
        let replay = measure::finish();
        assert_eq!(replay.lease_receipt_rows, 0);
        assert_eq!(replay.lease_receipt_bytes, 0);
        assert_eq!(replay.receipt_vm_steps, 0);
        assert_eq!(replay.audit_vm_steps, 0);
        assert_eq!(replay.capacity_vm_steps, 0);
        assert_eq!(replay.capacity_mutation_vm_steps, 0);
        assert_eq!(
            capacity_totals(&connection),
            (rows, i64::try_from(retained_bytes).unwrap(), rows)
        );
        let retained_totals = retained_capacity_message_totals(&connection);
        assert_eq!(
            retained_totals,
            (rows, i64::try_from(retained_bytes).unwrap())
        );
        let rejected_rows: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM internal_worker_outbound_messages WHERE message_id = ?1",
                [&second.message_id().0],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rejected_rows, 0);
        let retained: (String, Vec<u8>) = connection.query_row(
            "SELECT payload_digest, payload FROM internal_worker_outbound_messages WHERE message_id = ?1",
            [&frame.message_id().0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(retained.0, frame.payload_digest().0);
        assert_eq!(retained.1.as_slice(), b"offline-capacity-target");
        println!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({"id":"STORAGE-C01", "size":size, "metrics":metrics, "rejection":rejection, "replay":replay, "queued_rows":rows, "retained_bytes":retained_bytes, "default_authority_cap":config.max_pending_messages_per_authority, "default_global_bytes_cap":config.max_retained_bytes, "global_capacity_rejection_verified":true, "retained_raw_digest_and_bytes_preserved":true, "classification":"fixed indexed capacity cost across authority backlog"})
        );
        steps.push(metrics.capacity_vm_steps);
        mutation_steps.push(metrics.capacity_mutation_vm_steps);
        drop(connection);
        Box::new(storage).close().unwrap();
        fs::remove_dir_all(root).unwrap();
    }
    assert!(steps[1] <= steps[0] + 10);
    assert!(mutation_steps[1] <= mutation_steps[0] + 32);
}

#[test]
fn indexed_atomic_renewal_validates_exact_periods_with_linear_cost() {
    use crate::storage_mechanism_regression as measure;
    let mut totals = Vec::new();
    let mut capacity_work = Vec::new();
    for size in [100_u64, 200, 400] {
        let root = temporary_directory("authority-history-mechanism");
        let mut storage = SqliteStorage::open(&root).unwrap();
        let mut authority = prepare_authority(&mut storage, 1);
        let config = WorkerOutboundQueueConfig::default();
        let mut final_pair = None;
        measure::reset();
        for sequence in 1..=size {
            let expires = Instant(format!("2027-02-15T08:00:20.{sequence:03}Z"));
            let mut extended = authority.clone();
            extended.lease_expires_at = expires.clone();
            let frame = request(&extended, 1_000_000 + sequence, b"synthetic-renewal-frame");
            let slot = &authority.slot;
            let renewal = ExecutionLeaseRenewal {
                expires_at: expires,
                prior_expires_at: authority.lease_expires_at.clone(),
                fencing_token: slot.fencing_token.clone(),
                job_id: slot.job_id.clone(),
                lease_id: slot.lease_id.clone(),
                message_id: frame.message_id().clone(),
                request_id: RequestId(id("req", 1_000_000 + sequence)),
                sent_at: at(6),
                worker_id: slot.worker_id.clone(),
                worker_instance_id: slot.worker_instance_id.clone(),
                attempt: slot.attempt,
            };
            let receipt = storage
                .worker_outbound_queue(config)
                .unwrap()
                .renew_lease_and_enqueue(&renewal, &frame)
                .unwrap();
            assert_eq!(receipt.status, LeaseWriteStatus::Accepted);
            assert_eq!(receipt.lease.unwrap().expires_at, extended.lease_expires_at);
            authority = extended;
            final_pair = Some((renewal, frame));
        }
        let metrics = measure::finish();
        assert!(metrics.lease_receipt_rows <= 4 * size);
        assert!(metrics.capacity_vm_steps <= 256 * size);
        assert!(metrics.capacity_mutation_vm_steps <= 512 * size);
        let (renewal, frame) = final_pair.unwrap();
        measure::reset();
        assert_eq!(
            storage
                .worker_outbound_queue(config)
                .unwrap()
                .renew_lease_and_enqueue(&renewal, &frame)
                .unwrap()
                .status,
            LeaseWriteStatus::Duplicate
        );
        let replay = measure::finish();
        assert_eq!(replay.lease_receipt_rows, 0);
        let connection = Connection::open(storage.database_path()).unwrap();
        let receipts: i64 = connection.query_row("SELECT COUNT(*) FROM execution_lease_request_receipts WHERE job_id=?1 AND operation IN ('claim','renew')", [&authority.slot.job_id.0], |row| row.get(0)).unwrap();
        assert_eq!(receipts, i64::try_from(size + 1).unwrap());
        let plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT response_json FROM execution_lease_request_receipts
                 WHERE job_id = ?1 AND operation IN ('claim', 'renew')
                   AND json_extract(response_json, '$.status') = 'accepted'
                   AND json_extract(response_json, '$.lease.expires_at') = ?2",
                [&authority.slot.job_id.0, &authority.lease_expires_at.0],
                |row| row.get(3),
            )
            .unwrap();
        assert!(plan.contains("SEARCH") && plan.contains("execution_lease_accepted_period"));
        println!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({"id":"M10", "size":size, "metrics":metrics, "replay":replay, "retained_accepted_receipts":receipts, "plan":plan, "classification":"exact accepted period lookup"})
        );
        totals.push(metrics.lease_receipt_bytes);
        capacity_work.push(metrics.capacity_vm_steps + metrics.capacity_mutation_vm_steps);
        drop(connection);
        Box::new(storage).close().unwrap();
        fs::remove_dir_all(root).unwrap();
    }
    assert!(totals[1] < 3 * totals[0]);
    assert!(totals[2] < 3 * totals[1]);
    assert!(capacity_work[1] < 3 * capacity_work[0]);
    assert!(capacity_work[2] < 3 * capacity_work[1]);
}

#[test]
fn indexed_enqueue_rejects_foreign_job_in_selected_accepted_period() {
    let root = temporary_directory("accepted-period-corruption");
    let mut storage = SqliteStorage::open(&root).unwrap();
    let authority = prepare_authority(&mut storage, 1);
    let connection = Connection::open(storage.database_path()).unwrap();
    let encoded: String = connection
        .query_row(
            "SELECT response_json FROM execution_lease_request_receipts
             WHERE operation = 'claim' AND job_id = ?1",
            [&authority.slot.job_id.0],
            |row| row.get(0),
        )
        .unwrap();
    let mut corrupt: serde_json::Value = serde_json::from_str(&encoded).unwrap();
    corrupt["lease"]["job_id"] = serde_json::json!(id("job", 2));
    connection
        .execute(
            "INSERT INTO execution_lease_request_receipts
             (operation, job_id, request_id, request_digest, response_json)
             VALUES ('renew', ?1, ?2, ?3, ?4)",
            rusqlite::params![
                authority.slot.job_id.0,
                id("req", 90_000),
                format!("sha256:{}", "c".repeat(64)),
                serde_json::to_string(&corrupt).unwrap(),
            ],
        )
        .unwrap();
    let frame = request(&authority, 90_001, b"selected-period-corruption");
    assert_eq!(
        storage
            .worker_outbound_queue(WorkerOutboundQueueConfig::default())
            .unwrap()
            .enqueue(&frame)
            .unwrap_err()
            .code(),
        WorkerOutboundQueueErrorCode::Storage
    );
    let pending: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM internal_worker_outbound_messages",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pending, 0);
    drop(connection);
    Box::new(storage).close().unwrap();
    fs::remove_dir_all(root).unwrap();
}

fn capacity_totals(connection: &Connection) -> (i64, i64, i64) {
    let (pending, bytes): (i64, i64) = connection
        .query_row(
            "SELECT pending_messages, retained_bytes FROM internal_worker_outbound_capacity_head
             WHERE singleton = 1 AND layout_version = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let slots = connection
        .query_row(
            "SELECT COALESCE(SUM(pending_messages), 0) FROM internal_worker_outbound_capacity_slots",
            [],
            |row| row.get(0),
        )
        .unwrap();
    (pending, bytes, slots)
}

fn downgrade_capacity_layout(connection: &Connection) {
    connection
        .execute_batch(
            "DROP TRIGGER internal_worker_outbound_capacity_insert;
             DROP TRIGGER internal_worker_outbound_capacity_delete;
             DROP TRIGGER internal_worker_outbound_capacity_update;
             DROP TABLE internal_worker_outbound_capacity_slots;
             DROP TABLE internal_worker_outbound_capacity_head;",
        )
        .unwrap();
}

#[test]
fn indexed_capacity_backfill_preserves_old_bytes_and_rolls_back_midway_failure() {
    let root = temporary_directory("capacity-old-layout");
    let mut storage = SqliteStorage::open(&root).unwrap();
    let authority = prepare_authority(&mut storage, 1);
    let first = request(&authority, 91_001, b"first-retained-frame");
    let second = request(&authority, 91_002, b"second-retained-frame");
    {
        let mut queue = storage.worker_outbound_queue(config()).unwrap();
        queue.enqueue(&first).unwrap();
        queue.enqueue(&second).unwrap();
    }
    let database = storage.database_path().to_owned();
    Box::new(storage).close().unwrap();
    let connection = Connection::open(&database).unwrap();
    let before = retained_capacity_frame_rows(&connection);
    downgrade_capacity_layout(&connection);
    // A real SQL fault occurs after the header backfill has run. The empty
    // staging table is not a completed layout because no header marker exists.
    connection
        .execute_batch(crate::worker_outbound_queue::OUTBOUND_CAPACITY_SLOTS_SCHEMA)
        .unwrap();
    connection.execute_batch(
        "CREATE TRIGGER fail_capacity_backfill BEFORE INSERT ON internal_worker_outbound_capacity_slots
         BEGIN SELECT RAISE(FAIL, 'injected capacity backfill failure'); END;"
    ).unwrap();
    let mut storage = SqliteStorage::open(&root).unwrap();
    assert!(storage.worker_outbound_queue(config()).is_err());
    let marker_tables: i64 = connection
        .query_row("SELECT COUNT(*) FROM sqlite_master WHERE name = 'internal_worker_outbound_capacity_head'", [], |row| row.get(0))
        .unwrap();
    let guards: i64 = connection
        .query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger' AND name LIKE 'internal_worker_outbound_capacity_%'", [], |row| row.get(0))
        .unwrap();
    assert_eq!((marker_tables, guards), (0, 0));
    assert_eq!(
        connection
            .query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM internal_worker_outbound_capacity_slots",
                [],
                |row| row.get(0)
            )
            .unwrap(),
        0
    );
    connection
        .execute_batch("DROP TRIGGER fail_capacity_backfill;")
        .unwrap();
    storage.worker_outbound_queue(config()).unwrap();
    let after = retained_capacity_frame_rows(&connection);
    assert_eq!(before, after);
    let bytes = before
        .iter()
        .map(|row| i64::try_from(row.3.len()).unwrap())
        .sum();
    assert_eq!(capacity_totals(&connection), (2, bytes, 2));
    assert_eq!(
        storage
            .worker_outbound_queue(config())
            .unwrap()
            .enqueue(&request(&authority, 91_003, b"over-slot-cap"))
            .unwrap_err()
            .code(),
        WorkerOutboundQueueErrorCode::CapacityExceeded
    );
    let low_bytes = WorkerOutboundQueueConfig {
        max_frame_bytes: 16,
        max_pending_messages_per_authority: 10,
        max_retained_bytes: usize::try_from(bytes).unwrap(),
        ..config()
    };
    assert_eq!(
        storage
            .worker_outbound_queue(low_bytes)
            .unwrap()
            .enqueue(&request(&authority, 91_004, b"over-global-cap"))
            .unwrap_err()
            .code(),
        WorkerOutboundQueueErrorCode::CapacityExceeded
    );
    Box::new(storage).close().unwrap();
    let mut reopened = SqliteStorage::open(&root).unwrap();
    reopened.worker_outbound_queue(config()).unwrap();
    assert_eq!(capacity_totals(&connection), (2, bytes, 2));
    connection
        .execute_batch("DROP TRIGGER internal_worker_outbound_capacity_delete;")
        .unwrap();
    assert!(
        reopened.worker_outbound_queue(config()).is_err(),
        "completed marker with missing old-writer guard must fail closed"
    );
    drop(connection);
    Box::new(reopened).close().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn indexed_capacity_updates_are_atomic_across_renewal_ack_and_terminal_cleanup() {
    let root = temporary_directory("capacity-atomic-updates");
    let mut storage = SqliteStorage::open(&root).unwrap();
    let authority = prepare_authority(&mut storage, 1);
    let first = request(&authority, 92_001, b"first-capacity-frame");
    let (connection, _renewal, frame) =
        prepare_failed_capacity_increment(&mut storage, &authority, &first);
    let bytes =
        i64::try_from(b"first-capacity-frame".len() + b"encoded-renewal-frame".len()).unwrap();
    assert_eq!(capacity_totals(&connection), (2, bytes, 2));
    assert_eq!(
        storage
            .worker_outbound_queue(config())
            .unwrap()
            .enqueue(&request(frame.authority(), 92_002, b"third-period-frame"))
            .unwrap_err()
            .code(),
        WorkerOutboundQueueErrorCode::CapacityExceeded
    );
    let page = storage
        .worker_outbound_queue(config())
        .unwrap()
        .claim_page(frame.authority(), &at(7), None, 1)
        .unwrap();
    assert_eq!(page.claims[0].message_id(), first.message_id());
    connection.execute_batch(
        "CREATE TRIGGER fail_capacity_delete BEFORE UPDATE ON internal_worker_outbound_capacity_head
         WHEN NEW.pending_messages < OLD.pending_messages
         BEGIN SELECT RAISE(FAIL, 'injected capacity decrement failure'); END;"
    ).unwrap();
    assert!(
        storage
            .worker_outbound_queue(config())
            .unwrap()
            .acknowledge(frame.authority(), first.message_id(), &at(7))
            .is_err()
    );
    assert_eq!(capacity_totals(&connection), (2, bytes, 2));
    let retained: Vec<u8> = connection
        .query_row(
            "SELECT payload FROM internal_worker_outbound_messages WHERE message_id = ?1",
            [&first.message_id().0],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, b"first-capacity-frame");
    connection
        .execute_batch("DROP TRIGGER fail_capacity_delete;")
        .unwrap();
    storage
        .worker_outbound_queue(config())
        .unwrap()
        .acknowledge(frame.authority(), first.message_id(), &at(7))
        .unwrap();
    storage
        .worker_outbound_queue(config())
        .unwrap()
        .acknowledge(frame.authority(), first.message_id(), &at(8))
        .unwrap();
    assert_eq!(
        capacity_totals(&connection),
        (1, i64::try_from(b"encoded-renewal-frame".len()).unwrap(), 1)
    );
    storage
        .worker_session_slots()
        .unwrap()
        .close(&WorkerSlotCloseRequest {
            authority: frame.authority().slot.clone(),
            request_id: RequestId(id("req", 92_003)),
            expected_revision: 1,
            outcome: WorkerSlotState::Completed,
            closed_at: at(9),
        })
        .unwrap();
    storage
        .worker_outbound_queue(config())
        .unwrap()
        .settle_terminal(frame.authority(), &at(9))
        .unwrap();
    assert_eq!(capacity_totals(&connection), (0, 0, 0));
    let slots: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM internal_worker_outbound_capacity_slots",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(slots, 0);
    drop(connection);
    Box::new(storage).close().unwrap();
    let mut reopened = SqliteStorage::open(&root).unwrap();
    reopened.worker_outbound_queue(config()).unwrap();
    let connection = Connection::open(reopened.database_path()).unwrap();
    assert_eq!(capacity_totals(&connection), (0, 0, 0));
    drop(connection);
    Box::new(reopened).close().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn indexed_capacity_guards_track_raw_writers_and_all_retained_states() {
    let root = temporary_directory("capacity-raw-writers");
    let mut storage = SqliteStorage::open(&root).unwrap();
    let first_authority = prepare_authority(&mut storage, 1);
    let second_authority = prepare_authority(&mut storage, 2);
    let first = request(&first_authority, 93_001, b"first");
    let second = request(&second_authority, 93_002, b"second");
    enqueue_raw_capacity_fixture_pair(&mut storage, &first, &second);
    let connection = Connection::open(storage.database_path()).unwrap();
    assert_eq!(capacity_totals(&connection), (2, 11, 2));
    let cloned = id("xmsg", 93_003);
    connection
        .execute(
            "INSERT INTO internal_worker_outbound_messages
         (message_id, authority_digest, worker_id, worker_instance_id, worker_session_id,
          job_id, lease_id, attempt, fencing_token, sent_at, payload_digest, payload,
          state, delivery_attempts, claimed_at)
         SELECT ?1, authority_digest, worker_id, worker_instance_id, worker_session_id,
                job_id, lease_id, attempt, fencing_token, sent_at, payload_digest, payload,
                state, delivery_attempts, claimed_at
         FROM internal_worker_outbound_messages WHERE message_id = ?2",
            [&cloned, &first.message_id().0],
        )
        .unwrap();
    assert_eq!(capacity_totals(&connection), (3, 16, 3));
    assert_eq!(
        storage
            .worker_outbound_queue(config())
            .unwrap()
            .enqueue(&request(&first_authority, 93_004, b"over-slot"))
            .unwrap_err()
            .code(),
        WorkerOutboundQueueErrorCode::CapacityExceeded
    );
    connection
        .execute(
            "UPDATE internal_worker_outbound_messages
         SET state = 'claimed', delivery_attempts = 1, claimed_at = ?1 WHERE message_id = ?2",
            [&at(7).0, &cloned],
        )
        .unwrap();
    assert_eq!(capacity_totals(&connection), (3, 16, 3));
    let payload = b"longer-retained-payload";
    let digest = format!("sha256:{:x}", sha2::Sha256::digest(payload));
    connection
        .execute(
            "UPDATE internal_worker_outbound_messages SET payload = ?1, payload_digest = ?2
         WHERE message_id = ?3",
            rusqlite::params![payload.as_slice(), digest, cloned],
        )
        .unwrap();
    assert_eq!(
        capacity_totals(&connection),
        (3, 11 + i64::try_from(payload.len()).unwrap(), 3)
    );
    connection
        .execute(
            "UPDATE internal_worker_outbound_messages
         SET (authority_digest, worker_id, worker_instance_id, worker_session_id,
              job_id, lease_id, attempt, fencing_token) =
             (SELECT authority_digest, worker_id, worker_instance_id, worker_session_id,
                     job_id, lease_id, attempt, fencing_token
              FROM internal_worker_outbound_messages WHERE message_id = ?1)
         WHERE message_id = ?2",
            [&second.message_id().0, &cloned],
        )
        .unwrap();
    let by_worker: Vec<(String, i64)> = connection.prepare(
        "SELECT worker_id, pending_messages FROM internal_worker_outbound_capacity_slots ORDER BY worker_id",
    ).unwrap().query_map([], |row| Ok((row.get(0)?, row.get(1)?))).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(
        by_worker,
        vec![
            (first_authority.slot.worker_id.0.clone(), 1),
            (second_authority.slot.worker_id.0.clone(), 2)
        ]
    );
    let low_bytes = WorkerOutboundQueueConfig {
        max_retained_bytes: 256,
        ..config()
    };
    assert_eq!(
        storage
            .worker_outbound_queue(low_bytes)
            .unwrap()
            .enqueue(&request(&first_authority, 93_005, &[b'x'; 256]))
            .unwrap_err()
            .code(),
        WorkerOutboundQueueErrorCode::CapacityExceeded
    );
    connection
        .execute("DELETE FROM internal_worker_outbound_messages", [])
        .unwrap();
    assert_eq!(capacity_totals(&connection), (0, 0, 0));
    drop(connection);
    Box::new(storage).close().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn indexed_capacity_reopen_rejects_completed_layout_corruption() {
    for corruption in ["missing-slots", "weakened-slots", "missing-head"] {
        let root = temporary_directory(corruption);
        let mut storage = SqliteStorage::open(&root).unwrap();
        let authority = prepare_authority(&mut storage, 1);
        let first = request(&authority, 94_001, b"retained-original-frame");
        let second = request(&authority, 94_002, b"retained-next-frame");
        storage
            .worker_outbound_queue(config())
            .unwrap()
            .enqueue(&first)
            .unwrap();
        storage
            .worker_outbound_queue(config())
            .unwrap()
            .enqueue(&second)
            .unwrap();
        let database = storage.database_path().to_owned();
        Box::new(storage).close().unwrap();
        let connection = Connection::open(&database).unwrap();
        let snapshot = || {
            connection.prepare("SELECT message_id, payload_digest, payload FROM internal_worker_outbound_messages ORDER BY sequence")
                .unwrap().query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Vec<u8>>(2)?)))
                .unwrap().collect::<Result<Vec<_>, _>>().unwrap()
        };
        let before = snapshot();
        if corruption == "missing-head" {
            connection
                .execute_batch("DROP TABLE internal_worker_outbound_capacity_head;")
                .unwrap();
        } else {
            connection
                .execute_batch("DROP TABLE internal_worker_outbound_capacity_slots;")
                .unwrap();
            if corruption == "weakened-slots" {
                connection.execute_batch(
                    "CREATE TABLE internal_worker_outbound_capacity_slots (
                     worker_id TEXT NOT NULL, worker_instance_id TEXT NOT NULL, worker_session_id TEXT NOT NULL,
                     job_id TEXT NOT NULL, lease_id TEXT NOT NULL, attempt INTEGER NOT NULL, fencing_token TEXT NOT NULL,
                     pending_messages INTEGER NOT NULL,
                     PRIMARY KEY (worker_id, worker_instance_id, worker_session_id, job_id, lease_id, attempt, fencing_token));"
                ).unwrap();
            }
        }
        let mut reopened = SqliteStorage::open(&root).unwrap();
        assert!(
            reopened.worker_outbound_queue(config()).is_err(),
            "completed capacity layout must reject {corruption}"
        );
        assert_eq!(snapshot(), before);
        drop(connection);
        Box::new(reopened).close().unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn indexed_capacity_missing_exact_slot_rejects_retained_rows_without_scanning_history() {
    let root = temporary_directory("capacity-missing-exact-slot");
    let mut storage = SqliteStorage::open(&root).unwrap();
    let authority = prepare_authority(&mut storage, 1);
    let first = request(&authority, 95_001, b"retained-original-frame");
    let second = request(&authority, 95_002, b"retained-next-frame");
    storage
        .worker_outbound_queue(config())
        .unwrap()
        .enqueue(&first)
        .unwrap();
    storage
        .worker_outbound_queue(config())
        .unwrap()
        .enqueue(&second)
        .unwrap();
    let connection = Connection::open(storage.database_path()).unwrap();
    let snapshot = || {
        connection.prepare("SELECT message_id, payload_digest, payload FROM internal_worker_outbound_messages ORDER BY sequence")
            .unwrap().query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Vec<u8>>(2)?)))
            .unwrap().collect::<Result<Vec<_>, _>>().unwrap()
    };
    let before = snapshot();
    let totals = capacity_totals(&connection);
    let slot = &authority.slot;
    assert_eq!(
        connection
            .execute(
                "DELETE FROM internal_worker_outbound_capacity_slots
         WHERE worker_id = ?1 AND worker_instance_id = ?2 AND worker_session_id = ?3
           AND job_id = ?4 AND lease_id = ?5 AND attempt = ?6 AND fencing_token = ?7",
                rusqlite::params![
                    slot.worker_id.0,
                    slot.worker_instance_id.0,
                    slot.worker_session_id.0,
                    slot.job_id.0,
                    slot.lease_id.0,
                    i64::try_from(slot.attempt).unwrap(),
                    slot.fencing_token.0
                ],
            )
            .unwrap(),
        1
    );
    assert_eq!(
        storage
            .worker_outbound_queue(config())
            .unwrap()
            .enqueue(&request(&authority, 95_003, b"must-not-reset-cap"))
            .unwrap_err()
            .code(),
        WorkerOutboundQueueErrorCode::Storage
    );
    assert_eq!(snapshot(), before);
    let after = capacity_totals(&connection);
    assert_eq!((after.0, after.1), (totals.0, totals.1));
    assert_eq!(after.2, 0);
    let plan: String = connection
        .query_row(
            "EXPLAIN QUERY PLAN SELECT 1 FROM internal_worker_outbound_messages
         WHERE worker_id = ?1 AND worker_instance_id = ?2 AND worker_session_id = ?3
           AND job_id = ?4 AND lease_id = ?5 AND attempt = ?6 AND fencing_token = ?7 LIMIT 1",
            rusqlite::params![
                slot.worker_id.0,
                slot.worker_instance_id.0,
                slot.worker_session_id.0,
                slot.job_id.0,
                slot.lease_id.0,
                i64::try_from(slot.attempt).unwrap(),
                slot.fencing_token.0
            ],
            |row| row.get(3),
        )
        .unwrap();
    assert!(
        plan.contains("SEARCH") && plan.contains("internal_worker_outbound_capacity_route"),
        "{plan}"
    );
    let other = prepare_authority(&mut storage, 2);
    storage
        .worker_outbound_queue(config())
        .unwrap()
        .enqueue(&request(&other, 95_004, b"new-slot"))
        .unwrap();
    assert_eq!(snapshot().len(), 3);
    drop(connection);
    Box::new(storage).close().unwrap();
    fs::remove_dir_all(root).unwrap();
}
