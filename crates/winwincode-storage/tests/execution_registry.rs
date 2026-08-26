#![allow(
    clippy::drop_non_drop,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use rusqlite::Connection;
use winwincode_domain::{
    ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId,
    RequestId, Sha256Digest, WorkerId, WorkerInstanceId,
};
use winwincode_storage::ProductStateStorage;
use winwincode_storage::{
    ActiveLeaseSummary, DispatchResultRequest, DispatchResultStatus, ExecutionLeaseClaim,
    ExecutionLeaseRenewal, LeaseRecovery, LeaseWriteStatus, SqliteStorage, StorageErrorKind,
    WorkerHeartbeatRequest, WorkerRegistrationRequest, WorkerRegistrationStatus,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-execution-registry-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn instant(second: u64) -> Instant {
    Instant(format!("2027-01-15T08:00:{second:02}.000Z"))
}

fn registration(seed: u64, instance: u64, request: u64) -> WorkerRegistrationRequest {
    WorkerRegistrationRequest {
        capabilities: vec!["codex".into(), "artifact".into()],
        message_id: ExecutionMessageId(id("xmsg", request)),
        request_id: RequestId(id("req", request)),
        sent_at: instant(1),
        started_at: instant(0),
        worker_id: WorkerId(id("wrk", seed)),
        worker_instance_id: WorkerInstanceId(id("wki", instance)),
    }
}

fn heartbeat(seed: u64, instance: u64, sequence: i64, message: u64) -> WorkerHeartbeatRequest {
    let sequence_second = u64::try_from(sequence).expect("fixture sequence must be positive");
    WorkerHeartbeatRequest {
        active_leases: Vec::new(),
        available_slots: 2,
        heartbeat_sequence: ExecutionSequence(sequence),
        max_slots: 4,
        message_id: ExecutionMessageId(id("xmsg", message)),
        observed_at: instant(sequence_second + 1),
        sent_at: instant(sequence_second + 1),
        worker_id: WorkerId(id("wrk", seed)),
        worker_instance_id: WorkerInstanceId(id("wki", instance)),
    }
}

fn claim(
    seed: u64,
    instance: u64,
    request: u64,
    attempt: u64,
    fence: u64,
    issued_second: u64,
    expires_second: u64,
) -> ExecutionLeaseClaim {
    ExecutionLeaseClaim {
        expires_at: instant(expires_second),
        fencing_token: FencingToken(fence.to_string()),
        issued_at: instant(issued_second),
        job_id: ExecutionJobId(id("job", seed)),
        lease_id: LeaseId(id("lse", request)),
        message_id: ExecutionMessageId(id("xmsg", request)),
        payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        request_id: RequestId(id("req", request)),
        worker_id: WorkerId(id("wrk", seed)),
        worker_instance_id: WorkerInstanceId(id("wki", instance)),
        attempt,
    }
}

fn renew(
    seed: u64,
    instance: u64,
    request: u64,
    attempt: u64,
    fence: u64,
    prior_second: u64,
    expires_second: u64,
    sent_second: u64,
) -> ExecutionLeaseRenewal {
    ExecutionLeaseRenewal {
        expires_at: instant(expires_second),
        fencing_token: FencingToken(fence.to_string()),
        job_id: ExecutionJobId(id("job", seed)),
        lease_id: LeaseId(id("lse", 12)),
        message_id: ExecutionMessageId(id("xmsg", request)),
        prior_expires_at: instant(prior_second),
        request_id: RequestId(id("req", request)),
        sent_at: instant(sent_second),
        worker_id: WorkerId(id("wrk", seed)),
        worker_instance_id: WorkerInstanceId(id("wki", instance)),
        attempt,
    }
}

fn dispatch_result(
    lease: &ExecutionLeaseClaim,
    request: u64,
    checked_second: u64,
) -> DispatchResultRequest {
    DispatchResultRequest {
        checked_at: instant(checked_second),
        expires_at: lease.expires_at.clone(),
        fencing_token: lease.fencing_token.clone(),
        issued_at: lease.issued_at.clone(),
        job_id: lease.job_id.clone(),
        lease_id: lease.lease_id.clone(),
        message_id: ExecutionMessageId(id("xmsg", request)),
        payload_digest: lease.payload_digest.clone(),
        request_id: RequestId(id("req", request)),
        sent_at: lease.issued_at.clone(),
        status: DispatchResultStatus::Accepted,
        attempt: lease.attempt,
        error: None,
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
        worker_session_id: Some(winwincode_domain::WorkerSessionId(id("wsn", request))),
    }
}

#[test]
fn registration_replays_exactly_and_conflicts_without_replacing_the_worker() {
    let root = temporary_directory("registration");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut registry = storage.execution_registry().expect("registry open");
    let request = registration(1, 1, 1);

    let first = registry.register_worker(&request).expect("registration");
    assert_eq!(first.status, WorkerRegistrationStatus::Accepted);
    assert_eq!(first.lease_recovery, LeaseRecovery::NoActiveLeases);

    let replay = registry
        .register_worker(&request)
        .expect("registration replay");
    assert_eq!(replay.status, WorkerRegistrationStatus::Duplicate);
    assert_eq!(replay.worker, first.worker);

    let mut changed = request.clone();
    changed.capabilities.push("mcp".into());
    let conflict = registry
        .register_worker(&changed)
        .expect("registration conflict");
    assert_eq!(conflict.status, WorkerRegistrationStatus::RejectedConflict);
    assert_eq!(
        registry
            .load_worker(&request.worker_id)
            .expect("worker read"),
        Some(first.worker.clone())
    );

    let mut replaced_instance_start = registration(1, 1, 2);
    replaced_instance_start.sent_at = instant(3);
    replaced_instance_start.started_at = instant(2);
    assert_eq!(
        registry
            .register_worker(&replaced_instance_start)
            .expect("replaced instance registration")
            .status,
        WorkerRegistrationStatus::RejectedConflict
    );
    assert_eq!(
        registry
            .load_worker(&request.worker_id)
            .expect("worker read after conflict"),
        Some(first.worker.clone())
    );

    drop(registry);
    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn dispatch_result_receipt_is_durable_duplicate_and_changed_body_conflict() {
    let root = temporary_directory("dispatch-result");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut registry = storage.execution_registry().expect("registry open");
    registry
        .register_worker(&registration(20, 1, 20))
        .expect("registration");
    let lease = claim(20, 1, 21, 1, 7, 1, 5);
    registry.claim_execution_job(&lease).expect("claim");

    let request = dispatch_result(&lease, 22, 2);
    let accepted = registry
        .record_dispatch_result(&request)
        .expect("dispatch result");
    assert_eq!(accepted.status, DispatchResultStatus::Accepted);
    assert!(!accepted.replayed);

    let duplicate = registry
        .record_dispatch_result(&request)
        .expect("dispatch result replay");
    assert_eq!(duplicate.status, DispatchResultStatus::Duplicate);
    assert!(duplicate.replayed);

    let mut changed = request.clone();
    changed.status = DispatchResultStatus::Conflict;
    let conflict = registry
        .record_dispatch_result(&changed)
        .expect("dispatch result changed body");
    assert_eq!(conflict.status, DispatchResultStatus::Conflict);
    assert_eq!(
        conflict.error.map(|error| error.code),
        Some(winwincode_storage::DispatchResultErrorCode::MessageConflict)
    );

    drop(registry);
    Box::new(storage).close().expect("storage close");

    let mut restarted_storage = SqliteStorage::open(&root).expect("storage reopen");
    let mut restarted_registry = restarted_storage
        .execution_registry()
        .expect("restarted registry");
    let mut restarted_request = request.clone();
    restarted_request.checked_at = instant(3);
    let restarted = restarted_registry
        .record_dispatch_result(&restarted_request)
        .expect("dispatch result restart replay");
    assert_eq!(restarted.status, DispatchResultStatus::Duplicate);
    assert!(restarted.replayed);

    drop(restarted_registry);
    Box::new(restarted_storage)
        .close()
        .expect("restarted storage close");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn new_worker_instance_is_accepted_but_reports_reacquire_until_old_lease_is_replaced() {
    let root = temporary_directory("reacquire");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut registry = storage.execution_registry().expect("registry open");
    registry
        .register_worker(&registration(2, 1, 2))
        .expect("first registration");
    let first_claim = claim(2, 1, 3, 1, 7, 1, 5);
    assert_eq!(
        registry
            .claim_execution_job(&first_claim)
            .expect("first claim")
            .status,
        LeaseWriteStatus::Accepted
    );

    let second = registry
        .register_worker(&registration(2, 2, 4))
        .expect("second registration");
    assert_eq!(second.status, WorkerRegistrationStatus::Accepted);
    assert_eq!(second.lease_recovery, LeaseRecovery::ReacquireRequired);
    assert_eq!(
        registry
            .load_worker(&WorkerId(id("wrk", 2)))
            .expect("worker read")
            .expect("worker")
            .worker_instance_id,
        WorkerInstanceId(id("wki", 2))
    );
    assert_eq!(
        registry
            .load_lease(&ExecutionJobId(id("job", 2)))
            .expect("lease read")
            .expect("old lease")
            .worker_instance_id,
        WorkerInstanceId(id("wki", 1))
    );

    assert_eq!(
        registry
            .register_worker(&registration(2, 1, 5))
            .expect("stale registration result")
            .status,
        WorkerRegistrationStatus::RejectedConflict
    );
    assert_eq!(
        registry
            .load_worker(&WorkerId(id("wrk", 2)))
            .expect("worker read after stale registration")
            .expect("worker")
            .worker_instance_id,
        WorkerInstanceId(id("wki", 2))
    );

    let old_heartbeat = registry
        .record_heartbeat(&heartbeat(2, 1, 1, 5))
        .expect("old heartbeat result");
    assert_eq!(
        old_heartbeat.status,
        LeaseWriteStatus::RejectedWorkerInstance
    );
    let new_heartbeat = registry
        .record_heartbeat(&heartbeat(2, 2, 1, 6))
        .expect("new heartbeat result");
    assert_eq!(new_heartbeat.status, LeaseWriteStatus::Accepted);

    drop(registry);
    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn heartbeat_requires_contiguous_sequence_is_idempotent_and_never_dispatches() {
    let root = temporary_directory("heartbeat");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut registry = storage.execution_registry().expect("registry open");
    registry
        .register_worker(&registration(3, 1, 7))
        .expect("registration");

    assert_eq!(
        registry
            .record_heartbeat(&heartbeat(3, 1, 1, 8))
            .expect("heartbeat")
            .status,
        LeaseWriteStatus::Accepted
    );
    assert_eq!(
        registry
            .record_heartbeat(&heartbeat(3, 1, 1, 8))
            .expect("heartbeat replay")
            .status,
        LeaseWriteStatus::Duplicate
    );
    assert_eq!(
        registry
            .record_heartbeat(&heartbeat(3, 1, 3, 9))
            .expect("heartbeat gap")
            .status,
        LeaseWriteStatus::Gap
    );
    assert_eq!(
        registry
            .record_heartbeat(&heartbeat(3, 1, 2, 10))
            .expect("heartbeat two")
            .status,
        LeaseWriteStatus::Accepted
    );
    let mut changed = heartbeat(3, 1, 2, 10);
    changed.available_slots = 1;
    assert_eq!(
        registry
            .record_heartbeat(&changed)
            .expect("heartbeat conflict")
            .status,
        LeaseWriteStatus::RejectedConflict
    );
    assert_eq!(
        registry
            .load_lease(&ExecutionJobId(id("job", 3)))
            .expect("lease read"),
        None
    );
    assert_eq!(
        registry
            .load_worker(&WorkerId(id("wrk", 3)))
            .expect("worker read")
            .expect("worker")
            .heartbeat_sequence,
        2
    );

    drop(registry);
    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn heartbeat_rejects_foreign_stale_and_expired_active_leases_without_writing() {
    let root = temporary_directory("heartbeat-lease-authority");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut registry = storage.execution_registry().expect("registry open");
    registry
        .register_worker(&registration(30, 1, 61))
        .expect("registration");
    let lease = claim(30, 1, 62, 1, 7, 1, 5);
    registry.claim_execution_job(&lease).expect("claim");

    let mut foreign = heartbeat(30, 1, 1, 63);
    foreign.active_leases = vec![ActiveLeaseSummary {
        job_id: lease.job_id.clone(),
        lease_id: LeaseId(id("lse", 99)),
        attempt: lease.attempt,
        fencing_token: lease.fencing_token.clone(),
    }];
    assert_eq!(
        registry
            .record_heartbeat(&foreign)
            .expect("foreign lease heartbeat")
            .status,
        LeaseWriteStatus::RejectedConflict
    );
    assert_eq!(
        registry
            .load_worker(&lease.worker_id)
            .expect("worker read")
            .expect("worker")
            .heartbeat_sequence,
        0
    );

    let mut stale = heartbeat(30, 1, 1, 64);
    stale.active_leases = vec![ActiveLeaseSummary {
        job_id: lease.job_id.clone(),
        lease_id: lease.lease_id.clone(),
        attempt: lease.attempt,
        fencing_token: FencingToken("6".into()),
    }];
    assert_eq!(
        registry
            .record_heartbeat(&stale)
            .expect("stale lease heartbeat")
            .status,
        LeaseWriteStatus::RejectedStaleFencingToken
    );

    let mut expired = heartbeat(30, 1, 1, 65);
    expired.observed_at = instant(5);
    expired.sent_at = instant(5);
    expired.active_leases = vec![ActiveLeaseSummary {
        job_id: lease.job_id.clone(),
        lease_id: lease.lease_id,
        attempt: lease.attempt,
        fencing_token: lease.fencing_token,
    }];
    assert_eq!(
        registry
            .record_heartbeat(&expired)
            .expect("expired lease heartbeat")
            .status,
        LeaseWriteStatus::RejectedExpiredLease
    );
    assert_eq!(
        registry
            .load_worker(&WorkerId(id("wrk", 30)))
            .expect("worker read after rejected heartbeats")
            .expect("worker")
            .heartbeat_sequence,
        0
    );

    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn claim_and_renew_replay_exactly_and_reject_expired_stale_or_foreign_writes() {
    let root = temporary_directory("lease");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut registry = storage.execution_registry().expect("registry open");
    registry
        .register_worker(&registration(4, 1, 11))
        .expect("registration");

    let first_claim = claim(4, 1, 12, 1, 7, 1, 5);
    let accepted = registry.claim_execution_job(&first_claim).expect("claim");
    assert_eq!(accepted.status, LeaseWriteStatus::Accepted);
    let replay = registry
        .claim_execution_job(&first_claim)
        .expect("claim replay");
    assert_eq!(replay.status, LeaseWriteStatus::Duplicate);
    assert_eq!(replay.lease, accepted.lease);

    let mut changed = first_claim.clone();
    changed.payload_digest = Sha256Digest(format!("sha256:{}", "b".repeat(64)));
    assert_eq!(
        registry
            .claim_execution_job(&changed)
            .expect("claim conflict")
            .status,
        LeaseWriteStatus::RejectedConflict
    );

    let mut foreign = first_claim.clone();
    foreign.request_id = RequestId(id("req", 13));
    foreign.message_id = ExecutionMessageId(id("xmsg", 13));
    foreign.worker_instance_id = WorkerInstanceId(id("wki", 99));
    assert_eq!(
        registry
            .claim_execution_job(&foreign)
            .expect("foreign claim")
            .status,
        LeaseWriteStatus::RejectedWorkerInstance
    );

    let renewal = renew(4, 1, 14, 1, 7, 5, 7, 2);
    assert_eq!(
        registry
            .renew_execution_lease(&renewal)
            .expect("renew")
            .status,
        LeaseWriteStatus::Accepted
    );
    assert_eq!(
        registry
            .renew_execution_lease(&renewal)
            .expect("renew replay")
            .status,
        LeaseWriteStatus::Duplicate
    );

    let mut stale = renewal.clone();
    stale.request_id = RequestId(id("req", 15));
    stale.message_id = ExecutionMessageId(id("xmsg", 15));
    stale.fencing_token = FencingToken("6".into());
    assert_eq!(
        registry
            .renew_execution_lease(&stale)
            .expect("stale renew")
            .status,
        LeaseWriteStatus::RejectedStaleFencingToken
    );

    let mut expired = renewal.clone();
    expired.request_id = RequestId(id("req", 16));
    expired.message_id = ExecutionMessageId(id("xmsg", 16));
    expired.sent_at = instant(7);
    assert_eq!(
        registry
            .renew_execution_lease(&expired)
            .expect("expired renew")
            .status,
        LeaseWriteStatus::RejectedExpiredLease
    );
    assert_eq!(
        registry
            .load_lease(&ExecutionJobId(id("job", 4)))
            .expect("lease read")
            .expect("lease")
            .expires_at,
        instant(7)
    );

    let mut reused_lease_id = claim(4, 1, 17, 2, 8, 8, 12);
    reused_lease_id.lease_id = first_claim.lease_id.clone();
    assert_eq!(
        registry
            .claim_execution_job(&reused_lease_id)
            .expect("reused lease id")
            .status,
        LeaseWriteStatus::RejectedConflict
    );

    let replacement = claim(4, 1, 17, 2, 8, 8, 12);
    assert_eq!(
        registry
            .claim_execution_job(&replacement)
            .expect("replacement claim")
            .status,
        LeaseWriteStatus::Accepted
    );
    assert_eq!(
        registry
            .load_lease(&ExecutionJobId(id("job", 4)))
            .expect("lease read")
            .expect("replacement")
            .attempt,
        2
    );

    drop(registry);
    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn registry_and_lease_survive_restart_and_exact_replays_remain_idempotent() {
    let root = temporary_directory("restart");
    let first_claim = claim(5, 1, 22, 1, 9, 1, 5);
    {
        let mut storage = SqliteStorage::open(&root).expect("storage open");
        let mut registry = storage.execution_registry().expect("registry open");
        registry
            .register_worker(&registration(5, 1, 21))
            .expect("registration");
        registry.claim_execution_job(&first_claim).expect("claim");
        drop(registry);
        Box::new(storage).close().expect("first close");
    }

    let mut storage = SqliteStorage::open(&root).expect("restart storage");
    let mut registry = storage.execution_registry().expect("restart registry");
    assert!(
        registry
            .load_worker(&WorkerId(id("wrk", 5)))
            .expect("worker read")
            .is_some()
    );
    assert!(
        registry
            .load_lease(&ExecutionJobId(id("job", 5)))
            .expect("lease read")
            .is_some()
    );
    assert_eq!(
        registry
            .register_worker(&registration(5, 1, 21))
            .expect("registration replay")
            .status,
        WorkerRegistrationStatus::Duplicate
    );
    assert_eq!(
        registry
            .claim_execution_job(&first_claim)
            .expect("claim replay")
            .status,
        LeaseWriteStatus::Duplicate
    );
    drop(registry);
    Box::new(storage).close().expect("restart close");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn concurrent_exact_registration_and_claim_have_one_commit_and_one_replay() {
    let root = temporary_directory("concurrency");
    {
        let mut bootstrap = SqliteStorage::open(&root).expect("bootstrap storage open");
        let registry = bootstrap
            .execution_registry()
            .expect("bootstrap registry open");
        drop(registry);
        Box::new(bootstrap)
            .close()
            .expect("bootstrap storage close");
    }
    let barrier = Arc::new(Barrier::new(2));
    let request = registration(6, 1, 31);
    let lease = claim(6, 1, 32, 1, 3, 1, 5);
    let handles = (0..2)
        .map(|_| {
            let root = root.clone();
            let barrier = Arc::clone(&barrier);
            let request = request.clone();
            let lease = lease.clone();
            thread::spawn(move || {
                let mut storage = SqliteStorage::open(&root).expect("thread storage open");
                let mut registry = storage.execution_registry().expect("thread registry open");
                barrier.wait();
                let registration = registry
                    .register_worker(&request)
                    .expect("thread registration");
                let claim = registry.claim_execution_job(&lease).expect("thread claim");
                (registration.status, claim.status)
            })
        })
        .collect::<Vec<_>>();
    let results = handles
        .into_iter()
        .map(|handle| handle.join().expect("thread join"))
        .collect::<Vec<_>>();
    assert_eq!(
        results
            .iter()
            .filter(|(registration, _)| *registration == WorkerRegistrationStatus::Accepted)
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|(_, claim)| *claim == LeaseWriteStatus::Accepted)
            .count(),
        1
    );

    let mut storage = SqliteStorage::open(&root).expect("read storage open");
    let registry = storage.execution_registry().expect("read registry open");
    assert!(
        registry
            .load_lease(&ExecutionJobId(id("job", 6)))
            .expect("lease read")
            .is_some()
    );
    drop(registry);
    Box::new(storage).close().expect("read close");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn lease_insert_failure_rolls_back_the_lease_and_its_request_receipt() {
    let root = temporary_directory("rollback");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut registry = storage.execution_registry().expect("registry open");
    registry
        .register_worker(&registration(7, 1, 41))
        .expect("registration");
    drop(registry);

    let connection = Connection::open(root.join("control-plane.sqlite3")).expect("injector open");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_execution_lease BEFORE INSERT ON execution_leases
             BEGIN SELECT RAISE(ABORT, 'injected execution lease failure'); END;",
        )
        .expect("lease trigger");
    connection.close().expect("injector close");

    let mut registry = storage.execution_registry().expect("registry reopen");
    let request = claim(7, 1, 42, 1, 3, 1, 5);
    let error = registry
        .claim_execution_job(&request)
        .expect_err("injected lease failure");
    assert_eq!(error.kind(), StorageErrorKind::Adapter);
    assert_eq!(
        registry
            .load_lease(&ExecutionJobId(id("job", 7)))
            .expect("lease read"),
        None
    );
    assert!(
        !registry
            .has_request("claim", &ExecutionJobId(id("job", 7)), &request.request_id)
            .expect("request receipt read")
    );

    drop(registry);
    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn worker_and_heartbeat_receipt_failures_roll_back_their_authority_rows() {
    let root = temporary_directory("receipt-rollback");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let registry = storage.execution_registry().expect("registry open");
    drop(registry);

    let connection = Connection::open(root.join("control-plane.sqlite3")).expect("injector open");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_worker_receipt
             BEFORE INSERT ON execution_worker_registration_receipts
             BEGIN SELECT RAISE(ABORT, 'injected worker receipt failure'); END;",
        )
        .expect("worker receipt trigger");
    connection.close().expect("injector close");

    let request = registration(8, 1, 51);
    let mut registry = storage.execution_registry().expect("registry reopen");
    assert_eq!(
        registry
            .register_worker(&request)
            .expect_err("worker receipt failure")
            .kind(),
        StorageErrorKind::Adapter
    );
    assert_eq!(
        registry
            .load_worker(&request.worker_id)
            .expect("worker read after rollback"),
        None
    );
    drop(registry);

    let connection = Connection::open(root.join("control-plane.sqlite3")).expect("injector open");
    connection
        .execute_batch("DROP TRIGGER fail_worker_receipt;")
        .expect("drop worker receipt trigger");
    connection.close().expect("injector close");

    let mut registry = storage.execution_registry().expect("registry reopen");
    registry
        .register_worker(&request)
        .expect("registration after rollback");
    drop(registry);

    let connection = Connection::open(root.join("control-plane.sqlite3")).expect("injector open");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_heartbeat_receipt
             BEFORE INSERT ON execution_heartbeats
             BEGIN SELECT RAISE(ABORT, 'injected heartbeat receipt failure'); END;",
        )
        .expect("heartbeat receipt trigger");
    connection.close().expect("injector close");

    let mut registry = storage.execution_registry().expect("registry reopen");
    assert_eq!(
        registry
            .record_heartbeat(&heartbeat(8, 1, 1, 52))
            .expect_err("heartbeat receipt failure")
            .kind(),
        StorageErrorKind::Adapter
    );
    assert_eq!(
        registry
            .load_worker(&request.worker_id)
            .expect("worker read after heartbeat rollback")
            .expect("worker")
            .heartbeat_sequence,
        0
    );

    drop(registry);
    let connection = Connection::open(root.join("control-plane.sqlite3")).expect("injector open");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_lease_receipt
             BEFORE INSERT ON execution_lease_request_receipts
             BEGIN SELECT RAISE(ABORT, 'injected lease receipt failure'); END;",
        )
        .expect("lease receipt trigger");
    connection.close().expect("injector close");

    let mut registry = storage.execution_registry().expect("registry reopen");
    let lease_request = claim(8, 1, 53, 1, 3, 1, 5);
    assert_eq!(
        registry
            .claim_execution_job(&lease_request)
            .expect_err("lease receipt failure")
            .kind(),
        StorageErrorKind::Adapter
    );
    assert_eq!(
        registry
            .load_lease(&lease_request.job_id)
            .expect("lease read after receipt rollback"),
        None
    );
    assert!(
        !registry
            .has_request("claim", &lease_request.job_id, &lease_request.request_id)
            .expect("request receipt read after rollback")
    );

    drop(registry);
    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("directory release");
}
