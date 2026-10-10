// SPDX-License-Identifier: Apache-2.0

#[test]
#[ignore = "mechanism audit: atomic renewal authority history cost baseline"]
fn m10_real_atomic_renewal_rebuilds_only_this_jobs_authority_history() {
    use crate::storage_mechanism_regression as measure;
    let mut totals = Vec::new();
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
        assert!(metrics.lease_receipt_rows >= size * (size + 3) / 2);
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
        let plan: String = connection.query_row("EXPLAIN QUERY PLAN SELECT response_json FROM execution_lease_request_receipts WHERE job_id=?1 AND operation IN ('claim','renew')", [&authority.slot.job_id.0], |row| row.get(3)).unwrap();
        assert!(plan.contains("SEARCH"));
        println!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({"id":"M10", "size":size, "metrics":metrics, "replay":replay, "retained_accepted_receipts":receipts, "plan":plan, "classification":"conditional long-job authority-history cost"})
        );
        totals.push(metrics.lease_receipt_bytes);
        drop(connection);
        Box::new(storage).close().unwrap();
        fs::remove_dir_all(root).unwrap();
    }
    assert!(totals[1] > 3 * totals[0]);
    assert!(totals[2] > 3 * totals[1]);
}

#[test]
#[ignore = "mechanism audit: global capacity guard over 10k retained rows"]
fn storage_c01_real_enqueue_global_capacity_guard_counts_bounded_cross_authority_backlog() {
    use crate::storage_mechanism_regression as measure;
    let mut steps = Vec::new();
    for size in [1_000_u64, 10_000] {
        let root = temporary_directory("global-capacity-mechanism");
        let mut storage = SqliteStorage::open(&root).unwrap();
        let config = WorkerOutboundQueueConfig::default();
        let mut authorities = Vec::new();
        for seed in 1..=(size / 1_000) {
            authorities.push(prepare_authority(&mut storage, seed));
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
        let connection = Connection::open(storage.database_path()).unwrap();
        let (rows, retained_bytes): (i64, i64) = connection
            .query_row(
                "SELECT COUNT(*), SUM(length(payload)) FROM internal_worker_outbound_messages",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(rows, i64::try_from(size + 1).unwrap());
        let retained_bytes = usize::try_from(retained_bytes).unwrap();
        let low_cap = WorkerOutboundQueueConfig {
            max_frame_bytes: 256,
            max_retained_bytes: retained_bytes,
            ..config
        };
        let second = request(&authorities[0], 4_000_000 + size, b"next-capacity-frame");
        assert_eq!(
            storage
                .worker_outbound_queue(low_cap)
                .unwrap()
                .enqueue(&second)
                .unwrap_err()
                .code(),
            WorkerOutboundQueueErrorCode::CapacityExceeded
        );
        assert!(
            storage
                .worker_outbound_queue(low_cap)
                .unwrap()
                .enqueue(&frame)
                .unwrap()
                .replayed
        );
        println!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({"id":"STORAGE-C01", "size":size, "metrics":metrics, "queued_rows":rows, "retained_bytes":retained_bytes, "default_authority_cap":config.max_pending_messages_per_authority, "default_global_bytes_cap":config.max_retained_bytes, "global_capacity_rejection_verified":true, "classification":"bounded necessary capacity guard"})
        );
        steps.push(metrics.capacity_vm_steps);
        drop(connection);
        Box::new(storage).close().unwrap();
        fs::remove_dir_all(root).unwrap();
    }
    assert!(steps[1] > 8 * steps[0]);
}
