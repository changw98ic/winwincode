#![allow(clippy::too_many_arguments)]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use winwincode_domain::{
    CodexThreadId, DeliveryId, ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken,
    Instant, LeaseId, OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId,
    Sha256Digest, UserId, WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_storage::{
    EXECUTION_PROTOCOL_VERSION, ExecutionAdmissionBoundary, ExecutionAdmissionLimits,
    ExecutionAdmissionPolicy, ExecutionLeaseClaim, ExecutionLeaseTerminalOutcome,
    ExecutionLeaseTerminalRequest, ExecutionQueueScope, ExecutionRepositoryAccess,
    ExecutionReservationRequest, ExecutionReservationStart, LeaseWriteStatus, SqliteStorage,
    WorkerAuthenticationIdentity, WorkerHeartbeatRequest, WorkerPlatform, WorkerPoolId,
    WorkerRegistrationRequest, WorkerSlotAuthority, WorkerSlotCancellation, WorkerSlotCloseRequest,
    WorkerSlotErrorCode, WorkerSlotEventAdvance, WorkerSlotOpenRequest, WorkerSlotRecoveryAction,
    WorkerSlotRecoveryRequest, WorkerSlotResourceLimits, WorkerSlotResources, WorkerSlotState,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-worker-session-slots-{name}-{}-{suffix}",
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
        delivery_id: Some(DeliveryId(id("dlv", seed))),
        product_session_id: ProductSessionId(id("psn", seed)),
    }
}

fn pool(seed: u64) -> WorkerPoolId {
    WorkerPoolId(id("wpl", seed))
}

fn registration(
    worker: u64,
    instance: u64,
    request: u64,
    max_slots: u64,
    started_second: u64,
) -> WorkerRegistrationRequest {
    WorkerRegistrationRequest {
        authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
            control_plane_principal: "fixture-control-plane".into(),
        },
        protocol_version: EXECUTION_PROTOCOL_VERSION.into(),
        platform: WorkerPlatform::Aarch64AppleDarwin,
        capabilities: vec!["codex".into(), "artifact".into()],
        capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        security_zone: "local".into(),
        max_slots,
        message_id: ExecutionMessageId(id("xmsg", request)),
        request_id: RequestId(id("req", request)),
        sent_at: at(started_second + 1),
        started_at: at(started_second),
        worker_id: WorkerId(id("wrk", worker)),
        worker_instance_id: WorkerInstanceId(id("wki", instance)),
    }
}

fn heartbeat(
    worker: u64,
    instance: u64,
    request: u64,
    max_slots: u64,
    observed_second: u64,
) -> WorkerHeartbeatRequest {
    WorkerHeartbeatRequest {
        active_leases: Vec::new(),
        available_slots: max_slots,
        heartbeat_sequence: ExecutionSequence(1),
        max_slots,
        running_slots: 0,
        message_id: ExecutionMessageId(id("xmsg", request)),
        observed_at: at(observed_second),
        sent_at: at(observed_second),
        worker_id: WorkerId(id("wrk", worker)),
        worker_instance_id: WorkerInstanceId(id("wki", instance)),
    }
}

fn lease(
    worker: u64,
    instance: u64,
    job: u64,
    request: u64,
    attempt: u64,
    fence: u64,
    issued_second: u64,
    expires_second: u64,
) -> ExecutionLeaseClaim {
    ExecutionLeaseClaim {
        expires_at: at(expires_second),
        fencing_token: FencingToken(fence.to_string()),
        issued_at: at(issued_second),
        job_id: ExecutionJobId(id("job", job)),
        lease_id: LeaseId(id("lse", request)),
        message_id: ExecutionMessageId(id("xmsg", request)),
        payload_digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
        request_id: RequestId(id("req", request)),
        worker_id: WorkerId(id("wrk", worker)),
        worker_instance_id: WorkerInstanceId(id("wki", instance)),
        attempt,
    }
}

fn admission_boundaries(
    request_scope: &ExecutionQueueScope,
    worker_pool: &WorkerPoolId,
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
            worker_pool_id: worker_pool.clone(),
        },
    ]
}

fn prepare_admission(
    storage: &mut SqliteStorage,
    request_scope: &ExecutionQueueScope,
    worker_pool: &WorkerPoolId,
    jobs: &[u64],
) {
    let mut admission = storage.execution_admission().expect("admission open");
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 20,
        max_queued: 20,
        token_budget: 100_000,
        cost_budget_microunits: 1_000_000,
        max_runtime_millis: 60_000,
    };
    for boundary in admission_boundaries(request_scope, worker_pool) {
        admission
            .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
            .expect("policy configure");
    }
    for job in jobs {
        let reservation = ExecutionReservationRequest {
            scope: request_scope.clone(),
            user_id: UserId(id("usr", *job)),
            worker_pool_id: worker_pool.clone(),
            job_id: ExecutionJobId(id("job", *job)),
            request_id: RequestId(id("req", 1_000 + *job)),
            repository_access: ExecutionRepositoryAccess::ReadOnly,
            reserved_tokens: 100,
            reserved_cost_microunits: 1_000,
            runtime_limit_millis: 30_000,
            submitted_at: at(3),
        };
        admission.reserve(&reservation).expect("admission reserve");
        admission
            .start(&ExecutionReservationStart {
                scope: request_scope.clone(),
                worker_pool_id: worker_pool.clone(),
                job_id: reservation.job_id,
                request_id: RequestId(id("req", 2_000 + *job)),
                expected_revision: 1,
                started_at: at(4),
            })
            .expect("admission start");
    }
}

fn prepare_worker_and_leases(
    storage: &mut SqliteStorage,
    worker: u64,
    instance: u64,
    max_slots: u64,
    jobs: &[u64],
) -> Vec<ExecutionLeaseClaim> {
    let mut registry = storage.execution_registry().expect("registry open");
    registry
        .register_worker(&registration(worker, instance, 10 + instance, max_slots, 0))
        .expect("Worker registration");
    assert_eq!(
        registry
            .record_heartbeat(&heartbeat(worker, instance, 20 + instance, max_slots, 2))
            .expect("Worker heartbeat")
            .status,
        LeaseWriteStatus::Accepted
    );
    jobs.iter()
        .enumerate()
        .map(|(index, job)| {
            let request = lease(
                worker,
                instance,
                *job,
                100 + u64::try_from(index).expect("small lease index"),
                1,
                1,
                3,
                20,
            );
            assert_eq!(
                registry
                    .claim_execution_job(&request)
                    .expect("lease claim")
                    .status,
                LeaseWriteStatus::Accepted
            );
            request
        })
        .collect()
}

fn authority(
    claim: &ExecutionLeaseClaim,
    worker_session: u64,
    codex_thread: u64,
) -> WorkerSlotAuthority {
    WorkerSlotAuthority {
        worker_id: claim.worker_id.clone(),
        worker_instance_id: claim.worker_instance_id.clone(),
        worker_session_id: WorkerSessionId(id("wsn", worker_session)),
        codex_thread_id: CodexThreadId(id("cdx", codex_thread)),
        job_id: claim.job_id.clone(),
        lease_id: claim.lease_id.clone(),
        attempt: claim.attempt,
        fencing_token: claim.fencing_token.clone(),
    }
}

fn open_request(
    authority: WorkerSlotAuthority,
    request: u64,
    resources: WorkerSlotResources,
) -> WorkerSlotOpenRequest {
    WorkerSlotOpenRequest {
        authority,
        resources,
        request_id: RequestId(id("req", request)),
        opened_at: at(5),
    }
}

fn resource_limits() -> WorkerSlotResourceLimits {
    WorkerSlotResourceLimits {
        max_memory_bytes: 300,
        max_disk_bytes: 300,
        max_processes: 3,
    }
}

#[test]
fn concurrent_slot_open_never_exceeds_worker_capacity() {
    let root = temporary_directory("atomic-capacity");
    let request_scope = scope(9);
    let worker_pool = pool(9);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    prepare_admission(&mut storage, &request_scope, &worker_pool, &[90, 91]);
    let leases = prepare_worker_and_leases(&mut storage, 9, 9, 1, &[90, 91]);
    let resources = WorkerSlotResources {
        memory_bytes: 50,
        disk_bytes: 50,
        process_slots: 1,
    };
    {
        let first = authority(&leases[0], 90, 90);
        storage
            .worker_session_slots()
            .expect("slots open")
            .configure_resources(
                &first.worker_id,
                &first.worker_instance_id,
                resource_limits(),
            )
            .expect("resource limits");
    }
    drop(storage);

    let barrier = Arc::new(Barrier::new(3));
    let handles = leases
        .iter()
        .enumerate()
        .map(|(index, lease)| {
            let root = root.clone();
            let barrier = Arc::clone(&barrier);
            let seed = 90 + u64::try_from(index).expect("small slot index");
            let request = open_request(authority(lease, seed, seed), 600 + seed, resources);
            thread::spawn(move || {
                let mut storage = SqliteStorage::open(root).expect("thread storage");
                let mut slots = storage.worker_session_slots().expect("thread slots");
                barrier.wait();
                slots
                    .open(&request)
                    .map(|receipt| receipt.slot.authority.worker_session_id)
                    .map_err(|error| error.code())
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let results = handles
        .into_iter()
        .map(|handle| handle.join().expect("slot thread"))
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(WorkerSlotErrorCode::CapacityExhausted)))
            .count(),
        1
    );

    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn multiple_sessions_keep_capacity_events_and_cancellation_isolated() {
    let root = temporary_directory("isolation");
    let request_scope = scope(1);
    let worker_pool = pool(1);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    prepare_admission(&mut storage, &request_scope, &worker_pool, &[1, 2, 3]);
    let leases = prepare_worker_and_leases(&mut storage, 1, 1, 2, &[1, 2, 3]);
    let first_authority = authority(&leases[0], 1, 1);
    let second_authority = authority(&leases[1], 2, 2);
    let third_authority = authority(&leases[2], 3, 3);
    let resources = WorkerSlotResources {
        memory_bytes: 50,
        disk_bytes: 50,
        process_slots: 1,
    };
    let mut slots = storage.worker_session_slots().expect("slots open");
    slots
        .configure_resources(
            &first_authority.worker_id,
            &first_authority.worker_instance_id,
            resource_limits(),
        )
        .expect("resource limits");
    slots
        .open(&open_request(first_authority.clone(), 300, resources))
        .expect("first slot");
    slots
        .open(&open_request(second_authority.clone(), 301, resources))
        .expect("second slot");
    let full = slots
        .open(&open_request(third_authority.clone(), 302, resources))
        .expect_err("Worker full");
    assert_eq!(full.code(), WorkerSlotErrorCode::CapacityExhausted);
    assert_eq!(
        slots
            .capacity(
                &first_authority.worker_id,
                &first_authority.worker_instance_id
            )
            .expect("full capacity")
            .available_slots,
        0
    );

    let advanced = slots
        .advance_event_cursor(&WorkerSlotEventAdvance {
            authority: first_authority.clone(),
            request_id: RequestId(id("req", 303)),
            expected_cursor: 0,
            next_cursor: 1,
            observed_at: at(6),
        })
        .expect("first event");
    assert_eq!(advanced.slot.event_cursor, 1);
    let cancelled = slots
        .request_cancellation(&WorkerSlotCancellation {
            authority: first_authority.clone(),
            request_id: RequestId(id("req", 304)),
            expected_revision: 2,
            requested_at: at(7),
        })
        .expect("first cancellation");
    assert_eq!(cancelled.slot.state, WorkerSlotState::Cancelling);
    let second = slots
        .load(&second_authority.worker_session_id)
        .expect("second load")
        .expect("second slot");
    assert_eq!(second.state, WorkerSlotState::Running);
    assert_eq!(second.event_cursor, 0);
    assert_eq!(second.revision, 1);

    slots
        .close(&WorkerSlotCloseRequest {
            authority: first_authority,
            request_id: RequestId(id("req", 305)),
            expected_revision: 3,
            outcome: WorkerSlotState::Cancelled,
            closed_at: at(8),
        })
        .expect("cancel acknowledgement");
    slots
        .open(&open_request(third_authority, 306, resources))
        .expect("slot after release");

    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn configured_local_resources_block_overcommit_without_consuming_a_slot() {
    let root = temporary_directory("resources");
    let request_scope = scope(2);
    let worker_pool = pool(2);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    prepare_admission(&mut storage, &request_scope, &worker_pool, &[10, 11]);
    let leases = prepare_worker_and_leases(&mut storage, 2, 2, 3, &[10, 11]);
    let first = authority(&leases[0], 10, 10);
    let second = authority(&leases[1], 11, 11);
    let mut slots = storage.worker_session_slots().expect("slots open");
    let limits = WorkerSlotResourceLimits {
        max_memory_bytes: 100,
        max_disk_bytes: 100,
        max_processes: 3,
    };
    slots
        .configure_resources(&first.worker_id, &first.worker_instance_id, limits)
        .expect("resource limits");
    let resources = WorkerSlotResources {
        memory_bytes: 60,
        disk_bytes: 10,
        process_slots: 1,
    };
    slots
        .open(&open_request(first.clone(), 400, resources))
        .expect("first slot");
    assert_eq!(
        slots
            .open(&open_request(second, 401, resources))
            .expect_err("memory exhausted")
            .code(),
        WorkerSlotErrorCode::ResourceExhausted
    );
    let capacity = slots
        .capacity(&first.worker_id, &first.worker_instance_id)
        .expect("capacity");
    assert_eq!(capacity.running_slots, 1);
    assert_eq!(capacity.available_slots, 2);
    assert_eq!(capacity.reserved.memory_bytes, 60);

    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn terminal_lease_cannot_open_a_new_worker_session_slot() {
    let root = temporary_directory("terminal-lease");
    let request_scope = scope(12);
    let worker_pool = pool(12);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    prepare_admission(&mut storage, &request_scope, &worker_pool, &[120]);
    let lease = prepare_worker_and_leases(&mut storage, 12, 12, 1, &[120])
        .pop()
        .expect("lease");
    storage
        .execution_registry()
        .expect("registry")
        .finish_execution_lease(&ExecutionLeaseTerminalRequest {
            job_id: lease.job_id.clone(),
            lease_id: lease.lease_id.clone(),
            worker_id: lease.worker_id.clone(),
            worker_instance_id: lease.worker_instance_id.clone(),
            attempt: lease.attempt,
            fencing_token: lease.fencing_token.clone(),
            outcome: ExecutionLeaseTerminalOutcome::Completed,
            terminal_at: at(5),
            request_id: RequestId(id("req", 512)),
        })
        .expect("terminal lease");
    {
        let authority = authority(&lease, 120, 120);
        let mut slots = storage.worker_session_slots().expect("slots open");
        slots
            .configure_resources(
                &authority.worker_id,
                &authority.worker_instance_id,
                resource_limits(),
            )
            .expect("resource limits");
        assert_eq!(
            slots
                .open(&open_request(
                    authority,
                    513,
                    WorkerSlotResources {
                        memory_bytes: 1,
                        disk_bytes: 1,
                        process_slots: 1,
                    },
                ))
                .expect_err("terminal lease cannot mint a new slot")
                .code(),
            WorkerSlotErrorCode::LeaseMismatch
        );
    }
    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
#[allow(clippy::too_many_lines)]
fn restart_recovers_only_slots_with_new_exact_lease_and_fails_the_rest() {
    let root = temporary_directory("restart");
    let request_scope = scope(3);
    let worker_pool = pool(3);
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    prepare_admission(&mut storage, &request_scope, &worker_pool, &[20, 21]);
    let old_leases = prepare_worker_and_leases(&mut storage, 3, 30, 2, &[20, 21]);
    let first = authority(&old_leases[0], 20, 20);
    let second = authority(&old_leases[1], 21, 21);
    let resources = WorkerSlotResources {
        memory_bytes: 50,
        disk_bytes: 50,
        process_slots: 1,
    };
    {
        let mut slots = storage.worker_session_slots().expect("slots open");
        slots
            .configure_resources(
                &first.worker_id,
                &first.worker_instance_id,
                resource_limits(),
            )
            .expect("old limits");
        slots
            .open(&open_request(first.clone(), 500, resources))
            .expect("first old slot");
        slots
            .open(&open_request(second.clone(), 501, resources))
            .expect("second old slot");
        slots
            .advance_event_cursor(&WorkerSlotEventAdvance {
                authority: first.clone(),
                request_id: RequestId(id("req", 502)),
                expected_cursor: 0,
                next_cursor: 1,
                observed_at: at(6),
            })
            .expect("old cursor");
    }

    let new_instance = 31;
    let new_lease = {
        let mut registry = storage.execution_registry().expect("registry reopen");
        registry
            .register_worker(&registration(3, new_instance, 510, 2, 20))
            .expect("replacement registration");
        let new_lease = lease(3, new_instance, 20, 511, 2, 2, 21, 40);
        assert_eq!(
            registry
                .claim_execution_job(&new_lease)
                .expect("replacement lease")
                .status,
            LeaseWriteStatus::Accepted
        );
        new_lease
    };
    let recovery = WorkerSlotRecoveryRequest {
        worker_id: new_lease.worker_id.clone(),
        worker_instance_id: new_lease.worker_instance_id.clone(),
        request_id: RequestId(id("req", 512)),
        recovered_at: at(22),
    };
    let mut slots = storage.worker_session_slots().expect("new slots");
    slots
        .configure_resources(
            &new_lease.worker_id,
            &new_lease.worker_instance_id,
            resource_limits(),
        )
        .expect("new limits");
    let receipt = slots
        .reconcile_restart(&recovery)
        .expect("restart reconcile");
    assert_eq!(receipt.actions.len(), 2);
    let WorkerSlotRecoveryAction::Recovered { slot: recovered } = &receipt.actions[0] else {
        panic!("first slot should recover");
    };
    assert_eq!(
        recovered.authority.worker_instance_id,
        new_lease.worker_instance_id
    );
    assert_eq!(recovered.authority.lease_id, new_lease.lease_id);
    assert_eq!(recovered.authority.fencing_token, FencingToken("2".into()));
    assert_eq!(recovered.event_cursor, 1);
    let WorkerSlotRecoveryAction::Failed { slot: failed } = &receipt.actions[1] else {
        panic!("second slot should fail explicitly");
    };
    assert_eq!(failed.state, WorkerSlotState::RecoveryFailed);
    assert!(
        slots
            .reconcile_restart(&recovery)
            .expect("recovery replay")
            .replayed
    );
    let capacity = slots
        .capacity(&new_lease.worker_id, &new_lease.worker_instance_id)
        .expect("new capacity");
    assert_eq!(capacity.running_slots, 1);
    assert_eq!(capacity.available_slots, 1);

    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}
