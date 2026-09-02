// SPDX-License-Identifier: Apache-2.0

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

use winwincode_domain::{
    ControlPlaneEventId, ExecutionJobId, ExecutionMessageId, FencingToken, Instant, LeaseId,
    OrganizationId, ProjectId, RepositoryId, RequestId, Sha256Digest, SystemActorId, WorkerId,
    WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_storage::{
    DispatchResultRequest, DispatchResultStatus, EXECUTION_PROTOCOL_VERSION, ExecutionLeaseClaim,
    ExecutionLeaseRenewal, LeaseWriteStatus, NewOutboxEvent, ProjectionEventStream,
    PublicEventActor, PublicEventScope, PublicEventSource, SqliteStorage, StorageErrorKind,
    WorkerAuthenticationIdentity, WorkerManagementCommand, WorkerManagementState,
    WorkerOperationalState, WorkerPlatform, WorkerRegistrationRequest, WorkerRegistryScope,
    public_receipt_identity,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-worker-management-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn instant(second: u64) -> Instant {
    Instant(format!("2027-01-15T08:00:{second:02}.000Z"))
}

fn scope(seed: u64) -> WorkerRegistryScope {
    WorkerRegistryScope::Repository {
        organization_id: OrganizationId(id("org", seed)),
        workspace_id: WorkspaceId(id("wsp", seed)),
        project_id: ProjectId(id("prj", seed)),
        repository_id: RepositoryId(id("rep", seed)),
    }
}

fn public_scope(scope: &WorkerRegistryScope) -> PublicEventScope {
    let WorkerRegistryScope::Repository {
        organization_id,
        workspace_id,
        project_id,
        repository_id,
    } = scope
    else {
        panic!("fixture scope must be repository scoped");
    };
    PublicEventScope::Repository {
        organization_id: organization_id.clone(),
        workspace_id: workspace_id.clone(),
        project_id: project_id.clone(),
        repository_id: repository_id.clone(),
    }
}

fn actor() -> PublicEventActor {
    PublicEventActor::System {
        id: SystemActorId(id("sys", 1)),
    }
}

fn registration(worker: u64, request: u64) -> WorkerRegistrationRequest {
    WorkerRegistrationRequest {
        authentication_identity: WorkerAuthenticationIdentity::TransportPrincipal {
            issuer: "fixture-issuer".into(),
            subject: format!("worker-{worker}"),
            credential_fingerprint: Sha256Digest(format!("sha256:{}", "f".repeat(64))),
        },
        protocol_version: EXECUTION_PROTOCOL_VERSION.into(),
        platform: WorkerPlatform::X86_64UnknownLinuxGnu,
        capabilities: vec!["artifact_stream".into(), "shell".into()],
        capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        security_zone: "build-local".into(),
        max_slots: 4,
        message_id: ExecutionMessageId(id("xmsg", request)),
        request_id: RequestId(id("req", request)),
        sent_at: instant(1),
        started_at: instant(0),
        worker_id: WorkerId(id("wrk", worker)),
        worker_instance_id: WorkerInstanceId(id("wki", worker)),
    }
}

fn management_command(
    scope: &WorkerRegistryScope,
    worker: u64,
    request: u64,
    expected_revision: u64,
    target_state: WorkerManagementState,
    digest_byte: char,
    occurred_at: Instant,
) -> WorkerManagementCommand {
    let public_scope = public_scope(scope);
    let public_actor = actor();
    WorkerManagementCommand {
        receipt_identity: public_receipt_identity(
            &public_actor,
            &public_scope,
            RequestId(id("req", request)),
        )
        .expect("receipt identity"),
        command_digest: Sha256Digest(format!("sha256:{}", digest_byte.to_string().repeat(64))),
        scope: scope.clone(),
        worker_id: WorkerId(id("wrk", worker)),
        expected_revision,
        target_state,
        occurred_at: occurred_at.clone(),
        public_event: NewOutboxEvent::public_projection(
            ControlPlaneEventId(id("evt", request)),
            "worker-health.changed.v1",
            br#"{"type":"worker-health.changed.v1"}"#.to_vec(),
            ProjectionEventStream::Scope,
            public_scope,
            occurred_at,
            PublicEventSource::ControlPlane {
                actor: public_actor,
                component: "worker-management-test".into(),
            },
        )
        .expect("public event"),
    }
}

fn claim(
    worker: u64,
    job: u64,
    request: u64,
    issued_second: u64,
    expires_second: u64,
) -> ExecutionLeaseClaim {
    ExecutionLeaseClaim {
        expires_at: instant(expires_second),
        fencing_token: FencingToken(request.to_string()),
        issued_at: instant(issued_second),
        job_id: ExecutionJobId(id("job", job)),
        lease_id: LeaseId(id("lse", request)),
        message_id: ExecutionMessageId(id("xmsg", request)),
        payload_digest: Sha256Digest(format!("sha256:{}", "c".repeat(64))),
        request_id: RequestId(id("req", request)),
        worker_id: WorkerId(id("wrk", worker)),
        worker_instance_id: WorkerInstanceId(id("wki", worker)),
        attempt: 1,
    }
}

fn renewal(lease: &ExecutionLeaseClaim, request: u64) -> ExecutionLeaseRenewal {
    ExecutionLeaseRenewal {
        expires_at: instant(7),
        fencing_token: lease.fencing_token.clone(),
        job_id: lease.job_id.clone(),
        lease_id: lease.lease_id.clone(),
        message_id: ExecutionMessageId(id("xmsg", request)),
        prior_expires_at: lease.expires_at.clone(),
        request_id: RequestId(id("req", request)),
        sent_at: instant(2),
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
        attempt: lease.attempt,
    }
}

fn dispatch_result(lease: &ExecutionLeaseClaim, request: u64) -> DispatchResultRequest {
    DispatchResultRequest {
        checked_at: instant(3),
        expires_at: instant(7),
        fencing_token: lease.fencing_token.clone(),
        issued_at: lease.issued_at.clone(),
        job_id: lease.job_id.clone(),
        lease_id: lease.lease_id.clone(),
        message_id: ExecutionMessageId(id("xmsg", request)),
        payload_digest: lease.payload_digest.clone(),
        request_id: RequestId(id("req", request)),
        sent_at: instant(3),
        status: DispatchResultStatus::Accepted,
        attempt: lease.attempt,
        error: None,
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
        worker_session_id: Some(WorkerSessionId(id("wsn", request))),
    }
}

#[test]
fn drain_enable_replay_scope_and_restart_share_one_registry_authority() {
    let root = temporary_directory("lifecycle");
    let worker_scope = scope(1);
    let existing_lease = claim(1, 1, 10, 1, 5);
    {
        let mut storage = SqliteStorage::open(&root).expect("storage open");
        let mut registry = storage.execution_registry().expect("registry");
        registry
            .register_worker_for_scope(&registration(1, 1), &worker_scope)
            .expect("registration");
        assert_eq!(
            registry
                .claim_execution_job(&existing_lease)
                .expect("existing claim")
                .status,
            LeaseWriteStatus::Accepted
        );
        let drain = management_command(
            &worker_scope,
            1,
            20,
            0,
            WorkerManagementState::Draining,
            'd',
            instant(2),
        );
        let first = registry.manage_worker(&drain).expect("drain");
        assert!(!first.replayed);
        assert_eq!(first.previous_revision, 0);
        assert_eq!(first.worker.revision, 1);
        assert_eq!(
            first.worker.operational_state,
            WorkerOperationalState::Draining
        );
        assert_eq!(first.worker.available_capacity, 0);
        assert_eq!(first.worker.active_lease_count, 1);

        let replay = registry.manage_worker(&drain).expect("drain replay");
        assert!(replay.replayed);
        assert_eq!(replay.worker, first.worker);

        let changed = management_command(
            &worker_scope,
            1,
            20,
            0,
            WorkerManagementState::Draining,
            'e',
            instant(2),
        );
        assert_eq!(
            registry
                .manage_worker(&changed)
                .expect_err("changed command body")
                .kind(),
            StorageErrorKind::RequestConflict
        );
        assert_eq!(
            registry
                .claim_execution_job(&claim(1, 2, 21, 2, 6))
                .expect("draining claim")
                .status,
            LeaseWriteStatus::RejectedConflict
        );
        assert_eq!(
            registry
                .renew_execution_lease(&renewal(&existing_lease, 22))
                .expect("existing renewal")
                .status,
            LeaseWriteStatus::Accepted
        );
        assert_eq!(
            registry
                .record_dispatch_result(&dispatch_result(&existing_lease, 23))
                .expect("existing completion")
                .status,
            DispatchResultStatus::Accepted
        );
        assert!(
            registry
                .load_managed_worker(&scope(2), &WorkerId(id("wrk", 1)), &instant(3))
                .expect("foreign scope read")
                .is_none()
        );
    }

    let mut reopened = SqliteStorage::open(&root).expect("storage reopen");
    let mut registry = reopened.execution_registry().expect("registry reopen");
    let persisted = registry
        .load_managed_worker(&worker_scope, &WorkerId(id("wrk", 1)), &instant(3))
        .expect("persisted read")
        .expect("persisted Worker");
    assert_eq!(persisted.revision, 1);
    assert_eq!(
        persisted.operational_state,
        WorkerOperationalState::Draining
    );
    let stale = management_command(
        &worker_scope,
        1,
        24,
        0,
        WorkerManagementState::Enabled,
        'f',
        instant(3),
    );
    assert_eq!(
        registry
            .manage_worker(&stale)
            .expect_err("stale enable")
            .kind(),
        StorageErrorKind::RevisionConflict
    );
    let enable = management_command(
        &worker_scope,
        1,
        25,
        1,
        WorkerManagementState::Enabled,
        '1',
        instant(3),
    );
    let enabled = registry.manage_worker(&enable).expect("enable");
    assert_eq!(enabled.worker.revision, 2);
    assert_eq!(
        enabled.worker.operational_state,
        WorkerOperationalState::Enabled
    );
    assert_eq!(enabled.worker.available_capacity, 4);
    assert_eq!(
        registry
            .claim_execution_job(&claim(1, 2, 26, 3, 8))
            .expect("enabled claim")
            .status,
        LeaseWriteStatus::Accepted
    );

    drop(registry);
    drop(reopened);
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn concurrent_drain_and_claim_have_one_atomic_order() {
    let root = temporary_directory("concurrent-order");
    let worker_scope = scope(3);
    let mut setup = SqliteStorage::open(&root).expect("storage open");
    setup
        .execution_registry()
        .expect("registry")
        .register_worker_for_scope(&registration(3, 30), &worker_scope)
        .expect("registration");
    drop(setup);

    let barrier = Arc::new(Barrier::new(3));
    let drain_root = root.clone();
    let drain_scope = worker_scope.clone();
    let drain_barrier = Arc::clone(&barrier);
    let drain_thread = thread::spawn(move || {
        let mut storage = SqliteStorage::open(&drain_root).expect("drain storage");
        let command = management_command(
            &drain_scope,
            3,
            31,
            0,
            WorkerManagementState::Draining,
            '2',
            instant(2),
        );
        drain_barrier.wait();
        storage
            .execution_registry()
            .expect("drain registry")
            .manage_worker(&command)
            .expect("drain")
    });
    let claim_root = root.clone();
    let claim_barrier = Arc::clone(&barrier);
    let claim_thread = thread::spawn(move || {
        let mut storage = SqliteStorage::open(&claim_root).expect("claim storage");
        let request = claim(3, 3, 32, 1, 5);
        claim_barrier.wait();
        storage
            .execution_registry()
            .expect("claim registry")
            .claim_execution_job(&request)
            .expect("claim")
            .status
    });
    barrier.wait();
    let drain = drain_thread.join().expect("drain thread");
    let claim_status = claim_thread.join().expect("claim thread");
    assert_eq!(drain.worker.revision, 1);
    assert!(matches!(
        claim_status,
        LeaseWriteStatus::Accepted | LeaseWriteStatus::RejectedConflict
    ));

    let mut storage = SqliteStorage::open(&root).expect("storage reopen");
    let registry = storage.execution_registry().expect("registry reopen");
    let final_worker = registry
        .load_managed_worker(&worker_scope, &WorkerId(id("wrk", 3)), &instant(2))
        .expect("final Worker read")
        .expect("final Worker");
    assert_eq!(
        final_worker.operational_state,
        WorkerOperationalState::Draining
    );
    assert_eq!(
        final_worker.active_lease_count,
        u64::from(claim_status == LeaseWriteStatus::Accepted)
    );

    drop(registry);
    drop(storage);
    fs::remove_dir_all(root).expect("directory release");
}
