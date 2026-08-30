use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use rusqlite::Connection;
use winwincode_domain::{
    CodexThreadId, DeliveryId, ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken,
    Instant, LeaseId, OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId,
    Sha256Digest, UserId, WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_storage::{
    EXECUTION_PROTOCOL_VERSION, ExecutionAdmissionBoundary, ExecutionAdmissionLimits,
    ExecutionAdmissionPolicy, ExecutionLeaseClaim, ExecutionQueueScope, ExecutionRepositoryAccess,
    ExecutionReservationRequest, ExecutionReservationStart, LeaseWriteStatus, ProductStateStorage,
    SqliteStorage, WorkerAuthenticationIdentity, WorkerHeartbeatRequest, WorkerOutboundAuthority,
    WorkerOutboundEnqueueRequest, WorkerOutboundMessageState, WorkerOutboundQueueConfig,
    WorkerOutboundQueueErrorCode, WorkerOutboundSettlement, WorkerPlatform, WorkerPoolId,
    WorkerRegistrationRequest, WorkerSlotAuthority, WorkerSlotCloseRequest, WorkerSlotOpenRequest,
    WorkerSlotResourceLimits, WorkerSlotResources, WorkerSlotState,
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
        token_budget: 10_000,
        cost_budget_microunits: 100_000,
        max_runtime_millis: 60_000,
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
        reserved_tokens: 10,
        reserved_cost_microunits: 10,
        runtime_limit_millis: 30_000,
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
        assert_eq!(
            queue
                .claim_page(authority, &at(6), None, 1)
                .expect_err("unhealthy Worker cannot claim")
                .code(),
            WorkerOutboundQueueErrorCode::AuthorityMismatch
        );
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
    assert!(!first_page.claims[0].replayed());
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
    assert_eq!(replayed.claims[0].delivery_attempt(), 2);

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
