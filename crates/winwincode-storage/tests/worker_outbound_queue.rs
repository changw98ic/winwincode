use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use rusqlite::Connection;
use sha2::Digest as _;
use winwincode_domain::{
    CodexThreadId, DeliveryId, ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken,
    Instant, LeaseId, OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId,
    Sha256Digest, UserId, WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_storage::{
    EXECUTION_PROTOCOL_VERSION, ExecutionAdmissionBoundary, ExecutionAdmissionLimits,
    ExecutionAdmissionPolicy, ExecutionLeaseClaim, ExecutionLeaseRenewal, ExecutionQueueScope,
    ExecutionRepositoryAccess, ExecutionReservationRequest, ExecutionReservationStart,
    LeaseWriteStatus, ProductStateStorage, SqliteStorage, WorkerAuthenticationIdentity,
    WorkerHeartbeatRequest, WorkerOutboundAuthority, WorkerOutboundEnqueueRequest,
    WorkerOutboundMessageState, WorkerOutboundQueueConfig, WorkerOutboundQueueErrorCode,
    WorkerOutboundSettlement, WorkerPlatform, WorkerPoolId, WorkerRegistrationRequest,
    WorkerSlotAuthority, WorkerSlotCloseRequest, WorkerSlotOpenRequest, WorkerSlotResourceLimits,
    WorkerSlotResources, WorkerSlotState,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-worker-outbound-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn at(second: u64) -> Instant {
    Instant(format!("2027-02-15T08:00:{second:02}.000Z"))
}

fn scope(seed: u64) -> ExecutionQueueScope {
    ExecutionQueueScope {
        organization_id: OrganizationId(id("org", seed)),
        workspace_id: WorkspaceId(id("wsp", seed)),
        project_id: ProjectId(id("prj", seed)),
        repository_id: RepositoryId(id("rep", seed)),
        product_session_id: ProductSessionId(id("psn", seed)),
        delivery_id: Some(DeliveryId(id("dlv", seed))),
    }
}

fn registration(seed: u64) -> WorkerRegistrationRequest {
    WorkerRegistrationRequest {
        authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
            control_plane_principal: "fixture-control-plane".into(),
        },
        protocol_version: EXECUTION_PROTOCOL_VERSION.into(),
        platform: WorkerPlatform::Aarch64AppleDarwin,
        capabilities: vec!["codex".into()],
        capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        security_zone: "local".into(),
        max_slots: 4,
        message_id: ExecutionMessageId(id("xmsg", 10 + seed)),
        request_id: RequestId(id("req", 10 + seed)),
        sent_at: at(1),
        started_at: at(0),
        worker_id: WorkerId(id("wrk", seed)),
        worker_instance_id: WorkerInstanceId(id("wki", seed)),
    }
}

fn heartbeat(seed: u64) -> WorkerHeartbeatRequest {
    WorkerHeartbeatRequest {
        active_leases: Vec::new(),
        available_slots: 4,
        heartbeat_sequence: ExecutionSequence(1),
        max_slots: 4,
        running_slots: 0,
        message_id: ExecutionMessageId(id("xmsg", 20 + seed)),
        observed_at: at(2),
        sent_at: at(2),
        worker_id: WorkerId(id("wrk", seed)),
        worker_instance_id: WorkerInstanceId(id("wki", seed)),
    }
}

fn lease(seed: u64) -> ExecutionLeaseClaim {
    ExecutionLeaseClaim {
        expires_at: at(20),
        fencing_token: FencingToken("1".into()),
        issued_at: at(3),
        job_id: ExecutionJobId(id("job", seed)),
        lease_id: LeaseId(id("lse", seed)),
        message_id: ExecutionMessageId(id("xmsg", 30 + seed)),
        payload_digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
        request_id: RequestId(id("req", 30 + seed)),
        worker_id: WorkerId(id("wrk", seed)),
        worker_instance_id: WorkerInstanceId(id("wki", seed)),
        attempt: 1,
    }
}

fn boundaries(
    request_scope: &ExecutionQueueScope,
    pool: &WorkerPoolId,
) -> Vec<ExecutionAdmissionBoundary> {
    vec![
        ExecutionAdmissionBoundary::Organization {
            organization_id: request_scope.organization_id.clone(),
        },
        ExecutionAdmissionBoundary::Project {
            organization_id: request_scope.organization_id.clone(),
            project_id: request_scope.project_id.clone(),
        },
        ExecutionAdmissionBoundary::Repository {
            organization_id: request_scope.organization_id.clone(),
            project_id: request_scope.project_id.clone(),
            repository_id: request_scope.repository_id.clone(),
        },
        ExecutionAdmissionBoundary::Delivery {
            organization_id: request_scope.organization_id.clone(),
            delivery_id: request_scope.delivery_id.clone().expect("delivery"),
        },
        ExecutionAdmissionBoundary::ProductSession {
            organization_id: request_scope.organization_id.clone(),
            project_id: request_scope.project_id.clone(),
            product_session_id: request_scope.product_session_id.clone(),
        },
        ExecutionAdmissionBoundary::WorkerPool {
            organization_id: request_scope.organization_id.clone(),
            worker_pool_id: pool.clone(),
        },
    ]
}

fn prepare_authority(storage: &mut SqliteStorage, seed: u64) -> WorkerOutboundAuthority {
    prepare_admission(storage, seed);
    prepare_worker_slot(storage, seed)
}

fn prepare_admission(storage: &mut SqliteStorage, seed: u64) {
    let request_scope = scope(seed);
    let pool = WorkerPoolId(id("wpl", seed));
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 4,
        max_queued: 4,
        token_budget: Some(10_000),
        cost_budget_microunits: Some(100_000),
        max_runtime_millis: Some(60_000),
    };
    let mut admission = storage.execution_admission().expect("admission");
    for boundary in boundaries(&request_scope, &pool) {
        admission
            .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
            .expect("configure admission");
    }
    let reservation = ExecutionReservationRequest {
        scope: request_scope,
        user_id: UserId(id("usr", seed)),
        worker_pool_id: pool,
        job_id: ExecutionJobId(id("job", seed)),
        request_id: RequestId(id("req", 40 + seed)),
        repository_access: ExecutionRepositoryAccess::ReadOnly,
        reserved_tokens: Some(10),
        reserved_cost_microunits: Some(10),
        runtime_limit_millis: Some(30_000),
        submitted_at: at(3),
    };
    admission.reserve(&reservation).expect("reserve");
    admission
        .start(&ExecutionReservationStart {
            scope: reservation.scope,
            worker_pool_id: reservation.worker_pool_id,
            job_id: reservation.job_id,
            request_id: RequestId(id("req", 50 + seed)),
            expected_revision: 1,
            started_at: at(4),
        })
        .expect("start");
}

fn prepare_worker_slot(storage: &mut SqliteStorage, seed: u64) -> WorkerOutboundAuthority {
    let lease = lease(seed);
    {
        let mut registry = storage.execution_registry().expect("registry");
        registry
            .register_worker(&registration(seed))
            .expect("register");
        assert_eq!(
            registry
                .record_heartbeat(&heartbeat(seed))
                .expect("heartbeat")
                .status,
            LeaseWriteStatus::Accepted
        );
        assert_eq!(
            registry
                .claim_execution_job(&lease)
                .expect("claim lease")
                .status,
            LeaseWriteStatus::Accepted
        );
    }
    let slot = WorkerSlotAuthority {
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
        worker_session_id: WorkerSessionId(id("wsn", seed)),
        codex_thread_id: CodexThreadId(id("cdx", seed)),
        job_id: lease.job_id.clone(),
        lease_id: lease.lease_id.clone(),
        attempt: lease.attempt,
        fencing_token: lease.fencing_token.clone(),
    };
    {
        let mut slots = storage.worker_session_slots().expect("slots");
        slots
            .configure_resources(
                &slot.worker_id,
                &slot.worker_instance_id,
                WorkerSlotResourceLimits {
                    max_memory_bytes: 100,
                    max_disk_bytes: 100,
                    max_processes: 1,
                },
            )
            .expect("resources");
        slots
            .open(&WorkerSlotOpenRequest {
                authority: slot.clone(),
                resources: WorkerSlotResources {
                    memory_bytes: 10,
                    disk_bytes: 10,
                    process_slots: 1,
                },
                request_id: RequestId(id("req", 60 + seed)),
                opened_at: at(5),
            })
            .expect("open slot");
    }
    WorkerOutboundAuthority {
        slot,
        lease_issued_at: lease.issued_at,
        lease_expires_at: lease.expires_at,
    }
}

fn config() -> WorkerOutboundQueueConfig {
    WorkerOutboundQueueConfig {
        max_frame_bytes: 256,
        max_pending_messages_per_authority: 2,
        max_retained_bytes: 512,
        max_claim_page_size: 1,
    }
}

fn request(
    authority: &WorkerOutboundAuthority,
    message_seed: u64,
    payload: &[u8],
) -> WorkerOutboundEnqueueRequest {
    WorkerOutboundEnqueueRequest::new(
        authority.clone(),
        ExecutionMessageId(id("xmsg", message_seed)),
        at(6),
        payload.to_vec(),
    )
    .expect("request")
}

fn assert_disconnect_retains_and_reconnects(
    storage: &mut SqliteStorage,
    authority: &WorkerOutboundAuthority,
    enqueue_request: &WorkerOutboundEnqueueRequest,
) {
    let connection = Connection::open(storage.database_path()).expect("fixture connection");
    connection
        .execute(
            "UPDATE execution_workers SET health = 'timed_out' WHERE worker_id = ?1",
            [&authority.slot.worker_id.0],
        )
        .expect("simulate disconnect");
    drop(connection);
    {
        let mut queue = storage.worker_outbound_queue(config()).expect("queue");
        assert!(
            queue
                .enqueue(enqueue_request)
                .expect("disconnect replay stays durable")
                .replayed
        );
        let page = queue
            .claim_page(authority, &at(6), None, 1)
            .expect("retained delivery is independent of cached Worker health");
        assert_eq!(page.claims[0].message_id(), enqueue_request.message_id());
    }
    let connection = Connection::open(storage.database_path()).expect("fixture connection");
    connection
        .execute(
            "UPDATE execution_workers SET health = 'healthy' WHERE worker_id = ?1",
            [&authority.slot.worker_id.0],
        )
        .expect("simulate reconnect");
}

#[test]
fn enqueue_claim_restart_replay_capacity_and_authority_are_closed() {
    let root = temporary_directory("durable-claim");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let authority = prepare_authority(&mut storage, 1);
    let foreign = prepare_authority(&mut storage, 2);
    let first = request(&authority, 100, b"first-private-frame");
    let second = request(&authority, 101, b"second-private-frame");
    {
        let mut queue = storage.worker_outbound_queue(config()).expect("queue");
        let accepted = queue.enqueue(&first).expect("enqueue");
        assert_eq!(accepted.state, Some(WorkerOutboundMessageState::Pending));
        assert!(!accepted.replayed);
        let replay = queue.enqueue(&first).expect("enqueue replay");
        assert!(replay.replayed);
        let changed = request(&authority, 100, b"changed-private-frame");
        assert_eq!(
            queue.enqueue(&changed).expect_err("changed body").code(),
            WorkerOutboundQueueErrorCode::MessageConflict
        );
        queue.enqueue(&second).expect("second enqueue");
        assert_eq!(
            queue
                .enqueue(&request(&authority, 102, b"third-private-frame"))
                .expect_err("authority count bound")
                .code(),
            WorkerOutboundQueueErrorCode::CapacityExceeded
        );
        assert_eq!(
            queue
                .claim_page(&foreign, &at(6), None, 1)
                .expect("foreign authority owns an empty page")
                .claims
                .len(),
            0
        );
    }
    assert_disconnect_retains_and_reconnects(&mut storage, &authority, &first);
    let first_page = storage
        .worker_outbound_queue(config())
        .expect("queue")
        .claim_page(&authority, &at(6), None, 1)
        .expect("first page");
    assert_eq!(first_page.claims[0].frame_bytes(), b"first-private-frame");
    assert!(first_page.claims[0].replayed());
    let second_page = storage
        .worker_outbound_queue(config())
        .expect("queue")
        .claim_page(&authority, &at(6), first_page.next_cursor.as_ref(), 1)
        .expect("second stable page");
    assert_eq!(second_page.claims[0].frame_bytes(), b"second-private-frame");
    assert!(second_page.next_cursor.is_none());

    Box::new(storage).close().expect("close");
    let mut restarted = SqliteStorage::open(&root).expect("restart");
    let replayed = restarted
        .worker_outbound_queue(config())
        .expect("queue")
        .claim_page(&authority, &at(6), None, 1)
        .expect("restart replay");
    assert_eq!(replayed.claims[0].frame_bytes(), b"first-private-frame");
    assert!(replayed.claims[0].replayed());
    assert_eq!(replayed.claims[0].delivery_attempt(), 3);

    let mut stale = authority.clone();
    stale.slot.fencing_token = FencingToken("2".into());
    assert_eq!(
        restarted
            .worker_outbound_queue(config())
            .expect("queue")
            .enqueue(&request(&stale, 103, b"stale"))
            .expect_err("stale fence")
            .code(),
        WorkerOutboundQueueErrorCode::AuthorityMismatch
    );
    Box::new(restarted).close().expect("close restart");
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn cursor_and_ack_are_bound_to_one_exact_worker_session_authority() {
    let root = temporary_directory("cross-authority");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let authority = prepare_authority(&mut storage, 5);
    let foreign = prepare_authority(&mut storage, 6);
    let first = request(&authority, 400, b"authority-one-frame");
    let second = request(&authority, 401, b"authority-one-next");
    {
        let mut queue = storage.worker_outbound_queue(config()).expect("queue");
        queue.enqueue(&first).expect("first enqueue");
        queue.enqueue(&second).expect("second enqueue");
    }
    let first_page = storage
        .worker_outbound_queue(config())
        .expect("queue")
        .claim_page(&authority, &at(6), None, 1)
        .expect("claim first page");
    let cursor = first_page.next_cursor.as_ref().expect("next cursor");
    let cursor_error = storage
        .worker_outbound_queue(config())
        .expect("queue")
        .claim_page(&foreign, &at(6), Some(cursor), 1)
        .expect_err("foreign cursor");
    assert_eq!(
        cursor_error.code(),
        WorkerOutboundQueueErrorCode::InvalidInput
    );

    let foreign_ack = storage
        .worker_outbound_queue(config())
        .expect("queue")
        .acknowledge(&foreign, first.message_id(), &at(7))
        .expect_err("foreign ack");
    assert_eq!(
        foreign_ack.code(),
        WorkerOutboundQueueErrorCode::AuthorityMismatch
    );
    let acknowledged = storage
        .worker_outbound_queue(config())
        .expect("queue")
        .acknowledge(&authority, first.message_id(), &at(7))
        .expect("exact authority ack");
    assert_eq!(
        acknowledged.settlement,
        WorkerOutboundSettlement::Acknowledged
    );
    let second_page = storage
        .worker_outbound_queue(config())
        .expect("queue")
        .claim_page(&authority, &at(7), Some(cursor), 1)
        .expect("cursor state unchanged by foreign attempts");
    assert_eq!(second_page.claims[0].message_id(), second.message_id());
    Box::new(storage).close().expect("close");
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "ordered receipt, fault and restart boundaries in one native fixture"
)]
fn remote_confirmation_survives_cleanup_failure_and_restart_without_repeated_ack() {
    let root = temporary_directory("remote-confirmation");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let authority = prepare_authority(&mut storage, 8);
    let first = request(&authority, 800, b"private-confirmed-input");
    let second = request(&authority, 801, b"next-input");
    let fault = Connection::open(storage.database_path()).expect("fault");
    {
        let mut queue = storage.worker_outbound_queue(config()).expect("queue");
        queue.enqueue(&first).expect("enqueue");
        queue.enqueue(&second).expect("enqueue");
        assert!(
            queue
                .retry_confirmed(
                    &authority.slot.worker_id,
                    &authority.slot.worker_instance_id
                )
                .expect("no proof")
                .acknowledgements
                .is_empty()
        );
        assert_eq!(
            queue
                .confirm_remote(
                    &authority.slot.worker_id,
                    &authority.slot.worker_instance_id,
                    first.message_id(),
                    &at(7)
                )
                .expect_err("unclaimed cannot be confirmed")
                .code(),
            WorkerOutboundQueueErrorCode::StateConflict
        );
        queue
            .claim_page(&authority, &at(6), None, 1)
            .expect("claim first");
        assert_eq!(
            queue
                .confirm_remote(
                    &WorkerId(id("wrk", 9)),
                    &authority.slot.worker_instance_id,
                    first.message_id(),
                    &at(7)
                )
                .expect_err("foreign worker")
                .code(),
            WorkerOutboundQueueErrorCode::AuthorityMismatch
        );
        assert!(
            !queue
                .confirm_remote(
                    &authority.slot.worker_id,
                    &authority.slot.worker_instance_id,
                    &ExecutionMessageId(id("xmsg", 899)),
                    &at(7)
                )
                .expect("unrelated transport ACK")
        );
    }
    fault.execute_batch("CREATE TRIGGER fail_confirmation BEFORE INSERT ON internal_worker_outbound_confirmations BEGIN SELECT RAISE(ABORT,'fixture'); END;").expect("fault");
    assert_eq!(
        storage
            .worker_outbound_queue(config())
            .expect("queue")
            .confirm_remote(
                &authority.slot.worker_id,
                &authority.slot.worker_instance_id,
                first.message_id(),
                &at(7)
            )
            .expect_err("receipt must be persisted before transport success")
            .code(),
        WorkerOutboundQueueErrorCode::Storage
    );
    fault.execute_batch("DROP TRIGGER fail_confirmation; CREATE TRIGGER fail_cleanup BEFORE INSERT ON internal_worker_outbound_settlements BEGIN SELECT RAISE(ABORT,'fixture'); END;").expect("cleanup fault");
    {
        let mut queue = storage.worker_outbound_queue(config()).expect("queue");
        assert!(
            queue
                .confirm_remote(
                    &authority.slot.worker_id,
                    &authority.slot.worker_instance_id,
                    first.message_id(),
                    &at(7)
                )
                .expect("proof")
        );
        assert!(
            queue
                .confirm_remote(
                    &authority.slot.worker_id,
                    &authority.slot.worker_instance_id,
                    first.message_id(),
                    &at(8)
                )
                .expect("duplicate proof")
        );
        let retry = queue
            .retry_confirmed(
                &authority.slot.worker_id,
                &authority.slot.worker_instance_id,
            )
            .expect("retry progress");
        assert_eq!(
            retry.retry_error,
            Some(WorkerOutboundQueueErrorCode::Storage)
        );
        assert!(retry.acknowledgements.is_empty());
    }
    let confirmed_at: String = fault
        .query_row(
            "SELECT confirmed_at FROM internal_worker_outbound_confirmations",
            [],
            |row| row.get(0),
        )
        .expect("clock");
    assert_eq!(
        confirmed_at,
        at(7).0,
        "duplicate keeps first reliable confirmation"
    );
    fault
        .execute_batch("DROP TRIGGER fail_cleanup")
        .expect("release");
    Box::new(storage).close().expect("close");
    let mut storage = SqliteStorage::open(&root).expect("restart");
    let mut queue = storage.worker_outbound_queue(config()).expect("queue");
    let surviving = queue
        .claim_page(&authority, &at(8), None, 1)
        .expect("restart skips proven receipt");
    assert_eq!(surviving.claims[0].message_id(), second.message_id());
    let retry = queue
        .retry_confirmed(
            &authority.slot.worker_id,
            &authority.slot.worker_instance_id,
        )
        .expect("independent cleanup");
    assert_eq!(retry.acknowledgements.len(), 1);
    assert_eq!(retry.acknowledgements[0].message_id, *first.message_id());
    assert_eq!(retry.retry_error, None);
    let next = queue
        .claim_page(&authority, &at(8), None, 1)
        .expect("next input");
    assert_eq!(next.claims[0].message_id(), second.message_id());
    let proofs: i64 = fault
        .query_row(
            "SELECT COUNT(*) FROM internal_worker_outbound_confirmations",
            [],
            |row| row.get(0),
        )
        .expect("proofs");
    assert_eq!(proofs, 0);
    // Additive migration also opens a pre-proof database without fabricating ACKs.
    fault
        .execute_batch("DROP TABLE internal_worker_outbound_confirmations")
        .expect("old schema");
    assert!(
        storage
            .worker_outbound_queue(config())
            .expect("upgrade")
            .retry_confirmed(
                &authority.slot.worker_id,
                &authority.slot.worker_instance_id
            )
            .expect("upgrade proofs")
            .acknowledgements
            .is_empty()
    );
    drop(fault);
    Box::new(storage).close().expect("close");
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn proven_remote_cleanup_crosses_renewal_and_terminal_slot_without_execution() {
    let root = temporary_directory("remote-terminal");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let authority = prepare_authority(&mut storage, 11);
    let first = request(&authority, 1100, b"confirmed-before-renewal");
    let second = request(&authority, 1101, b"confirmed-before-terminal");
    {
        let mut queue = storage.worker_outbound_queue(config()).expect("queue");
        queue.enqueue(&first).expect("enqueue");
        queue.enqueue(&second).expect("enqueue");
        let page = queue
            .claim_page(&authority, &at(6), None, 1)
            .expect("claim first");
        queue
            .claim_page(&authority, &at(6), page.next_cursor.as_ref(), 1)
            .expect("claim second");
        queue
            .confirm_remote(
                &authority.slot.worker_id,
                &authority.slot.worker_instance_id,
                first.message_id(),
                &at(7),
            )
            .expect("confirm");
    }
    let (renewal, _) = renewal_frame(&authority);
    storage
        .execution_registry()
        .expect("registry")
        .renew_execution_lease(&renewal)
        .expect("renew");
    let progress = storage
        .worker_outbound_queue(config())
        .expect("queue")
        .retry_confirmed(
            &authority.slot.worker_id,
            &authority.slot.worker_instance_id,
        )
        .expect("cleanup under accepted renewal");
    assert_eq!(progress.acknowledgements.len(), 1);
    assert_eq!(progress.retry_error, None);
    storage
        .worker_outbound_queue(config())
        .expect("queue")
        .confirm_remote(
            &authority.slot.worker_id,
            &authority.slot.worker_instance_id,
            second.message_id(),
            &at(8),
        )
        .expect("confirm after renewal");
    storage
        .worker_session_slots()
        .expect("slots")
        .close(&WorkerSlotCloseRequest {
            authority: authority.slot.clone(),
            request_id: RequestId(id("req", 1100)),
            expected_revision: 1,
            outcome: WorkerSlotState::Completed,
            closed_at: at(9),
        })
        .expect("terminal");
    Box::new(storage).close().expect("close");
    let mut storage = SqliteStorage::open(&root).expect("restart after terminal");
    let progress = storage
        .worker_outbound_queue(config())
        .expect("queue")
        .retry_confirmed(
            &authority.slot.worker_id,
            &authority.slot.worker_instance_id,
        )
        .expect("terminal cleanup");
    assert_eq!(progress.retry_error, None);
    assert_eq!(progress.acknowledgements.len(), 1);
    assert_eq!(
        progress.acknowledgements[0].settlement,
        WorkerOutboundSettlement::Terminal
    );
    Box::new(storage).close().expect("close");
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn remote_confirmation_retries_secure_checkpoint_and_rejects_corrupt_proof() {
    const SECRET: &[u8] = b"private-remote-checkpoint-fixture-9812";
    let root = temporary_directory("remote-checkpoint");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let authority = prepare_authority(&mut storage, 9);
    let first = request(&authority, 900, SECRET);
    let reader = Connection::open(storage.database_path()).expect("reader");
    {
        let mut queue = storage.worker_outbound_queue(config()).expect("queue");
        queue.enqueue(&first).expect("enqueue");
        queue
            .claim_page(&authority, &at(6), None, 1)
            .expect("claim");
        queue
            .confirm_remote(
                &authority.slot.worker_id,
                &authority.slot.worker_instance_id,
                first.message_id(),
                &at(7),
            )
            .expect("confirmation");
    }
    reader
        .execute_batch(
            "UPDATE internal_worker_outbound_confirmations SET authority_digest='corrupt'",
        )
        .expect("corrupt digest");
    assert!(
        storage
            .worker_outbound_queue(config())
            .expect("queue")
            .retry_confirmed(
                &authority.slot.worker_id,
                &authority.slot.worker_instance_id
            )
            .is_err()
    );
    reader
        .execute(
            "UPDATE internal_worker_outbound_confirmations SET authority_digest=?1",
            [format!(
                "sha256:{:x}",
                sha2::Sha256::digest(serde_json::to_vec(&authority).unwrap())
            )],
        )
        .expect("restore digest");
    reader.execute_batch("BEGIN").expect("hold snapshot");
    let held: Vec<u8> = reader
        .query_row(
            "SELECT payload FROM internal_worker_outbound_messages",
            [],
            |row| row.get(0),
        )
        .expect("read");
    assert_eq!(held, SECRET);
    let progress = storage
        .worker_outbound_queue(config())
        .expect("queue")
        .retry_confirmed(
            &authority.slot.worker_id,
            &authority.slot.worker_instance_id,
        )
        .expect("retry progress");
    assert_eq!(
        progress.retry_error,
        Some(WorkerOutboundQueueErrorCode::Storage)
    );
    assert!(progress.acknowledgements.is_empty());
    reader.execute_batch("ROLLBACK").expect("release");
    let proofs: i64 = reader
        .query_row(
            "SELECT COUNT(*) FROM internal_worker_outbound_confirmations",
            [],
            |row| row.get(0),
        )
        .expect("proof retained");
    assert_eq!(
        proofs, 1,
        "committed deletion alone does not complete secure cleanup"
    );
    drop(reader);
    let database = storage.database_path().to_path_buf();
    Box::new(storage).close().expect("close");
    let mut storage = SqliteStorage::open(&root).expect("restart");
    let progress = storage
        .worker_outbound_queue(config())
        .expect("queue")
        .retry_confirmed(
            &authority.slot.worker_id,
            &authority.slot.worker_instance_id,
        )
        .expect("secure cleanup retry");
    assert_eq!(progress.acknowledgements.len(), 1);
    assert!(progress.acknowledgements[0].replayed);
    assert_eq!(progress.retry_error, None);
    Box::new(storage).close().expect("close");
    assert_files_exclude(&database, SECRET);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn acknowledgement_retry_clears_payload_and_keeps_secret_free_tombstone() {
    const SECRET: &[u8] = b"private-input-value-queue-fixture-88419";
    let root = temporary_directory("ack-cleanup");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let authority = prepare_authority(&mut storage, 3);
    let enqueue_request = request(&authority, 200, SECRET);
    {
        let mut queue = storage.worker_outbound_queue(config()).expect("queue");
        queue.enqueue(&enqueue_request).expect("enqueue");
        let claim = queue
            .claim_page(&authority, &at(6), None, 1)
            .expect("claim");
        assert_eq!(claim.claims[0].frame_bytes(), SECRET);
        assert!(!format!("{enqueue_request:?}").contains("private-input-value"));
        assert!(!format!("{:?}", claim.claims[0]).contains("private-input-value"));
    }

    let reader = Connection::open(storage.database_path()).expect("reader");
    reader.execute_batch("BEGIN").expect("begin read");
    let held: Vec<u8> = reader
        .query_row(
            "SELECT payload FROM internal_worker_outbound_messages WHERE message_id = ?1",
            [&id("xmsg", 200)],
            |row| row.get(0),
        )
        .expect("hold old WAL snapshot");
    assert_eq!(held, SECRET);
    let checkpoint_busy = storage
        .worker_outbound_queue(config())
        .expect("queue")
        .acknowledge(&authority, enqueue_request.message_id(), &at(7))
        .expect_err("active reader prevents secure WAL truncate");
    assert_eq!(
        checkpoint_busy.code(),
        WorkerOutboundQueueErrorCode::Storage
    );
    reader.execute_batch("ROLLBACK").expect("release reader");
    drop(reader);

    let replay = storage
        .worker_outbound_queue(config())
        .expect("queue")
        .acknowledge(&authority, enqueue_request.message_id(), &at(7))
        .expect("ack replay completes secure checkpoint");
    assert!(replay.replayed);
    assert_eq!(replay.settlement, WorkerOutboundSettlement::Acknowledged);
    let enqueue_replay = storage
        .worker_outbound_queue(config())
        .expect("queue")
        .enqueue(&enqueue_request)
        .expect("settled exact enqueue replay");
    assert!(enqueue_replay.replayed);
    assert_eq!(
        enqueue_replay.settlement,
        Some(WorkerOutboundSettlement::Acknowledged)
    );
    assert_eq!(
        storage
            .worker_outbound_queue(config())
            .expect("queue")
            .enqueue(&request(&authority, 200, b"different-body"))
            .expect_err("changed settled body")
            .code(),
        WorkerOutboundQueueErrorCode::MessageConflict
    );

    let database_path = storage.database_path().to_path_buf();
    let connection = Connection::open(&database_path).expect("inspect tombstone");
    let active: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM internal_worker_outbound_messages",
            [],
            |row| row.get(0),
        )
        .expect("active count");
    let tombstone: (String, String) = connection
        .query_row(
            "SELECT payload_digest, settlement FROM internal_worker_outbound_settlements WHERE message_id = ?1",
            [&id("xmsg", 200)],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("tombstone");
    assert_eq!(active, 0);
    assert!(tombstone.0.starts_with("sha256:"));
    assert_eq!(tombstone.1, "acknowledged");
    drop(connection);
    Box::new(storage).close().expect("close");
    assert_files_exclude(&database_path, SECRET);
    assert_restricted_permissions(&database_path);
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn terminal_settlement_clears_every_raw_frame_and_replays_without_body() {
    const SECRET_ONE: &[u8] = b"private-approval-reason-55001";
    const SECRET_TWO: &[u8] = b"private-input-response-55002";
    let root = temporary_directory("terminal-cleanup");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let authority = prepare_authority(&mut storage, 4);
    let one = request(&authority, 300, SECRET_ONE);
    let two = request(&authority, 301, SECRET_TWO);
    {
        let mut queue = storage.worker_outbound_queue(config()).expect("queue");
        queue.enqueue(&one).expect("one");
        queue.enqueue(&two).expect("two");
    }
    storage
        .worker_session_slots()
        .expect("slots")
        .close(&WorkerSlotCloseRequest {
            authority: authority.slot.clone(),
            request_id: RequestId(id("req", 302)),
            expected_revision: 1,
            outcome: WorkerSlotState::Completed,
            closed_at: at(8),
        })
        .expect("close slot");
    let cleared = storage
        .worker_outbound_queue(config())
        .expect("queue")
        .settle_terminal(&authority, &at(8))
        .expect("terminal cleanup");
    assert_eq!(cleared, 2);
    let replay = storage
        .worker_outbound_queue(config())
        .expect("queue")
        .enqueue(&one)
        .expect("terminal tombstone replay");
    assert_eq!(replay.settlement, Some(WorkerOutboundSettlement::Terminal));
    assert_eq!(
        storage
            .worker_outbound_queue(config())
            .expect("queue")
            .enqueue(&request(&authority, 300, b"changed-terminal-body"))
            .expect_err("terminal changed body")
            .code(),
        WorkerOutboundQueueErrorCode::MessageConflict
    );
    let database_path = storage.database_path().to_path_buf();
    Box::new(storage).close().expect("close");
    assert_files_exclude(&database_path, SECRET_ONE);
    assert_files_exclude(&database_path, SECRET_TWO);
    fs::remove_dir_all(root).expect("remove fixture");
}

fn assert_files_exclude(database_path: &std::path::Path, needle: &[u8]) {
    for path in [
        database_path.to_path_buf(),
        PathBuf::from(format!("{}-wal", database_path.display())),
        PathBuf::from(format!("{}-shm", database_path.display())),
    ] {
        if let Ok(bytes) = fs::read(&path) {
            assert!(
                !bytes.windows(needle.len()).any(|window| window == needle),
                "{} retained the raw interaction fixture",
                path.display()
            );
        }
    }
}

fn assert_restricted_permissions(database_path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let file_mode = fs::metadata(database_path)
            .expect("database metadata")
            .permissions()
            .mode()
            & 0o777;
        let directory_mode = fs::metadata(database_path.parent().expect("parent"))
            .expect("directory metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
        assert_eq!(directory_mode, 0o700);
    }
}

fn renewal_frame(
    authority: &WorkerOutboundAuthority,
) -> (ExecutionLeaseRenewal, WorkerOutboundEnqueueRequest) {
    let mut extended = authority.clone();
    extended.lease_expires_at = at(30);
    let frame = request(&extended, 900, b"encoded-renewal-frame");
    let slot = &authority.slot;
    let renewal = ExecutionLeaseRenewal {
        expires_at: at(30),
        prior_expires_at: authority.lease_expires_at.clone(),
        fencing_token: slot.fencing_token.clone(),
        job_id: slot.job_id.clone(),
        lease_id: slot.lease_id.clone(),
        message_id: frame.message_id().clone(),
        request_id: RequestId(id("req", 900)),
        sent_at: at(6),
        worker_id: slot.worker_id.clone(),
        worker_instance_id: slot.worker_instance_id.clone(),
        attempt: slot.attempt,
    };
    (renewal, frame)
}

#[test]
fn renewal_and_control_frame_commit_and_replay_together_after_restart() {
    let root = temporary_directory("atomic-renewal");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let authority = prepare_authority(&mut storage, 1);
    let (renewal, frame) = renewal_frame(&authority);
    let receipt = storage
        .worker_outbound_queue(config())
        .expect("queue")
        .renew_lease_and_enqueue(&renewal, &frame)
        .expect("atomic renewal");
    assert_eq!(receipt.status, LeaseWriteStatus::Accepted);
    assert_eq!(receipt.lease.expect("lease").expires_at, at(30));
    Box::new(storage).close().expect("close");
    let mut storage = SqliteStorage::open(&root).expect("restart");
    let mut queue = storage.worker_outbound_queue(config()).expect("queue");
    assert_eq!(
        queue
            .renew_lease_and_enqueue(&renewal, &frame)
            .expect("replay")
            .status,
        LeaseWriteStatus::Duplicate
    );
    let changed = request(frame.authority(), 900, b"different-frame");
    assert_eq!(
        queue
            .renew_lease_and_enqueue(&renewal, &changed)
            .expect_err("changed replay")
            .code(),
        WorkerOutboundQueueErrorCode::MessageConflict
    );
    let page = queue
        .claim_page(frame.authority(), &at(21), None, 1)
        .expect("claim beyond old expiry");
    assert_eq!(page.claims.len(), 1);
    assert_eq!(page.claims[0].frame_bytes(), b"encoded-renewal-frame");
    assert!(page.next_cursor.is_none());
    queue
        .acknowledge(frame.authority(), frame.message_id(), &at(22))
        .expect("ack");
    assert_eq!(
        queue
            .renew_lease_and_enqueue(&renewal, &frame)
            .expect("settled replay")
            .status,
        LeaseWriteStatus::Duplicate
    );
    assert!(
        queue
            .claim_page(frame.authority(), &at(23), None, 1)
            .expect("empty")
            .claims
            .is_empty()
    );
    Box::new(storage).close().expect("close");
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn renewal_queue_failure_rolls_back_lease_and_receipt() {
    let root = temporary_directory("renewal-rollback");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let authority = prepare_authority(&mut storage, 1);
    let (renewal, frame) = renewal_frame(&authority);
    {
        let mut queue = storage.worker_outbound_queue(config()).expect("queue");
        queue
            .enqueue(&request(&authority, 100, &[b'a'; 256]))
            .expect("first frame");
        queue
            .enqueue(&request(&authority, 101, &[b'b'; 256]))
            .expect("second frame");
        assert_eq!(
            queue
                .renew_lease_and_enqueue(&renewal, &frame)
                .expect_err("full queue")
                .code(),
            WorkerOutboundQueueErrorCode::CapacityExceeded
        );
    }
    let current = storage
        .execution_registry()
        .expect("registry")
        .load_lease(&renewal.job_id)
        .expect("load")
        .expect("lease");
    assert_eq!(current.expires_at, authority.lease_expires_at);
    {
        let mut queue = storage.worker_outbound_queue(config()).expect("queue");
        let page = queue
            .claim_page(&authority, &at(7), None, 1)
            .expect("old authority still live");
        queue
            .acknowledge(&authority, page.claims[0].message_id(), &at(7))
            .expect("free capacity");
        let mut rejected = renewal.clone();
        rejected.prior_expires_at = at(19);
        assert_eq!(
            queue
                .renew_lease_and_enqueue(&rejected, &frame)
                .expect("wrong prior")
                .status,
            LeaseWriteStatus::RejectedConflict
        );
        let foreign = request(&authority, 900, b"encoded-renewal-frame");
        assert_eq!(
            queue
                .renew_lease_and_enqueue(&renewal, &foreign)
                .expect_err("wrong envelope")
                .code(),
            WorkerOutboundQueueErrorCode::InvalidInput
        );
        let mut invalid_authority = frame.authority().clone();
        invalid_authority.lease_issued_at = at(2);
        let invalid_frame = request(&invalid_authority, 900, b"encoded-renewal-frame");
        assert_eq!(
            queue
                .renew_lease_and_enqueue(&renewal, &invalid_frame)
                .expect_err("invalid authority rolls back renewal")
                .code(),
            WorkerOutboundQueueErrorCode::AuthorityMismatch
        );
        assert_eq!(
            queue
                .renew_lease_and_enqueue(&renewal, &frame)
                .expect("retry after rollback")
                .status,
            LeaseWriteStatus::Accepted
        );
    }
    Box::new(storage).close().expect("close");
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn renewal_keeps_historical_frames_claimable_and_acknowledgeable() {
    let root = temporary_directory("historical-frames");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let authority = prepare_authority(&mut storage, 1);
    let original = request(&authority, 100, b"original-approval");
    let (renewal, frame) = renewal_frame(&authority);
    {
        let mut queue = storage.worker_outbound_queue(config()).expect("queue");
        queue.enqueue(&original).expect("original");
        queue
            .claim_page(&authority, &at(6), None, 1)
            .expect("first delivery");
        queue
            .renew_lease_and_enqueue(&renewal, &frame)
            .expect("renew");
        assert_eq!(
            queue
                .enqueue(&request(frame.authority(), 901, b"extra"))
                .expect_err("renewal cannot reset per-attempt capacity")
                .code(),
            WorkerOutboundQueueErrorCode::CapacityExceeded
        );
    }
    Box::new(storage).close().expect("close");
    let mut storage = SqliteStorage::open(&root).expect("restart");
    let mut queue = storage.worker_outbound_queue(config()).expect("queue");
    let page = queue
        .claim_page(frame.authority(), &at(21), None, 1)
        .expect("historical page");
    assert_eq!(page.claims[0].frame_bytes(), b"original-approval");
    assert_eq!(page.claims[0].authority(), &authority);
    assert!(page.claims[0].replayed());
    let next = queue
        .claim_page(frame.authority(), &at(21), page.next_cursor.as_ref(), 1)
        .expect("current page");
    assert_eq!(next.claims[0].message_id(), frame.message_id());
    assert_eq!(next.claims[0].authority(), frame.authority());
    assert!(next.next_cursor.is_none());
    assert_eq!(
        queue
            .claim_page(&authority, &at(21), None, 1)
            .unwrap()
            .claims[0]
            .frame_bytes(),
        b"original-approval"
    );
    let mut invented = frame.authority().clone();
    invented.lease_expires_at = at(25);
    assert!(
        queue
            .acknowledge(&invented, original.message_id(), &at(21))
            .is_err()
    );
    queue
        .acknowledge(frame.authority(), original.message_id(), &at(21))
        .expect("ack old frame");
    assert!(
        queue
            .acknowledge(frame.authority(), original.message_id(), &at(22))
            .expect("ack replay")
            .replayed
    );
    assert_eq!(
        queue
            .enqueue(&original)
            .expect("old exact replay after ack")
            .settlement,
        Some(WorkerOutboundSettlement::Acknowledged)
    );
    Box::new(storage).close().expect("close");
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn terminal_cleanup_clears_frames_from_every_accepted_lease_period() {
    let root = temporary_directory("historical-terminal");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let authority = prepare_authority(&mut storage, 1);
    let original = request(&authority, 100, b"original-approval");
    let (renewal, frame) = renewal_frame(&authority);
    {
        let mut queue = storage.worker_outbound_queue(config()).expect("queue");
        queue.enqueue(&original).expect("original");
        queue
            .renew_lease_and_enqueue(&renewal, &frame)
            .expect("renew");
    }
    storage
        .worker_session_slots()
        .expect("slots")
        .close(&WorkerSlotCloseRequest {
            authority: authority.slot.clone(),
            request_id: RequestId(id("req", 902)),
            expected_revision: 1,
            outcome: WorkerSlotState::Completed,
            closed_at: at(21),
        })
        .expect("close slot");
    let mut queue = storage.worker_outbound_queue(config()).expect("queue");
    assert_eq!(
        queue
            .settle_terminal(&authority, &at(21))
            .expect("all periods"),
        2
    );
    for request in [&original, &frame] {
        assert_eq!(
            queue.enqueue(request).expect("settled replay").settlement,
            Some(WorkerOutboundSettlement::Terminal)
        );
    }
    assert_eq!(
        queue
            .settle_terminal(frame.authority(), &at(22))
            .expect("cleanup replay"),
        0
    );
    Box::new(storage).close().expect("close");
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn retained_delivery_confirmation_and_cleanup_survive_expiry_and_restart() {
    let root = temporary_directory("late-outbound-ack");
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let authority = prepare_authority(&mut storage, 1);
    let first = request(&authority, 1800, b"retained-before-expiry");
    let second = WorkerOutboundEnqueueRequest::new(
        authority.clone(),
        ExecutionMessageId(id("xmsg", 1801)),
        at(55),
        b"late-delivery".to_vec(),
    )
    .unwrap();
    {
        let mut queue = storage
            .worker_outbound_queue(WorkerOutboundQueueConfig {
                max_claim_page_size: 2,
                ..config()
            })
            .unwrap();
        queue.enqueue(&first).unwrap();
        queue
            .enqueue(&second)
            .expect("transport retention has no execution deadline");
        let page = queue.claim_page(&authority, &at(56), None, 2).unwrap();
        assert_eq!(page.claims.len(), 2);
        assert_eq!(page.claims[0].frame_bytes(), b"retained-before-expiry");
        assert!(
            queue
                .confirm_remote(
                    &authority.slot.worker_id,
                    &authority.slot.worker_instance_id,
                    first.message_id(),
                    &at(57)
                )
                .unwrap()
        );
    }
    Box::new(storage).close().unwrap();
    let mut storage = SqliteStorage::open(&root).unwrap();
    {
        let mut queue = storage
            .worker_outbound_queue(WorkerOutboundQueueConfig {
                max_claim_page_size: 2,
                ..config()
            })
            .unwrap();
        let progress = queue
            .retry_confirmed(
                &authority.slot.worker_id,
                &authority.slot.worker_instance_id,
            )
            .unwrap();
        assert_eq!(progress.acknowledgements.len(), 1);
        assert!(progress.retry_error.is_none());
        queue
            .acknowledge(&authority, second.message_id(), &at(59))
            .unwrap();
        assert!(
            queue
                .claim_page(&authority, &at(59), None, 2)
                .unwrap()
                .claims
                .is_empty()
        );
    }
    Box::new(storage).close().unwrap();
    fs::remove_dir_all(root).unwrap();
}
