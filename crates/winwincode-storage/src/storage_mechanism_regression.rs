// SPDX-License-Identifier: Apache-2.0

use std::cell::RefCell;

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub(crate) struct Metrics {
    pub receipt_vm_steps: u64,
    pub audit_vm_steps: u64,
    pub lease_receipt_rows: u64,
    pub lease_receipt_bytes: u64,
    pub capacity_vm_steps: u64,
    pub capacity_mutation_vm_steps: u64,
}

thread_local! {
    static METRICS: RefCell<Option<Metrics>> = const { RefCell::new(None) };
}

pub(crate) fn reset() {
    METRICS.with(|value| *value.borrow_mut() = Some(Metrics::default()));
}
pub(crate) fn finish() -> Metrics {
    METRICS.with(|value| value.borrow_mut().take().unwrap_or_default())
}
fn record(action: impl FnOnce(&mut Metrics)) {
    METRICS.with(|value| {
        if let Some(metrics) = value.borrow_mut().as_mut() {
            action(metrics);
        }
    });
}

pub(crate) fn receipt_query(steps: i32) {
    record(|metrics| metrics.receipt_vm_steps += u64::try_from(steps).unwrap());
}
pub(crate) fn audit_query(steps: i32) {
    record(|metrics| metrics.audit_vm_steps += u64::try_from(steps).unwrap());
}
pub(crate) fn capacity_query(steps: i32) {
    record(|metrics| metrics.capacity_vm_steps += u64::try_from(steps).unwrap());
}
pub(crate) fn capacity_mutation(steps: i32) {
    record(|metrics| metrics.capacity_mutation_vm_steps += u64::try_from(steps).unwrap());
}
pub(crate) fn lease_receipt(bytes: usize) {
    record(|metrics| {
        metrics.lease_receipt_rows += 1;
        metrics.lease_receipt_bytes += bytes as u64;
    });
}

fn temporary_root(size: u64) -> std::path::PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "winwincode-storage-method-regression-{}-{size}-{unique}",
        std::process::id()
    ))
}

fn commit(seed: u64) -> crate::StateCommit {
    let identity = crate::ReceiptIdentity::new(
        crate::ReceiptActorKey::from_encoded(b"offline-actor".to_vec()).unwrap(),
        crate::ReceiptScopeKey::from_encoded(b"offline-scope".to_vec()).unwrap(),
        winwincode_domain::RequestId(format!("req_{seed:026}")),
    )
    .unwrap();
    crate::StateCommit::new(
        identity,
        winwincode_domain::Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        format!("offline-state-{seed}"),
        0,
        b"{}".to_vec(),
        vec![crate::NewOutboxEvent::internal(
            format!("evt-{seed}"),
            "offline.state.changed",
            b"{}".to_vec(),
        )],
    )
    .with_pending_audit_event(
        crate::PendingAuditEvent::new(format!("audit-{seed}"), b"{}".to_vec()).unwrap(),
    )
}

#[test]
fn indexed_immutable_import_keeps_ordinary_limits_and_binds_atomic_source() {
    use crate::ProductStateStorage as _;
    use sha2::Digest as _;
    let root = temporary_root(65_000);
    let mut storage = crate::SqliteStorage::open(&root).unwrap();
    let initial = commit(65_000);
    storage.commit(&initial).unwrap();
    let mut next = commit(65_001);
    next.stream_id.clone_from(&initial.stream_id);
    next.expected_revision = 1;
    next.state = b"{\"layout\":\"indexed\"}".to_vec();
    let rows = (1..=32)
        .map(|sequence| {
            crate::StateMutation::new(format!("immutable-import-{sequence}"), 0, b"{}".to_vec())
                .unwrap()
        })
        .collect::<Vec<_>>();
    let wrong = crate::ImmutableStateImport::new(
        winwincode_domain::Sha256Digest(format!("sha256:{}", "b".repeat(64))),
        rows.clone(),
    )
    .unwrap();
    assert!(
        storage
            .commit(&next.clone().with_immutable_state_import(wrong))
            .is_err()
    );
    assert!(storage.load_state("immutable-import-1").unwrap().is_none());
    assert_eq!(
        storage
            .load_state(&initial.stream_id)
            .unwrap()
            .unwrap()
            .revision,
        1
    );
    let source = winwincode_domain::Sha256Digest(format!(
        "sha256:{:x}",
        sha2::Sha256::digest(&initial.state)
    ));
    let import = crate::ImmutableStateImport::new(source, rows).unwrap();
    let imported = next.with_immutable_state_import(import);
    let receipt = storage.commit(&imported).unwrap();
    assert_eq!(receipt.revision, 2);
    assert_eq!(
        storage
            .load_state("immutable-import-32")
            .unwrap()
            .unwrap()
            .revision,
        1
    );
    assert!(storage.commit(&imported).unwrap().idempotent_replay);
    let mut ordinary = commit(65_002);
    for sequence in 0..=crate::MAX_STATE_MUTATIONS_PER_COMMIT {
        ordinary = ordinary.with_state_mutation(
            crate::StateMutation::new(format!("ordinary-{sequence}"), 0, b"{}".to_vec()).unwrap(),
        );
    }
    assert!(ordinary.validate_for_storage_adapter().is_err());
    assert!(
        crate::ImmutableStateImport::new(
            winwincode_domain::Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            vec![crate::StateMutation::new("existing-row", 1, b"{}".to_vec()).unwrap()]
        )
        .is_err()
    );
    Box::new(storage).close().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "heavy mechanism regression: fixed indexed receipt and audit cost over settled history"]
fn m05_m06_real_receipt_and_audit_methods_keep_fixed_indexed_cost() {
    use crate::ProductStateStorage as _;
    let mut receipt_steps = Vec::new();
    let mut audit_steps = Vec::new();
    for size in [1_000_u64, 10_000] {
        let root = temporary_root(size);
        let mut storage = crate::SqliteStorage::open(&root).unwrap();
        for seed in 1..=size {
            storage.commit(&commit(seed)).unwrap();
            storage.mark_published(&format!("evt-{seed}")).unwrap();
            storage
                .mark_audit_event_persisted(&format!("audit-{seed}"))
                .unwrap();
        }
        reset();
        let target = commit(size + 1);
        let receipt = storage.commit(&target).unwrap();
        let committed = finish();
        assert_eq!(receipt.events.len(), 1);
        reset();
        let replay = storage
            .load_receipt(&receipt.receipt_identity, &receipt.command_digest)
            .unwrap()
            .unwrap();
        let loaded = finish();
        assert!(replay.idempotent_replay);
        assert_eq!(replay.events, receipt.events);
        assert!(committed.receipt_vm_steps <= 64);
        assert!(loaded.receipt_vm_steps <= 64);
        storage
            .mark_published(&format!("evt-{}", size + 1))
            .unwrap();
        storage
            .mark_audit_event_persisted(&format!("audit-{}", size + 1))
            .unwrap();
        reset();
        assert!(storage.pending_audit_events().unwrap().is_empty());
        let audit = finish();
        assert!(audit.audit_vm_steps <= 16);
        assert!(storage.pending_events().unwrap().is_empty());
        let old_outbox: i64 = storage
            .connection()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM outbox WHERE published = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let old_audit: i64 = storage
            .connection()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM audit_outbox WHERE persisted = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_outbox, i64::try_from(size + 1).unwrap());
        assert_eq!(old_audit, i64::try_from(size + 1).unwrap());
        println!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({"id":"M05", "size":size, "commit":committed, "load_receipt":loaded, "returned_events":1, "settled_history":old_outbox})
        );
        println!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({"id":"M06", "size":size, "metrics":audit, "empty_pending_audit":true, "settled_history":old_audit})
        );
        receipt_steps.push(loaded.receipt_vm_steps);
        audit_steps.push(audit.audit_vm_steps);
        Box::new(storage).close().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    assert!(receipt_steps[1] <= receipt_steps[0] + 10);
    assert!(audit_steps[1] <= audit_steps[0] + 10);
}

mod outbound {
    include!("../tests/worker_outbound_queue.rs");
    include!("storage_outbound_regression.rs");
}

#[test]
fn indexed_receipt_and_audit_methods_do_not_scan_settled_history() {
    use crate::ProductStateStorage as _;
    let mut receipt_steps = Vec::new();
    let mut audit_steps = Vec::new();
    for size in [1_000_u64, 10_000] {
        let root = temporary_root(size);
        let mut storage = crate::SqliteStorage::open(&root).unwrap();
        for seed in 1..=size {
            storage.commit(&commit(seed)).unwrap();
            storage.mark_published(&format!("evt-{seed}")).unwrap();
            storage
                .mark_audit_event_persisted(&format!("audit-{seed}"))
                .unwrap();
        }
        reset();
        let target = commit(size + 1);
        let receipt = storage.commit(&target).unwrap();
        let committed = finish();
        assert_eq!(receipt.events.len(), 1);
        reset();
        let replay = storage
            .load_receipt(&receipt.receipt_identity, &receipt.command_digest)
            .unwrap()
            .unwrap();
        let loaded = finish();
        assert!(replay.idempotent_replay);
        assert_eq!(replay.events, receipt.events);
        storage
            .mark_published(&format!("evt-{}", size + 1))
            .unwrap();
        storage
            .mark_audit_event_persisted(&format!("audit-{}", size + 1))
            .unwrap();
        reset();
        assert!(storage.pending_audit_events().unwrap().is_empty());
        let audit = finish();
        assert!(storage.pending_events().unwrap().is_empty());
        let old_outbox: i64 = storage
            .connection()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM outbox WHERE published = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let old_audit: i64 = storage
            .connection()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM audit_outbox WHERE persisted = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_outbox, i64::try_from(size + 1).unwrap());
        assert_eq!(old_audit, i64::try_from(size + 1).unwrap());
        println!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({"id":"M05", "size":size, "commit":committed, "load_receipt":loaded, "returned_events":1, "settled_history":old_outbox})
        );
        println!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({"id":"M06", "size":size, "metrics":audit, "empty_pending_audit":true, "settled_history":old_audit})
        );
        receipt_steps.push(loaded.receipt_vm_steps);
        audit_steps.push(audit.audit_vm_steps);
        Box::new(storage).close().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    assert!(receipt_steps[1] <= receipt_steps[0] + 10);
    assert!(audit_steps[1] <= audit_steps[0] + 10);
}
