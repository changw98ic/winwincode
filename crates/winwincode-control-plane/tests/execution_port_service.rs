// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::drop_non_drop)]

//! Black-box tests for the Control Plane's narrow `ExecutionPort` service seam.
//!
//! The test exercises the service's public API and the durable
//! `ExecutionRegistry` port.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, from_value};
use winwincode_control_plane::{ExecutionPortService, ExecutionPortServiceError};
use winwincode_execution_port::generated::{
    ExecutionPortMessage, JobDispatchMessage, JobDispatchResultMessage,
    JobDispatchResultMessageStatus, WorkerHeartbeatAckMessageStatus, WorkerHeartbeatMessage,
    WorkerRegisterMessage, WorkerRegistrationResultMessageLeaseRecovery,
    WorkerRegistrationResultMessageStatus,
};
use winwincode_execution_port::transport::{
    EndpointSide, LocalWorkerAdapter, RemoteTransportAdapter, TypedFrame,
};
use winwincode_storage::{
    EXECUTION_PROTOCOL_VERSION, ExecutionJobState, ExecutionJobSubmission,
    ExecutionJobTransitionRequest, ExecutionLeaseClaim, ExecutionQueueScope, NewOutboxEvent,
    ProductStateStorage, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey, SqliteStorage,
    StateCommit, WorkerAuthenticationIdentity, WorkerPlatform, WorkerRegistrationRequest,
    WorkerRegistryScope,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-execution-port-service-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn fixture_message<T: serde::de::DeserializeOwned>(kind: &str) -> T {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .expect("canonical ExecutionPort fixture");
    fixture["messages"]
        .as_array()
        .expect("fixture messages")
        .iter()
        .find(|message| message["kind"] == kind)
        .cloned()
        .map(from_value)
        .expect("fixture kind")
        .unwrap_or_else(|error| panic!("{kind} fixture must decode: {error}"))
}

fn claim_from_dispatch(dispatch: &JobDispatchMessage) -> ExecutionLeaseClaim {
    ExecutionLeaseClaim {
        expires_at: dispatch.lease.expires_at.clone(),
        fencing_token: dispatch.lease.fencing_token.clone(),
        issued_at: dispatch.lease.issued_at.clone(),
        job_id: dispatch.lease.job_id.clone(),
        lease_id: dispatch.lease.lease_id.clone(),
        message_id: dispatch.message_id.clone(),
        payload_digest: dispatch.job.payload_digest.clone(),
        request_id: dispatch.request_id.clone(),
        worker_id: dispatch.lease.worker_id.clone(),
        worker_instance_id: dispatch.lease.worker_instance_id.clone(),
        attempt: u64::try_from(dispatch.lease.attempt).expect("positive attempt"),
    }
}

fn commit_durable_dispatch_intent(
    storage: &mut SqliteStorage,
    job: &winwincode_execution_port::generated::ExecutionJob,
) {
    let event_id = format!("execution-job:{}", job.job_id.0);
    let payload = serde_json::to_vec(job).expect("canonical job payload");
    let identity = ReceiptIdentity::new(
        ReceiptActorKey::from_encoded(b"fixture-delivery-transaction-actor".to_vec())
            .expect("fixture actor key"),
        ReceiptScopeKey::from_encoded(
            format!("fixture-delivery-transaction-scope:{}", job.job_id.0).into_bytes(),
        )
        .expect("fixture scope key"),
        winwincode_domain::RequestId(format!("req-delivery-transaction-{}", job.job_id.0)),
    )
    .expect("fixture receipt identity");
    let commit = StateCommit::new(
        identity,
        winwincode_domain::Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        format!("delivery-execution-intent:{}", job.job_id.0),
        0,
        b"{}".to_vec(),
        vec![NewOutboxEvent::internal(
            event_id,
            "execution.job.dispatch",
            payload,
        )],
    );
    storage.commit(&commit).expect("durable dispatch intent");
}

fn seed_queue_lease(
    storage: &mut SqliteStorage,
    job: &winwincode_execution_port::generated::ExecutionJob,
    claim: &ExecutionLeaseClaim,
    seed: u64,
) {
    let (product_session_id, delivery_id, stage_run_id) = match &job.scope {
        winwincode_execution_port::generated::ExecutionScope::ProductSessionExecutionScope(
            scope,
        ) => (scope.product_session_id.clone(), None, None),
        winwincode_execution_port::generated::ExecutionScope::DeliveryStageExecutionScope(
            scope,
        ) => (
            scope.product_session_id.clone(),
            Some(scope.delivery_id.clone()),
            Some(scope.stage_run_id.clone()),
        ),
    };
    let scope = ExecutionQueueScope {
        organization_id: winwincode_domain::OrganizationId(format!("org_{seed:026}")),
        workspace_id: winwincode_domain::WorkspaceId(format!("wsp_{seed:026}")),
        project_id: winwincode_domain::ProjectId(format!("prj_{seed:026}")),
        repository_id: job.workspace.repository_id.clone(),
        product_session_id,
        delivery_id,
    };
    let submitted = storage
        .execution_queue()
        .expect("queue")
        .submit(&ExecutionJobSubmission {
            scope: scope.clone(),
            job_id: job.job_id.clone(),
            request_id: winwincode_domain::RequestId(format!("req_{:026}", seed + 1)),
            payload_digest: job.payload_digest.clone(),
            dispatch_payload: serde_json::to_vec(job).expect("canonical job payload"),
            attempt: u64::try_from(job.attempt).expect("positive attempt"),
            dependencies: Vec::new(),
            stage_run_id,
            submitted_at: claim.issued_at.clone(),
        })
        .expect("queue submit");
    storage
        .execution_queue()
        .expect("queue")
        .transition(&ExecutionJobTransitionRequest {
            scope,
            job_id: job.job_id.clone(),
            request_id: winwincode_domain::RequestId(format!("req_{:026}", seed + 2)),
            expected_revision: submitted.job.revision,
            from: ExecutionJobState::Queued,
            to: ExecutionJobState::Leased,
            occurred_at: claim.issued_at.clone(),
        })
        .expect("queue lease");
}

#[test]
fn register_worker_returns_the_durable_registration_result() {
    let root = temporary_directory("register");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let mut service = ExecutionPortService::with_heartbeat_interval(
        &mut storage,
        fixture_message::<WorkerRegisterMessage>("worker.register")
            .sent_at
            .clone(),
        5_000,
    )
    .expect("heartbeat interval");
    let message: WorkerRegisterMessage = fixture_message("worker.register");

    let response = service
        .handle(ExecutionPortMessage::WorkerRegisterMessage(message.clone()))
        .expect("worker registration response");
    let ExecutionPortMessage::WorkerRegistrationResultMessage(result) = response else {
        panic!("registration must return worker.registration_result");
    };

    assert_eq!(
        result.status,
        WorkerRegistrationResultMessageStatus::Accepted
    );
    assert_eq!(
        result.lease_recovery,
        WorkerRegistrationResultMessageLeaseRecovery::NoActiveLeases
    );
    assert_eq!(result.request_id, message.request_id);
    assert_eq!(result.worker_id, message.worker_id);
    assert_eq!(result.worker_instance_id, message.worker_instance_id);
    assert_eq!(result.kind, winwincode_execution_port::generated::WorkerRegistrationResultMessageKind::WorkerRegistrationResult);

    drop(service);
    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn heartbeat_updates_durable_liveness_without_claiming_or_dispatching_a_job() {
    let root = temporary_directory("heartbeat");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let register: WorkerRegisterMessage = fixture_message("worker.register");
    let mut service = ExecutionPortService::new(&mut storage, register.sent_at.clone());
    service
        .handle(ExecutionPortMessage::WorkerRegisterMessage(
            register.clone(),
        ))
        .expect("registration response");
    let mut heartbeat: WorkerHeartbeatMessage = fixture_message("worker.heartbeat");
    heartbeat.active_leases.clear();

    let response = service
        .handle(ExecutionPortMessage::WorkerHeartbeatMessage(
            heartbeat.clone(),
        ))
        .expect("heartbeat response");
    let ExecutionPortMessage::WorkerHeartbeatAckMessage(ack) = response else {
        panic!("heartbeat must return worker.heartbeat_ack");
    };

    assert_eq!(ack.status, WorkerHeartbeatAckMessageStatus::Accepted);
    assert_eq!(ack.heartbeat_sequence, heartbeat.heartbeat_sequence);
    assert_eq!(ack.worker_id, register.worker_id);
    assert_eq!(ack.worker_instance_id, register.worker_instance_id);
    assert_eq!(ack.next_heartbeat_within_ms, 5_000);

    drop(service);
    let registry = storage.execution_registry().expect("registry reopen");
    assert_eq!(
        registry
            .load_lease(&winwincode_domain::ExecutionJobId(
                "job_00000000000000000000000003".to_owned(),
            ))
            .expect("lease read"),
        None,
        "heartbeat must not create a lease or dispatch intent",
    );
    drop(registry);
    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn registration_replay_and_changed_body_conflict_are_durable_results() {
    let root = temporary_directory("registration-replay");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let register: WorkerRegisterMessage = fixture_message("worker.register");
    let mut service = ExecutionPortService::new(&mut storage, register.sent_at.clone());

    let first = service
        .handle(ExecutionPortMessage::WorkerRegisterMessage(
            register.clone(),
        ))
        .expect("first registration");
    let replay = service
        .handle(ExecutionPortMessage::WorkerRegisterMessage(
            register.clone(),
        ))
        .expect("registration replay");
    let mut changed = register.clone();
    changed.capabilities.features.pop();
    let conflict = service
        .handle(ExecutionPortMessage::WorkerRegisterMessage(changed))
        .expect("registration conflict");

    let registration_status = |value| {
        let ExecutionPortMessage::WorkerRegistrationResultMessage(result) = value else {
            panic!("registration response variant");
        };
        result
    };
    assert_eq!(
        registration_status(first).status,
        WorkerRegistrationResultMessageStatus::Accepted
    );
    assert_eq!(
        registration_status(replay).status,
        WorkerRegistrationResultMessageStatus::Duplicate
    );
    let conflict = registration_status(conflict);
    assert_eq!(
        conflict.status,
        WorkerRegistrationResultMessageStatus::Rejected
    );
    assert_eq!(
        conflict.error.as_ref().map(|error| &error.code),
        Some(&winwincode_execution_port::generated::ExecutionPortErrorCode::CapabilityMismatch)
    );

    drop(service);
    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn claim_builds_one_dispatch_from_the_durable_lease_and_replays_exactly() {
    let root = temporary_directory("claim");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let expected: JobDispatchMessage = fixture_message("job.dispatch");
    commit_durable_dispatch_intent(&mut storage, &expected.job);
    let register: WorkerRegisterMessage = fixture_message("worker.register");
    let mut service = ExecutionPortService::new(&mut storage, register.sent_at.clone());
    service
        .handle(ExecutionPortMessage::WorkerRegisterMessage(register))
        .expect("registration response");

    let claim = claim_from_dispatch(&expected);
    let first = service
        .claim_execution_job(expected.job.clone(), claim.clone())
        .expect("first claim");
    let replay = service
        .claim_execution_job(expected.job.clone(), claim)
        .expect("claim replay");

    assert_eq!(first.kind, expected.kind);
    assert_eq!(first.job, expected.job);
    assert_eq!(first.lease, expected.lease);
    assert_eq!(first.message_id, expected.message_id);
    assert_eq!(first.request_id, expected.request_id);
    assert_eq!(
        replay, first,
        "durable claim replay must return the exact dispatch"
    );

    drop(service);
    let registry = storage.execution_registry().expect("registry reopen");
    let lease = registry
        .load_lease(&first.job.job_id)
        .expect("lease read")
        .expect("durable lease");
    assert_eq!(lease.lease_id, first.lease.lease_id);
    assert_eq!(lease.fencing_token, first.lease.fencing_token);
    drop(registry);
    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn claim_without_a_durable_execution_intent_is_rejected_before_lease_write() {
    let root = temporary_directory("claim-without-intent");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let register: WorkerRegisterMessage = fixture_message("worker.register");
    let mut service = ExecutionPortService::new(&mut storage, register.sent_at.clone());
    service
        .handle(ExecutionPortMessage::WorkerRegisterMessage(register))
        .expect("registration response");

    let expected: JobDispatchMessage = fixture_message("job.dispatch");
    let claim = claim_from_dispatch(&expected);
    let error = service
        .claim_execution_job(expected.job.clone(), claim.clone())
        .expect_err("a claim without a durable dispatch intent must fail");
    assert!(matches!(
        error,
        ExecutionPortServiceError::Storage(error)
            if error.kind() == winwincode_storage::StorageErrorKind::InvalidInput
    ));

    drop(service);
    let registry = storage.execution_registry().expect("registry reopen");
    assert_eq!(
        registry
            .load_lease(&expected.job.job_id)
            .expect("lease read"),
        None,
        "a rejected intent join must not write a lease"
    );
    assert!(
        !registry
            .has_request("claim", &expected.job.job_id, &claim.request_id)
            .expect("claim receipt read"),
        "a rejected intent join must not write a claim receipt"
    );
    drop(registry);
    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn transport_authenticated_claim_without_durable_pool_placement_fails_closed() {
    let root = temporary_directory("claim-missing-authenticated-placement");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let expected: JobDispatchMessage = fixture_message("job.dispatch");
    commit_durable_dispatch_intent(&mut storage, &expected.job);
    let claim = claim_from_dispatch(&expected);
    let register: WorkerRegisterMessage = fixture_message("worker.register");
    storage
        .execution_registry()
        .expect("registry")
        .register_worker_for_scope(
            &WorkerRegistrationRequest {
                authentication_identity: WorkerAuthenticationIdentity::TransportPrincipal {
                    issuer: "fixture-enterprise-worker".to_owned(),
                    subject: "remote-worker".to_owned(),
                    credential_fingerprint: winwincode_domain::Sha256Digest(format!(
                        "sha256:{}",
                        "a".repeat(64)
                    )),
                },
                protocol_version: EXECUTION_PROTOCOL_VERSION.to_owned(),
                platform: WorkerPlatform::Aarch64AppleDarwin,
                capabilities: vec!["codex".to_owned()],
                capability_digest: winwincode_domain::Sha256Digest(format!(
                    "sha256:{}",
                    "b".repeat(64)
                )),
                security_zone: "enterprise-default".to_owned(),
                max_slots: 1,
                message_id: register.message_id,
                request_id: register.request_id,
                sent_at: claim.issued_at.clone(),
                started_at: winwincode_domain::Instant("2026-08-24T11:59:00.000Z".to_owned()),
                worker_id: claim.worker_id.clone(),
                worker_instance_id: claim.worker_instance_id.clone(),
            },
            &WorkerRegistryScope::Repository {
                organization_id: winwincode_domain::OrganizationId(
                    "org_00000000000000000000000001".to_owned(),
                ),
                workspace_id: winwincode_domain::WorkspaceId(
                    "wsp_00000000000000000000000001".to_owned(),
                ),
                project_id: winwincode_domain::ProjectId(
                    "prj_00000000000000000000000001".to_owned(),
                ),
                repository_id: expected.job.workspace.repository_id.clone(),
            },
        )
        .expect("transport Worker registration");

    let mut service = ExecutionPortService::new(&mut storage, claim.issued_at.clone());
    let error = service
        .claim_execution_job(expected.job.clone(), claim.clone())
        .expect_err("missing authenticated placement must fail closed");
    assert!(matches!(
        error,
        ExecutionPortServiceError::AuthorityRejected("authenticated Worker placement is missing")
    ));
    drop(service);
    assert!(
        storage
            .execution_registry()
            .expect("registry")
            .load_lease(&claim.job_id)
            .expect("lease read")
            .is_none(),
        "fail-closed authenticated claim must not write a Registry lease"
    );
    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("directory release");
}

fn setup_service<'storage>(
    storage: &'storage mut SqliteStorage,
    server_time: &str,
) -> (ExecutionPortService<'storage>, JobDispatchMessage) {
    let dispatch: JobDispatchMessage = fixture_message("job.dispatch");
    commit_durable_dispatch_intent(storage, &dispatch.job);
    seed_queue_lease(storage, &dispatch.job, &claim_from_dispatch(&dispatch), 100);
    let register: WorkerRegisterMessage = fixture_message("worker.register");
    let mut service =
        ExecutionPortService::new(storage, winwincode_domain::Instant(server_time.to_owned()));
    service
        .handle(ExecutionPortMessage::WorkerRegisterMessage(register))
        .expect("registration response");
    let mut heartbeat: WorkerHeartbeatMessage = fixture_message("worker.heartbeat");
    heartbeat.active_leases.clear();
    service
        .handle(ExecutionPortMessage::WorkerHeartbeatMessage(heartbeat))
        .expect("heartbeat response");
    service
        .claim_execution_job(dispatch.job.clone(), claim_from_dispatch(&dispatch))
        .expect("lease claim");
    (service, dispatch)
}

fn dispatch_result_response(
    service: &mut ExecutionPortService<'_>,
    result: JobDispatchResultMessage,
    context: &str,
) -> JobDispatchResultMessage {
    let response = service
        .handle(ExecutionPortMessage::JobDispatchResultMessage(result))
        .expect(context);
    let ExecutionPortMessage::JobDispatchResultMessage(response) = response else {
        panic!("dispatch result response variant");
    };
    response
}

fn changed_dispatch_result(mut result: JobDispatchResultMessage) -> JobDispatchResultMessage {
    result.payload_digest = winwincode_domain::Sha256Digest(
        "sha256:1111111111111111111111111111111111111111111111111111111111111111".to_owned(),
    );
    result
}

fn assert_dispatch_result_replay_and_conflict(
    service: &mut ExecutionPortService<'_>,
    result: &JobDispatchResultMessage,
) {
    let response = dispatch_result_response(service, result.clone(), "dispatch result response");
    assert_eq!(&response, result);
    assert_eq!(response.status, JobDispatchResultMessageStatus::Accepted);
    assert!(response.error.is_none());

    let replay = dispatch_result_response(service, result.clone(), "dispatch result replay");
    assert_eq!(replay.status, JobDispatchResultMessageStatus::Duplicate);
    assert!(replay.error.is_none());

    let conflict = dispatch_result_response(
        service,
        changed_dispatch_result(result.clone()),
        "changed dispatch result conflict",
    );
    assert_eq!(conflict.status, JobDispatchResultMessageStatus::Conflict);
    assert_eq!(
        conflict.error.as_ref().map(|error| &error.code),
        Some(&winwincode_execution_port::generated::ExecutionPortErrorCode::MessageConflict)
    );
}

fn assert_dispatch_result_restart_replay_and_conflict(
    service: &mut ExecutionPortService<'_>,
    result: JobDispatchResultMessage,
) {
    let replay =
        dispatch_result_response(service, result.clone(), "dispatch result restart replay");
    assert_eq!(replay.status, JobDispatchResultMessageStatus::Duplicate);
    assert!(replay.error.is_none());

    let conflict = dispatch_result_response(
        service,
        changed_dispatch_result(result),
        "dispatch result restart conflict",
    );
    assert_eq!(conflict.status, JobDispatchResultMessageStatus::Conflict);
    assert_eq!(
        conflict.error.as_ref().map(|error| &error.code),
        Some(&winwincode_execution_port::generated::ExecutionPortErrorCode::MessageConflict)
    );
}

#[test]
fn dispatch_result_receipt_replays_as_duplicate_and_conflicts_on_changed_body_after_restart() {
    let root = temporary_directory("dispatch-result");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let dispatch: JobDispatchMessage = fixture_message("job.dispatch");
    commit_durable_dispatch_intent(&mut storage, &dispatch.job);
    seed_queue_lease(
        &mut storage,
        &dispatch.job,
        &claim_from_dispatch(&dispatch),
        200,
    );
    let register: WorkerRegisterMessage = fixture_message("worker.register");
    let mut service = ExecutionPortService::new(&mut storage, register.sent_at.clone());
    service
        .handle(ExecutionPortMessage::WorkerRegisterMessage(register))
        .expect("registration response");
    drop(service);
    let mut registry = storage.execution_registry().expect("registry open");
    registry
        .claim_execution_job(&claim_from_dispatch(&dispatch))
        .expect("lease claim");
    drop(registry);
    let mut service = ExecutionPortService::new(
        &mut storage,
        winwincode_domain::Instant("2026-08-24T12:00:01.000Z".to_owned()),
    );

    let result: JobDispatchResultMessage = fixture_message("job.dispatch_result");
    let result_request_id = result.request_id.clone();
    assert_dispatch_result_replay_and_conflict(&mut service, &result);

    drop(service);
    let registry = storage
        .execution_registry()
        .expect("registry after receipt");
    assert_eq!(
        registry
            .load_lease(&dispatch.job.job_id)
            .expect("lease read after receipt")
            .expect("durable lease after receipt")
            .fencing_token,
        dispatch.lease.fencing_token
    );
    assert!(
        registry
            .has_request("dispatch_result", &dispatch.job.job_id, &result_request_id)
            .expect("dispatch result receipt read")
    );
    drop(registry);
    Box::new(storage).close().expect("storage close");

    let mut restarted_storage = SqliteStorage::open(&root).expect("storage reopen");
    let mut restarted_service = ExecutionPortService::new(
        &mut restarted_storage,
        winwincode_domain::Instant("2026-08-24T12:00:02.000Z".to_owned()),
    );
    assert_dispatch_result_restart_replay_and_conflict(&mut restarted_service, result);

    drop(restarted_service);
    Box::new(restarted_storage)
        .close()
        .expect("restarted storage close");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn stale_fencing_token_is_rejected_without_replacing_the_durable_lease() {
    let root = temporary_directory("dispatch-stale");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let (mut service, dispatch) = setup_service(&mut storage, "2026-08-24T12:00:01.000Z");
    let mut result = fixture_message::<JobDispatchResultMessage>("job.dispatch_result");
    let rejected_request_id =
        winwincode_domain::RequestId("req_00000000000000000000000030".to_owned());
    result.request_id = rejected_request_id.clone();
    result.lease.fencing_token = winwincode_domain::FencingToken("41".to_owned());

    let response = service
        .handle(ExecutionPortMessage::JobDispatchResultMessage(result))
        .expect("stale result response");
    let ExecutionPortMessage::JobDispatchResultMessage(response) = response else {
        panic!("dispatch result response variant");
    };
    assert_eq!(
        response.status,
        JobDispatchResultMessageStatus::RejectedStaleFencingToken
    );
    assert_eq!(
        response.error.as_ref().map(|error| &error.code),
        Some(&winwincode_execution_port::generated::ExecutionPortErrorCode::StaleFencingToken)
    );

    drop(service);
    let registry = storage.execution_registry().expect("registry reopen");
    let lease = registry
        .load_lease(&dispatch.job.job_id)
        .expect("lease read")
        .expect("durable lease");
    assert_eq!(lease.fencing_token.0, "42");
    assert!(
        !registry
            .has_request(
                "dispatch_result",
                &dispatch.job.job_id,
                &rejected_request_id
            )
            .expect("dispatch result receipt read"),
        "rejected dispatch result must not create a second durable request"
    );
    drop(registry);
    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn expired_lease_is_rejected_without_a_registry_write() {
    let root = temporary_directory("dispatch-expired");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let (mut service, dispatch) = setup_service(&mut storage, "2026-08-24T12:10:00.000Z");
    let result = fixture_message::<JobDispatchResultMessage>("job.dispatch_result");
    let result_request_id = result.request_id.clone();

    let response = service
        .handle(ExecutionPortMessage::JobDispatchResultMessage(result))
        .expect("expired result response");
    let ExecutionPortMessage::JobDispatchResultMessage(response) = response else {
        panic!("dispatch result response variant");
    };
    assert_eq!(
        response.status,
        JobDispatchResultMessageStatus::RejectedExpiredLease
    );
    assert_eq!(
        response.error.as_ref().map(|error| &error.code),
        Some(&winwincode_execution_port::generated::ExecutionPortErrorCode::LeaseExpired)
    );

    drop(service);
    let registry = storage.execution_registry().expect("registry reopen");
    let lease = registry
        .load_lease(&dispatch.job.job_id)
        .expect("lease read")
        .expect("durable lease");
    assert_eq!(lease.expires_at.0, "2026-08-24T12:10:00.000Z");
    assert!(
        !registry
            .has_request("dispatch_result", &dispatch.job.job_id, &result_request_id)
            .expect("dispatch result receipt read"),
        "expired dispatch result must not create a durable receipt"
    );
    drop(registry);
    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn a_result_timestamp_at_lease_expiry_is_rejected_even_before_control_plane_clock_expiry() {
    let root = temporary_directory("dispatch-expired-message");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let (mut service, dispatch) = setup_service(&mut storage, "2026-08-24T12:00:01.000Z");
    let mut result = fixture_message::<JobDispatchResultMessage>("job.dispatch_result");
    let result_request_id = result.request_id.clone();
    result.sent_at = winwincode_domain::Instant("2026-08-24T12:10:00.000Z".to_owned());

    let response = service
        .handle(ExecutionPortMessage::JobDispatchResultMessage(result))
        .expect("expired result response");
    let ExecutionPortMessage::JobDispatchResultMessage(response) = response else {
        panic!("dispatch result response variant");
    };
    assert_eq!(
        response.status,
        JobDispatchResultMessageStatus::RejectedExpiredLease
    );
    assert_eq!(
        response.error.as_ref().map(|error| &error.code),
        Some(&winwincode_execution_port::generated::ExecutionPortErrorCode::LeaseExpired)
    );

    drop(service);
    let registry = storage.execution_registry().expect("registry reopen");
    assert_eq!(
        registry
            .load_lease(&dispatch.job.job_id)
            .expect("lease read")
            .expect("durable lease")
            .expires_at
            .0,
        "2026-08-24T12:10:00.000Z"
    );
    assert!(
        !registry
            .has_request("dispatch_result", &dispatch.job.job_id, &result_request_id)
            .expect("dispatch result receipt read"),
        "expired dispatch result must not create a durable receipt"
    );
    drop(registry);
    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn replaced_worker_instance_is_rejected_before_the_lease_can_be_used() {
    let root = temporary_directory("dispatch-foreign-instance");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let (mut service, dispatch) = setup_service(&mut storage, "2026-08-24T12:00:01.000Z");
    let mut result = fixture_message::<JobDispatchResultMessage>("job.dispatch_result");
    let result_request_id = result.request_id.clone();
    result.lease.worker_instance_id =
        winwincode_domain::WorkerInstanceId("wki_00000000000000000000000099".to_owned());

    let response = service
        .handle(ExecutionPortMessage::JobDispatchResultMessage(result))
        .expect("foreign result response");
    let ExecutionPortMessage::JobDispatchResultMessage(response) = response else {
        panic!("dispatch result response variant");
    };
    assert_eq!(
        response.status,
        JobDispatchResultMessageStatus::RejectedWorkerInstance
    );
    assert_eq!(
        response.error.as_ref().map(|error| &error.code),
        Some(&winwincode_execution_port::generated::ExecutionPortErrorCode::WorkerInstanceChanged)
    );

    drop(service);
    let registry = storage.execution_registry().expect("registry reopen");
    assert_eq!(
        registry
            .load_lease(&dispatch.job.job_id)
            .expect("lease read")
            .expect("durable lease")
            .worker_instance_id
            .0,
        "wki_00000000000000000000000002"
    );
    assert!(
        !registry
            .has_request("dispatch_result", &dispatch.job.job_id, &result_request_id)
            .expect("dispatch result receipt read"),
        "foreign dispatch result must not create a durable receipt"
    );
    drop(registry);
    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn a_new_worker_instance_reports_reacquire_and_old_lease_writes_remain_zero_write() {
    let root = temporary_directory("dispatch-reacquire");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let (mut service, dispatch) = setup_service(&mut storage, "2026-08-24T12:00:01.000Z");

    let mut replacement: WorkerRegisterMessage = fixture_message("worker.register");
    replacement.message_id =
        winwincode_domain::ExecutionMessageId("xmsg_00000000000000000000000020".to_owned());
    replacement.request_id =
        winwincode_domain::RequestId("req_00000000000000000000000020".to_owned());
    replacement.worker_instance_id =
        winwincode_domain::WorkerInstanceId("wki_00000000000000000000000003".to_owned());
    replacement.started_at = winwincode_domain::Instant("2026-08-24T12:00:01.000Z".to_owned());
    replacement.sent_at = replacement.started_at.clone();

    let response = service
        .handle(ExecutionPortMessage::WorkerRegisterMessage(replacement))
        .expect("replacement registration response");
    let ExecutionPortMessage::WorkerRegistrationResultMessage(response) = response else {
        panic!("replacement registration response variant");
    };
    assert_eq!(
        response.status,
        WorkerRegistrationResultMessageStatus::Accepted
    );
    assert_eq!(
        response.lease_recovery,
        WorkerRegistrationResultMessageLeaseRecovery::ReacquireRequired
    );

    let mut result = fixture_message::<JobDispatchResultMessage>("job.dispatch_result");
    let rejected_request_id =
        winwincode_domain::RequestId("req_00000000000000000000000031".to_owned());
    result.request_id = rejected_request_id.clone();
    let response = service
        .handle(ExecutionPortMessage::JobDispatchResultMessage(result))
        .expect("old instance result response");
    let ExecutionPortMessage::JobDispatchResultMessage(response) = response else {
        panic!("dispatch result response variant");
    };
    assert_eq!(
        response.status,
        JobDispatchResultMessageStatus::RejectedWorkerInstance
    );
    assert_eq!(
        response.error.as_ref().map(|error| &error.code),
        Some(&winwincode_execution_port::generated::ExecutionPortErrorCode::WorkerInstanceChanged)
    );

    drop(service);
    let registry = storage.execution_registry().expect("registry reopen");
    let lease = registry
        .load_lease(&dispatch.job.job_id)
        .expect("lease read")
        .expect("old lease");
    assert_eq!(lease.worker_instance_id.0, "wki_00000000000000000000000002");
    assert!(
        !registry
            .has_request(
                "dispatch_result",
                &dispatch.job.job_id,
                &rejected_request_id
            )
            .expect("dispatch result receipt read"),
        "rejected old-instance result must not create a write receipt"
    );
    drop(registry);
    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn a_second_lease_id_for_an_active_job_is_rejected_without_a_unique_lease_write() {
    let root = temporary_directory("claim-unique");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let (mut service, dispatch) = setup_service(&mut storage, "2026-08-24T12:00:01.000Z");
    let mut conflicting = claim_from_dispatch(&dispatch);
    conflicting.lease_id = winwincode_domain::LeaseId("lse_00000000000000000000000005".to_owned());
    conflicting.message_id =
        winwincode_domain::ExecutionMessageId("xmsg_00000000000000000000000021".to_owned());
    conflicting.request_id =
        winwincode_domain::RequestId("req_00000000000000000000000021".to_owned());

    let error = service
        .claim_execution_job(dispatch.job.clone(), conflicting.clone())
        .expect_err("active job must not accept a second lease");
    assert!(matches!(
        error,
        ExecutionPortServiceError::ClaimRejected(
            winwincode_storage::LeaseWriteStatus::RejectedConflict
        )
    ));

    drop(service);
    let registry = storage.execution_registry().expect("registry reopen");
    let lease = registry
        .load_lease(&dispatch.job.job_id)
        .expect("lease read")
        .expect("original lease");
    assert_eq!(lease.lease_id, dispatch.lease.lease_id);
    assert!(
        !registry
            .has_request("claim", &dispatch.job.job_id, &conflicting.request_id)
            .expect("claim receipt read"),
        "conflicting claim must not create a durable receipt"
    );
    drop(registry);
    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn a_job_claim_mismatch_is_rejected_before_the_registry_can_write() {
    let root = temporary_directory("claim-mismatch");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let register: WorkerRegisterMessage = fixture_message("worker.register");
    let mut service = ExecutionPortService::new(&mut storage, register.sent_at.clone());
    service
        .handle(ExecutionPortMessage::WorkerRegisterMessage(register))
        .expect("registration response");

    let expected: JobDispatchMessage = fixture_message("job.dispatch");
    let claim = claim_from_dispatch(&expected);
    let mut mismatched_job = expected.job.clone();
    mismatched_job.payload_digest = winwincode_domain::Sha256Digest(
        "sha256:1111111111111111111111111111111111111111111111111111111111111111".to_owned(),
    );
    let error = service
        .claim_execution_job(mismatched_job, claim.clone())
        .expect_err("payload mismatch must be rejected");
    assert!(matches!(
        error,
        ExecutionPortServiceError::JobMismatch("payloadDigest")
    ));

    drop(service);
    let registry = storage.execution_registry().expect("registry reopen");
    assert_eq!(
        registry
            .load_lease(&expected.job.job_id)
            .expect("lease read"),
        None,
        "a claim rejected before storage must not create a lease"
    );
    assert!(
        !registry
            .has_request("claim", &expected.job.job_id, &claim.request_id)
            .expect("claim receipt read")
    );
    drop(registry);
    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn a_claim_must_join_the_exact_durable_job_identity_before_writing_a_lease() {
    let root = temporary_directory("claim-durable-identity");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let expected: JobDispatchMessage = fixture_message("job.dispatch");
    commit_durable_dispatch_intent(&mut storage, &expected.job);
    let register: WorkerRegisterMessage = fixture_message("worker.register");
    let mut service = ExecutionPortService::new(&mut storage, register.sent_at.clone());
    service
        .handle(ExecutionPortMessage::WorkerRegisterMessage(register))
        .expect("registration response");

    let mut foreign_job = expected.job.clone();
    foreign_job.job_id =
        winwincode_domain::ExecutionJobId("job_00000000000000000000000099".to_owned());
    let mut foreign_claim = claim_from_dispatch(&expected);
    foreign_claim.job_id = foreign_job.job_id.clone();
    let foreign_error = service
        .claim_execution_job(foreign_job, foreign_claim)
        .expect_err("a foreign job must not claim without its durable intent");
    assert!(matches!(
        foreign_error,
        ExecutionPortServiceError::Storage(error)
            if error.kind() == winwincode_storage::StorageErrorKind::InvalidInput
    ));

    let mut changed_attempt = expected.job.clone();
    changed_attempt.attempt += 1;
    let mut changed_attempt_claim = claim_from_dispatch(&expected);
    changed_attempt_claim.attempt += 1;
    let attempt_error = service
        .claim_execution_job(changed_attempt, changed_attempt_claim)
        .expect_err("a changed attempt must not claim the durable job");
    assert!(matches!(
        attempt_error,
        ExecutionPortServiceError::JobMismatch("attempt")
    ));

    let mut changed_digest = expected.job.clone();
    changed_digest.payload_digest = winwincode_domain::Sha256Digest(
        "sha256:1111111111111111111111111111111111111111111111111111111111111111".to_owned(),
    );
    let mut changed_digest_claim = claim_from_dispatch(&expected);
    changed_digest_claim.payload_digest = changed_digest.payload_digest.clone();
    let digest_error = service
        .claim_execution_job(changed_digest, changed_digest_claim)
        .expect_err("a changed payload digest must not claim the durable job");
    assert!(matches!(
        digest_error,
        ExecutionPortServiceError::JobMismatch("payloadDigest")
    ));

    let mut changed_scope = expected.job.clone();
    let winwincode_execution_port::generated::ExecutionScope::DeliveryStageExecutionScope(scope) =
        &mut changed_scope.scope
    else {
        panic!("fixture job must use a Delivery stage scope");
    };
    scope.stage_run_id = winwincode_domain::StageRunId("run_00000000000000000000000099".to_owned());
    let scope_error = service
        .claim_execution_job(changed_scope, claim_from_dispatch(&expected))
        .expect_err("a changed scope must not claim the durable job");
    assert!(matches!(
        scope_error,
        ExecutionPortServiceError::JobMismatch("scope")
    ));

    drop(service);
    let registry = storage.execution_registry().expect("registry reopen");
    assert_eq!(
        registry
            .load_lease(&expected.job.job_id)
            .expect("lease read"),
        None,
        "identity mismatches must not write a lease"
    );
    drop(registry);
    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("directory release");
}

#[test]
fn an_unknown_worker_id_is_rejected_without_mutating_the_current_lease() {
    let root = temporary_directory("dispatch-foreign-worker");
    let mut storage = SqliteStorage::open(&root).expect("storage open");
    let (mut service, dispatch) = setup_service(&mut storage, "2026-08-24T12:00:01.000Z");
    let mut result = fixture_message::<JobDispatchResultMessage>("job.dispatch_result");
    let result_request_id = result.request_id.clone();
    result.lease.worker_id =
        winwincode_domain::WorkerId("wrk_00000000000000000000000099".to_owned());

    let response = service
        .handle(ExecutionPortMessage::JobDispatchResultMessage(result))
        .expect("unknown worker response");
    let ExecutionPortMessage::JobDispatchResultMessage(response) = response else {
        panic!("dispatch result response variant");
    };
    assert_eq!(
        response.status,
        JobDispatchResultMessageStatus::RejectedWorkerInstance
    );
    assert_eq!(
        response.error.as_ref().map(|error| &error.code),
        Some(&winwincode_execution_port::generated::ExecutionPortErrorCode::WorkerNotRegistered)
    );

    drop(service);
    let registry = storage.execution_registry().expect("registry reopen");
    assert_eq!(
        registry
            .load_lease(&dispatch.job.job_id)
            .expect("lease read")
            .expect("current lease")
            .worker_id
            .0,
        "wrk_00000000000000000000000001"
    );
    assert!(
        !registry
            .has_request("dispatch_result", &dispatch.job.job_id, &result_request_id)
            .expect("dispatch result receipt read"),
        "unknown-worker dispatch result must not create a durable receipt"
    );
    drop(registry);
    Box::new(storage).close().expect("storage close");
    fs::remove_dir_all(root).expect("directory release");
}

fn accept_with_both_adapters(
    local: &mut ExecutionPortService<'_>,
    remote: &mut ExecutionPortService<'_>,
    message: ExecutionPortMessage,
) -> (ExecutionPortMessage, ExecutionPortMessage) {
    let frame = TypedFrame::new(
        winwincode_execution_port::transport::FrameDirection::WorkerToControlPlane,
        message,
    )
    .expect("worker frame");
    let encoded = RemoteTransportAdapter::<ExecutionPortService<'_>>::encode(&frame)
        .expect("remote frame encoding");
    let local_output = LocalWorkerAdapter::new(local, EndpointSide::ControlPlane)
        .accept(&frame)
        .expect("local adapter output");
    let remote_output = RemoteTransportAdapter::new(remote, EndpointSide::ControlPlane)
        .accept(&encoded)
        .expect("remote adapter output");
    (local_output, remote_output)
}

#[test]
#[allow(clippy::too_many_lines)]
fn local_and_remote_adapters_share_the_same_durable_service_outcomes() {
    let local_root = temporary_directory("parity-local");
    let remote_root = temporary_directory("parity-remote");
    let mut local_storage = SqliteStorage::open(&local_root).expect("local storage open");
    let mut remote_storage = SqliteStorage::open(&remote_root).expect("remote storage open");
    let expected: JobDispatchMessage = fixture_message("job.dispatch");
    commit_durable_dispatch_intent(&mut local_storage, &expected.job);
    commit_durable_dispatch_intent(&mut remote_storage, &expected.job);
    seed_queue_lease(
        &mut local_storage,
        &expected.job,
        &claim_from_dispatch(&expected),
        300,
    );
    seed_queue_lease(
        &mut remote_storage,
        &expected.job,
        &claim_from_dispatch(&expected),
        300,
    );
    let mut local = ExecutionPortService::new(
        &mut local_storage,
        winwincode_domain::Instant("2026-08-24T12:00:01.000Z".to_owned()),
    );
    let mut remote = ExecutionPortService::new(
        &mut remote_storage,
        winwincode_domain::Instant("2026-08-24T12:00:01.000Z".to_owned()),
    );

    let register: WorkerRegisterMessage = fixture_message("worker.register");
    let register_message = ExecutionPortMessage::WorkerRegisterMessage(register.clone());
    let (local_first, remote_first) =
        accept_with_both_adapters(&mut local, &mut remote, register_message.clone());
    assert_eq!(local_first, remote_first);
    let (local_duplicate, remote_duplicate) =
        accept_with_both_adapters(&mut local, &mut remote, register_message);
    assert_eq!(local_duplicate, remote_duplicate);

    let mut conflict = register;
    conflict.capabilities.features.pop();
    let (local_conflict, remote_conflict) = accept_with_both_adapters(
        &mut local,
        &mut remote,
        ExecutionPortMessage::WorkerRegisterMessage(conflict),
    );
    assert_eq!(local_conflict, remote_conflict);

    let mut heartbeat: WorkerHeartbeatMessage = fixture_message("worker.heartbeat");
    heartbeat.active_leases.clear();
    let (local_heartbeat, remote_heartbeat) = accept_with_both_adapters(
        &mut local,
        &mut remote,
        ExecutionPortMessage::WorkerHeartbeatMessage(heartbeat.clone()),
    );
    assert_eq!(local_heartbeat, remote_heartbeat);
    let (local_heartbeat_duplicate, remote_heartbeat_duplicate) = accept_with_both_adapters(
        &mut local,
        &mut remote,
        ExecutionPortMessage::WorkerHeartbeatMessage(heartbeat),
    );
    assert_eq!(
        local_heartbeat_duplicate, remote_heartbeat_duplicate,
        "heartbeat replay must remain adapter-parity and durable"
    );

    let local_dispatch = local
        .claim_execution_job(expected.job.clone(), claim_from_dispatch(&expected))
        .expect("local durable claim");
    let remote_dispatch = remote
        .claim_execution_job(expected.job.clone(), claim_from_dispatch(&expected))
        .expect("remote durable claim");
    assert_eq!(local_dispatch, remote_dispatch);

    let result = fixture_message::<JobDispatchResultMessage>("job.dispatch_result");
    let (local_result, remote_result) = accept_with_both_adapters(
        &mut local,
        &mut remote,
        ExecutionPortMessage::JobDispatchResultMessage(result),
    );
    assert_eq!(local_result, remote_result);

    let mut stale = fixture_message::<JobDispatchResultMessage>("job.dispatch_result");
    let stale_request_id =
        winwincode_domain::RequestId("req_00000000000000000000000032".to_owned());
    stale.request_id = stale_request_id.clone();
    stale.lease.fencing_token = winwincode_domain::FencingToken("41".to_owned());
    let (local_stale, remote_stale) = accept_with_both_adapters(
        &mut local,
        &mut remote,
        ExecutionPortMessage::JobDispatchResultMessage(stale),
    );
    assert_eq!(local_stale, remote_stale);

    drop(local);
    drop(remote);
    let local_registry = local_storage.execution_registry().expect("local reopen");
    let remote_registry = remote_storage.execution_registry().expect("remote reopen");
    let local_lease = local_registry
        .load_lease(&expected.job.job_id)
        .expect("local lease read")
        .expect("local durable lease");
    let remote_lease = remote_registry
        .load_lease(&expected.job.job_id)
        .expect("remote lease read")
        .expect("remote durable lease");
    assert_eq!(local_lease, remote_lease);
    assert_eq!(local_lease.fencing_token, expected.lease.fencing_token);
    assert!(
        !local_registry
            .has_request("dispatch_result", &expected.job.job_id, &stale_request_id)
            .expect("local stale receipt read")
    );
    assert!(
        !remote_registry
            .has_request("dispatch_result", &expected.job.job_id, &stale_request_id)
            .expect("remote stale receipt read")
    );
    assert_eq!(
        local_registry
            .load_worker(&expected.lease.worker_id)
            .expect("local worker read"),
        remote_registry
            .load_worker(&expected.lease.worker_id)
            .expect("remote worker read")
    );
    drop(local_registry);
    drop(remote_registry);
    Box::new(local_storage)
        .close()
        .expect("local storage close");
    Box::new(remote_storage)
        .close()
        .expect("remote storage close");
    fs::remove_dir_all(local_root).expect("local directory release");
    fs::remove_dir_all(remote_root).expect("remote directory release");
}

mod runtime_router_fixture {
    use std::convert::Infallible;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use rusqlite::{Connection, params};
    use sha2::{Digest, Sha256};
    use winwincode_api::generated::{
        Actor, CommandEnvelope, CommandName, RepositoryScope, Scope, UserActor,
    };
    use winwincode_control_plane::delivery_execution::{
        DeliveryExecutionConfig, DeliveryExecutionPortError, ExecutionJobDispatcher,
        PendingDeliveryExecution, prepare_delivery_advance,
    };
    use winwincode_control_plane::execution_port_service::{
        RuntimeEventPortRouter, RuntimeEventRoute, RuntimeEventRouteResolver,
        RuntimeReplayRequestCommand,
    };
    use winwincode_control_plane::{
        ControlPlane, ControlPlaneConfig, EventPublishError, EventPublisher,
        ExecutionPortServiceError, OutboxEvent,
    };
    use winwincode_delivery::application::stage::{
        AdvanceStageInput, NewStageIdentities, SessionBindingAuthority, advance,
        test_support::{active_lease_identity, session_binding_authority},
    };
    use winwincode_delivery::domain::{
        DELIVERY_SCHEMA_VERSION, Delivery, DeliveryStatus, DeliveryTask, DeliveryTaskStatus,
        SessionBindingId,
    };
    use winwincode_delivery::store::{
        AtomicPublication, CreateDelivery, DeliveryCommand, DeliveryCommandPort,
        DeliveryJournalPort, DeliveryStore, JournalBackendError, LoadedDeliveryJournal,
    };
    use winwincode_domain::{
        AttentionItemId, CodexThreadId, DeliveryId, EnterprisePolicyId, ExecutionAckSequence,
        ExecutionEventId, ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken,
        Instant, LeaseId, OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId,
        Revision, SchemaVersion, SessionBindingSourceIdentity, SessionBindingSourceIdentityKind,
        SessionIdentity, Sha256Digest, StageRunId, UserId, WorkerId, WorkerInstanceId,
        WorkerSessionId, WorkspaceId,
    };
    use winwincode_execution_port::action_enforcement::{
        ActionEnforcementIssuer, ActionEnforcementSigningKey,
    };
    use winwincode_execution_port::generated::{
        ActionEnforcementDecision, ActionEnforcementRequestMessage,
        ActionEnforcementRequestMessageKind, ActionPolicyKind, ExecutionEventCategory,
        ExecutionEventRecord, ExecutionJob, ExecutionLeaseStamp, ExecutionLimits,
        ExecutionPortMessage, ExecutionScope, ExecutionWorkspace, ExecutionWorkspaceWriteMode,
        LeaseWriteStatus, RuntimeAckMessage, RuntimeEventMessage, RuntimeEventMessageKind,
        SessionBindingMessage, SessionBindingMessageKind,
    };
    use winwincode_execution_port::transport::{
        EndpointSide, FrameDirection, LocalWorkerAdapter, RemoteTransportAdapter, TypedFrame,
    };
    use winwincode_storage::{
        AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, CommitReceipt,
        DurableOutboxEvent, EnterprisePolicyActor, EnterprisePolicyChildOverrideMode,
        EnterprisePolicyDefinition, EnterprisePolicyEffect, EnterprisePolicyInheritanceMode,
        EnterprisePolicyKind, EnterprisePolicyMode, EnterprisePolicyScope, EnterprisePolicyState,
        EnterprisePolicyVersionSource, EnterprisePolicyWrite, ExecutionLeaseClaim,
        ExecutionRegistry, LoadedAggregateJournal, NewOutboxEvent, PendingAuditEvent,
        ProductStateStorage, ProjectionEventCursor, ProjectionEventStreamKey, ProjectionReadCut,
        ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey, SqliteStorage, StateCommit,
        StateRevisionGuard, StorageError, StoredState, WorkerRegistrationRequest,
    };

    static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1_000);

    fn temporary_directory(name: &str) -> PathBuf {
        let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "winwincode-runtime-router-{name}-{}-{suffix}",
            std::process::id()
        ))
    }

    fn canonical_id(prefix: &str, value: u64) -> String {
        format!("{prefix}_{value:026}")
    }

    fn delivery_before_advance(seed: u64) -> Delivery {
        let mut snapshot = Delivery::decode_json(include_bytes!(
            "../../winwincode-delivery/tests/fixtures/delivery-main.json"
        ))
        .expect("canonical fixture")
        .into_snapshot();
        let delivery_id = DeliveryId(canonical_id("dlv", seed));
        snapshot.id = delivery_id.clone();
        snapshot.spec.delivery_id = delivery_id.clone();
        snapshot.revision = 1;
        snapshot.status = DeliveryStatus::Executing;
        snapshot.tasks = vec![DeliveryTask {
            schema_version: DELIVERY_SCHEMA_VERSION,
            id: winwincode_domain::DeliveryTaskId(canonical_id("dtk", seed)),
            delivery_id,
            title: "Implement the approved task".into(),
            goal: "Implement the approved candidate change.".into(),
            acceptance_criterion_ids: vec![snapshot.spec.acceptance_criteria[0].id.clone()],
            blocked_by_task_ids: Vec::new(),
            owner: None,
            status: DeliveryTaskStatus::Pending,
        }];
        snapshot.stage_runs.clear();
        snapshot.session_bindings.clear();
        snapshot.attention_items.clear();
        snapshot.evidence.clear();
        snapshot.verdict = None;
        snapshot.updated_at_millis = snapshot.created_at_millis;
        Delivery::try_from_snapshot(snapshot).expect("Delivery before advance")
    }

    fn pending_execution(seed: u64) -> PendingDeliveryExecution {
        let delivery = delivery_before_advance(seed);
        let result = advance(
            &delivery,
            AdvanceStageInput {
                current_lease: None,
                rework_authorization: None,
                expected_revision: 1,
                product_session_id: ProductSessionId(canonical_id("psn", seed)),
                identities: NewStageIdentities {
                    stage_run_id: StageRunId(canonical_id("run", seed)),
                    execution_job_id: ExecutionJobId(canonical_id("job", seed)),
                    session_binding_id: SessionBindingId::new(format!("binding-{seed}"))
                        .expect("binding id"),
                    attention_item_id: AttentionItemId(canonical_id("att", seed)),
                },
                review: None,
                previous_outcome: None,
                now_millis: 1_800_000_000_100,
            },
        )
        .expect("stage advance");
        prepare_delivery_advance(
            RequestId(canonical_id("req", seed)),
            result,
            DeliveryExecutionConfig {
                payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
                candidate_ref: None,
                workspace: ExecutionWorkspace {
                    checkout_revision: "original-checkout".into(),
                    repository_id: RepositoryId(canonical_id("rep", seed)),
                    write_mode: ExecutionWorkspaceWriteMode::Candidate,
                },
                limits: ExecutionLimits {
                    deadline_at: Instant("2027-01-15T09:00:00.000Z".into()),
                    max_artifact_bytes: 10_000_000,
                    max_runtime_seconds: 3_600,
                },
            },
        )
        .expect("pending execution")
    }

    fn delivery_advance_command(seed: u64) -> CommandEnvelope {
        CommandEnvelope {
            actor: Actor::UserActor(UserActor {
                id: UserId(canonical_id("usr", seed)),
                kind: winwincode_api::generated::UserActorKind::User,
            }),
            command: CommandName::DeliveryAdvance,
            expected_revision: Revision(1),
            payload: serde_json::json!({"deliveryId": canonical_id("dlv", seed)}),
            request_id: RequestId(canonical_id("req", seed)),
            schema_version: SchemaVersion::WinwincodeV1,
            scope: Scope::RepositoryScope(RepositoryScope {
                kind: winwincode_api::generated::RepositoryScopeKind::Repository,
                organization_id: OrganizationId(canonical_id("org", seed)),
                workspace_id: WorkspaceId(canonical_id("wsp", seed)),
                project_id: ProjectId(canonical_id("prj", seed)),
                repository_id: RepositoryId(canonical_id("rep", seed)),
            }),
        }
    }

    fn lease_and_binding(
        pending: &PendingDeliveryExecution,
        seed: u64,
    ) -> (SessionBindingAuthority, SessionBindingMessage) {
        let worker_session_id = WorkerSessionId(canonical_id("wsn", seed));
        let worker_id = WorkerId(canonical_id("wrk", seed));
        let worker_instance_id = WorkerInstanceId(canonical_id("wki", seed));
        let lease_id = LeaseId(canonical_id("lse", seed));
        let fencing_token = FencingToken(seed.to_string());
        let lease = active_lease_identity(
            pending.job().job_id.clone(),
            1,
            lease_id.clone(),
            fencing_token.clone(),
            worker_id.clone(),
            worker_instance_id.clone(),
            worker_session_id.clone(),
        );
        let (stage_run_id, product_session_id) = match &pending.job().scope {
            ExecutionScope::DeliveryStageExecutionScope(scope) => {
                (scope.stage_run_id.clone(), scope.product_session_id.clone())
            }
            ExecutionScope::ProductSessionExecutionScope(_) => {
                panic!("runtime router fixture must use a Delivery-stage job")
            }
        };
        let session_identity = SessionIdentity {
            codex_thread_id: CodexThreadId(canonical_id("cdx", seed)),
            product_session_id: product_session_id.clone(),
            stage_run_id: Some(stage_run_id.clone()),
            worker_session_id: worker_session_id.clone(),
        };
        let message = SessionBindingMessage {
            attempt: 1,
            bound_at: Instant("2027-01-15T08:00:01.000Z".into()),
            codex_thread_id: session_identity.codex_thread_id.clone(),
            fencing_token: fencing_token.clone(),
            kind: SessionBindingMessageKind::SessionBinding,
            lease: ExecutionLeaseStamp {
                attempt: 1,
                expires_at: Instant("2027-01-15T08:05:00.000Z".into()),
                fencing_token: fencing_token.clone(),
                issued_at: Instant("2027-01-15T08:00:00.200Z".into()),
                job_id: pending.job().job_id.clone(),
                lease_id: lease_id.clone(),
                worker_id: worker_id.clone(),
                worker_instance_id: worker_instance_id.clone(),
            },
            lease_id: lease_id.clone(),
            message_id: ExecutionMessageId(canonical_id("xmsg", seed)),
            product_session_id,
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: Instant("2027-01-15T08:00:01.100Z".into()),
            session_identity,
            source_identity: SessionBindingSourceIdentity {
                kind: SessionBindingSourceIdentityKind::ExecutionWorker,
                lease_id,
                worker_id: worker_id.clone(),
                worker_instance_id,
                worker_session_id: worker_session_id.clone(),
            },
            stage_run_id: Some(stage_run_id),
            worker_id,
            worker_session_id,
        };
        let authority = session_binding_authority(
            lease,
            message.lease.issued_at.clone(),
            message.lease.expires_at.clone(),
        );
        (authority, message)
    }

    fn runtime_message(
        pending: &PendingDeliveryExecution,
        binding: &SessionBindingMessage,
        seed: u64,
    ) -> RuntimeEventMessage {
        let (stage_run_id, product_session_id) = match &pending.job().scope {
            ExecutionScope::DeliveryStageExecutionScope(scope) => {
                (scope.stage_run_id.clone(), scope.product_session_id.clone())
            }
            ExecutionScope::ProductSessionExecutionScope(_) => {
                panic!("runtime router fixture must use a Delivery-stage job")
            }
        };
        RuntimeEventMessage {
            codex_thread_id: binding.codex_thread_id.clone(),
            event: ExecutionEventRecord {
                category: ExecutionEventCategory::Lifecycle,
                event_id: ExecutionEventId(canonical_id("xevt", seed + 100)),
                occurred_at: Instant("2027-01-15T08:00:01.050Z".into()),
                payload: None,
                sequence: ExecutionSequence(1),
                summary: "worker session started".into(),
            },
            kind: RuntimeEventMessageKind::RuntimeEvent,
            lease: binding.lease.clone(),
            message_id: ExecutionMessageId(canonical_id("xmsg", seed + 100)),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: Instant("2027-01-15T08:00:01.100Z".into()),
            session_identity: SessionIdentity {
                codex_thread_id: binding.codex_thread_id.clone(),
                product_session_id,
                stage_run_id: Some(stage_run_id),
                worker_session_id: binding.worker_session_id.clone(),
            },
            worker_session_id: binding.worker_session_id.clone(),
        }
    }

    fn replay_command(seed: u64, job_id: ExecutionJobId) -> RuntimeReplayRequestCommand {
        RuntimeReplayRequestCommand {
            job_id,
            max_events: 100,
            message_id: ExecutionMessageId(canonical_id("xmsg", seed + 500)),
            request_id: RequestId(canonical_id("req", seed + 500)),
            sent_at: Instant("2027-01-15T08:00:01.200Z".into()),
        }
    }

    fn repository_scope(seed: u64) -> RepositoryScope {
        match delivery_advance_command(seed).scope {
            Scope::RepositoryScope(scope) => scope,
            _ => panic!("runtime router fixture must use repository scope"),
        }
    }

    #[derive(Clone)]
    struct RuntimeFixtureResolver {
        route: RuntimeEventRoute,
        calls: Arc<AtomicU64>,
    }

    impl RuntimeEventRouteResolver for RuntimeFixtureResolver {
        type Error = Infallible;

        fn resolve(
            &mut self,
            _control_plane: &ControlPlane,
            _message: &RuntimeEventMessage,
        ) -> Result<RuntimeEventRoute, Self::Error> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.route.clone())
        }
    }

    struct RuntimeFixture {
        root: PathBuf,
        control_plane: ControlPlane,
        authority: SessionBindingAuthority,
        job: ExecutionJob,
        scope: RepositoryScope,
        runtime: RuntimeEventMessage,
    }

    struct GuardRecordingStorage {
        inner: SqliteStorage,
        commits: Arc<Mutex<Vec<Vec<StateRevisionGuard>>>>,
    }

    impl ProductStateStorage for GuardRecordingStorage {
        fn commit(&mut self, commit: &StateCommit) -> Result<CommitReceipt, StorageError> {
            self.commits
                .lock()
                .expect("recorded commit guards lock")
                .push(commit.state_guards().to_vec());
            self.inner.commit(commit)
        }

        fn load_receipt(
            &self,
            identity: &ReceiptIdentity,
            command_digest: &Sha256Digest,
        ) -> Result<Option<CommitReceipt>, StorageError> {
            self.inner.load_receipt(identity, command_digest)
        }

        fn load_pending_audit_event(
            &self,
            identity: &ReceiptIdentity,
        ) -> Result<Option<PendingAuditEvent>, StorageError> {
            self.inner.load_pending_audit_event(identity)
        }

        fn pending_audit_events(&self) -> Result<Vec<PendingAuditEvent>, StorageError> {
            Ok(Vec::new())
        }

        fn load_outbox_event(
            &self,
            event_id: &str,
        ) -> Result<Option<DurableOutboxEvent>, StorageError> {
            self.inner.load_outbox_event(event_id)
        }

        fn load_state(&self, stream_id: &str) -> Result<Option<StoredState>, StorageError> {
            self.inner.load_state(stream_id)
        }

        fn load_projection_read_cut(
            &self,
            state_stream_ids: &[String],
            key: &ProjectionEventStreamKey,
            expected: Option<&ProjectionEventCursor>,
        ) -> Result<ProjectionReadCut, StorageError> {
            self.inner
                .load_projection_read_cut(state_stream_ids, key, expected)
        }

        fn load_journal(
            &self,
            key: &AggregateJournalKey,
        ) -> Result<Option<LoadedAggregateJournal>, StorageError> {
            self.inner.load_journal(key)
        }

        fn pending_events(&self) -> Result<Vec<OutboxEvent>, StorageError> {
            self.inner.pending_events()
        }

        fn mark_published(&mut self, event_id: &str) -> Result<(), StorageError> {
            self.inner.mark_published(event_id)
        }

        fn close(self: Box<Self>) -> Result<(), StorageError> {
            Box::new(self.inner).close()
        }
    }

    fn runtime_fixture(seed: u64, name: &str) -> RuntimeFixture {
        let root = temporary_directory(name);
        let pending = pending_execution(seed);
        seed_delivery(&root, &delivery_before_advance(seed));
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane start");
        control_plane
            .commit_delivery_execution(
                &delivery_advance_command(seed),
                &pending,
                &mut RecordingDispatcher,
            )
            .expect("Delivery execution commit");
        let (authority, binding) = lease_and_binding(&pending, seed);
        control_plane
            .commit_delivery_session_binding(&binding, &authority, &binding.sent_at)
            .expect("SessionBinding commit");
        let scope = repository_scope(seed);
        let runtime = runtime_message(&pending, &binding, seed);
        RuntimeFixture {
            root,
            control_plane,
            authority,
            job: pending.job().clone(),
            scope,
            runtime,
        }
    }

    fn runtime_fixture_with_guard_recorder(
        seed: u64,
        name: &str,
    ) -> (RuntimeFixture, Arc<Mutex<Vec<Vec<StateRevisionGuard>>>>) {
        let root = temporary_directory(name);
        let pending = pending_execution(seed);
        seed_delivery(&root, &delivery_before_advance(seed));
        let storage = SqliteStorage::open(&root).expect("storage open");
        let commits = Arc::new(Mutex::new(Vec::new()));
        let mut control_plane = ControlPlane::start(
            Box::new(GuardRecordingStorage {
                inner: storage,
                commits: Arc::clone(&commits),
            }),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane start");
        control_plane
            .commit_delivery_execution(
                &delivery_advance_command(seed),
                &pending,
                &mut RecordingDispatcher,
            )
            .expect("Delivery execution commit");
        let (authority, binding) = lease_and_binding(&pending, seed);
        control_plane
            .commit_delivery_session_binding(&binding, &authority, &binding.sent_at)
            .expect("SessionBinding commit");
        let scope = repository_scope(seed);
        let runtime = runtime_message(&pending, &binding, seed);
        (
            RuntimeFixture {
                root,
                control_plane,
                authority,
                job: pending.job().clone(),
                scope,
                runtime,
            },
            commits,
        )
    }

    #[test]
    fn delivery_runtime_commit_carries_the_current_delivery_revision_guard() {
        let seed = 204;
        let (mut fixture, commits) =
            runtime_fixture_with_guard_recorder(seed, "delivery-state-guard");
        let delivery_id = match &fixture.job.scope {
            ExecutionScope::DeliveryStageExecutionScope(scope) => scope.delivery_id.clone(),
            ExecutionScope::ProductSessionExecutionScope(_) => {
                panic!("runtime fixture must use a Delivery-stage job")
            }
        };
        let delivery_stream = format!("delivery:{}", delivery_id.0);
        let current_delivery_revision = fixture
            .control_plane
            .load_state(&delivery_stream)
            .expect("current Delivery read")
            .expect("current Delivery state")
            .revision;
        commits.lock().expect("recorded commit guards lock").clear();

        let ack = fixture
            .control_plane
            .accept_runtime_event(
                &fixture.scope,
                &fixture.runtime,
                &fixture.authority,
                &fixture.runtime.sent_at,
            )
            .expect("runtime event should be accepted");
        assert_eq!(ack.status, LeaseWriteStatus::Accepted);

        let recorded = commits.lock().expect("recorded commit guards lock");
        assert_eq!(recorded.len(), 1, "runtime ingress must issue one commit");
        assert_eq!(
            recorded[0].len(),
            1,
            "Delivery runtime commit must carry one guard"
        );
        assert_eq!(recorded[0][0].stream_id(), delivery_stream);
        assert_eq!(
            recorded[0][0].expected_revision(),
            current_delivery_revision,
            "guard must bind the Delivery revision read before ledger construction"
        );
        drop(recorded);

        fixture
            .control_plane
            .shutdown()
            .expect("Control Plane shutdown");
        fs::remove_dir_all(fixture.root).expect("runtime fixture directory release");
    }

    fn install_runtime_lease(
        storage: &mut SqliteStorage,
        runtime: &RuntimeEventMessage,
        job: &ExecutionJob,
        seed: u64,
    ) {
        let mut registry = ExecutionRegistry::new(storage).expect("execution registry");
        registry
            .register_worker(&WorkerRegistrationRequest {
                authentication_identity:
                    winwincode_storage::WorkerAuthenticationIdentity::LocalEmbedded {
                        control_plane_principal: "fixture-control-plane".to_owned(),
                    },
                protocol_version: winwincode_storage::EXECUTION_PROTOCOL_VERSION.to_owned(),
                platform: winwincode_storage::WorkerPlatform::Aarch64AppleDarwin,
                capabilities: vec!["runtime-replay".to_owned()],
                capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
                security_zone: "local".to_owned(),
                max_slots: 1,
                message_id: ExecutionMessageId(canonical_id("xmsg", seed + 600)),
                request_id: RequestId(canonical_id("req", seed + 600)),
                sent_at: runtime.lease.issued_at.clone(),
                started_at: Instant("2027-01-15T08:00:00.000Z".into()),
                worker_id: runtime.lease.worker_id.clone(),
                worker_instance_id: runtime.lease.worker_instance_id.clone(),
            })
            .expect("worker registration");
        registry
            .claim_execution_job(&ExecutionLeaseClaim {
                expires_at: runtime.lease.expires_at.clone(),
                fencing_token: runtime.lease.fencing_token.clone(),
                issued_at: runtime.lease.issued_at.clone(),
                job_id: runtime.lease.job_id.clone(),
                lease_id: runtime.lease.lease_id.clone(),
                message_id: ExecutionMessageId(canonical_id("xmsg", seed + 601)),
                payload_digest: job.payload_digest.clone(),
                request_id: RequestId(canonical_id("req", seed + 601)),
                worker_id: runtime.lease.worker_id.clone(),
                worker_instance_id: runtime.lease.worker_instance_id.clone(),
                attempt: u64::try_from(runtime.lease.attempt).expect("lease attempt"),
            })
            .expect("execution lease claim");
    }

    fn runtime_ack(message: ExecutionPortMessage) -> RuntimeAckMessage {
        let ExecutionPortMessage::RuntimeAckMessage(ack) = message else {
            panic!("runtime router must return runtime.ack");
        };
        ack
    }

    fn accept_with_adapters(
        local_control_plane: &mut ControlPlane,
        remote_control_plane: &mut ControlPlane,
        local_route: RuntimeEventRoute,
        remote_route: RuntimeEventRoute,
        message: RuntimeEventMessage,
    ) -> (RuntimeAckMessage, RuntimeAckMessage, u64, u64) {
        let server_time = message.sent_at.clone();
        let frame = TypedFrame::new(
            FrameDirection::WorkerToControlPlane,
            ExecutionPortMessage::RuntimeEventMessage(message),
        )
        .expect("runtime worker frame");
        let encoded =
            RemoteTransportAdapter::<RuntimeEventPortRouter<'_, RuntimeFixtureResolver>>::encode(
                &frame,
            )
            .expect("remote runtime frame encoding");
        let local_calls = Arc::new(AtomicU64::new(0));
        let remote_calls = Arc::new(AtomicU64::new(0));
        let mut local_router = RuntimeEventPortRouter::new(
            local_control_plane,
            RuntimeFixtureResolver {
                route: local_route,
                calls: Arc::clone(&local_calls),
            },
            server_time.clone(),
        );
        let mut remote_router = RuntimeEventPortRouter::new(
            remote_control_plane,
            RuntimeFixtureResolver {
                route: remote_route,
                calls: Arc::clone(&remote_calls),
            },
            server_time,
        );
        let local_output = LocalWorkerAdapter::new(&mut local_router, EndpointSide::ControlPlane)
            .accept(&frame)
            .expect("local runtime adapter output");
        let remote_output =
            RemoteTransportAdapter::new(&mut remote_router, EndpointSide::ControlPlane)
                .accept(&encoded)
                .expect("remote runtime adapter output");
        (
            runtime_ack(local_output),
            runtime_ack(remote_output),
            local_calls.load(Ordering::Relaxed),
            remote_calls.load(Ordering::Relaxed),
        )
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn local_and_remote_runtime_adapters_use_the_control_plane_runtime_transaction() {
        let seed = 201;
        let mut local = runtime_fixture(seed, "parity-local");
        let mut remote = runtime_fixture(seed, "parity-remote");
        let local_route =
            RuntimeEventRoute::from_sealed_scheduler(local.scope.clone(), local.authority.clone());
        let remote_route = RuntimeEventRoute::from_sealed_scheduler(
            remote.scope.clone(),
            remote.authority.clone(),
        );

        let (local_first, remote_first, local_calls, remote_calls) = accept_with_adapters(
            &mut local.control_plane,
            &mut remote.control_plane,
            local_route.clone(),
            remote_route.clone(),
            local.runtime.clone(),
        );
        assert_eq!(local_calls, 1);
        assert_eq!(remote_calls, 1);
        assert_eq!(local_first, remote_first);
        assert_eq!(local_first.status, LeaseWriteStatus::Accepted);
        assert_eq!(local_first.ack_sequence, ExecutionAckSequence(1));

        let (local_duplicate, remote_duplicate, _, _) = accept_with_adapters(
            &mut local.control_plane,
            &mut remote.control_plane,
            local_route,
            remote_route,
            local.runtime.clone(),
        );
        assert_eq!(local_duplicate, remote_duplicate);
        assert_eq!(local_duplicate.status, LeaseWriteStatus::Duplicate);
        assert_eq!(local_duplicate.ack_sequence, ExecutionAckSequence(1));

        local.control_plane.shutdown().expect("local shutdown");
        remote.control_plane.shutdown().expect("remote shutdown");
        assert_runtime_public_context(&local.root, &local.runtime);
        assert_runtime_public_context(&remote.root, &remote.runtime);
        fs::remove_dir_all(local.root).expect("local database release");
        fs::remove_dir_all(remote.root).expect("remote database release");
    }

    type ReceiptRow = (Vec<u8>, Vec<u8>, String, String, String, i64);

    fn assert_runtime_public_context(root: &Path, message: &RuntimeEventMessage) {
        let connection = Connection::open(root.join("control-plane.sqlite3"))
            .expect("runtime public context database");
        let (occurred_at, source): (Vec<u8>, Vec<u8>) = connection
            .query_row(
                "SELECT public_occurred_at_json,public_source_json FROM outbox \
                 WHERE topic='runtime-projection.invalidated.v1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("runtime public context row");
        let occurred_at: Instant =
            serde_json::from_slice(&occurred_at).expect("runtime public time");
        let source: winwincode_storage::PublicEventSource =
            serde_json::from_slice(&source).expect("runtime public source");
        assert_eq!(occurred_at, message.sent_at);
        assert_eq!(
            source,
            winwincode_storage::PublicEventSource::SessionExecutionWorker {
                worker_id: message.lease.worker_id.clone(),
                worker_session_id: message.worker_session_id.clone(),
                lease_id: message.lease.lease_id.clone(),
                codex_thread_id: message.codex_thread_id.clone(),
                session_identity: message.session_identity.clone(),
            }
        );
    }

    type OutboxRow = (
        String,
        String,
        Vec<u8>,
        i64,
        Option<String>,
        Option<String>,
        Option<i64>,
    );

    #[derive(Debug, PartialEq, Eq)]
    struct RuntimeDurableSnapshot {
        ledger: Vec<(String, i64, Vec<u8>)>,
        receipts: Vec<ReceiptRow>,
        outbox: Vec<OutboxRow>,
        projection_heads: Vec<(Vec<u8>, String, String, i64, String)>,
    }

    fn runtime_snapshot(root: &Path) -> RuntimeDurableSnapshot {
        let connection = Connection::open(root.join("control-plane.sqlite3"))
            .expect("runtime snapshot database");
        let ledger = connection
            .prepare(
                "SELECT stream_id, revision, payload FROM product_state \
                 WHERE stream_id LIKE 'runtime:%' ORDER BY stream_id",
            )
            .expect("runtime ledger query")
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })
            .expect("runtime ledger rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("runtime ledger row decode");
        let receipts = connection
            .prepare(
                "SELECT actor_key, scope_key, request_id, command_digest, stream_id, revision \
                 FROM command_receipts WHERE stream_id LIKE 'runtime:%' \
                 ORDER BY actor_key, scope_key, request_id",
            )
            .expect("runtime receipt query")
            .query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })
            .expect("runtime receipt rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("runtime receipt row decode");
        let outbox = connection
            .prepare(
                "SELECT event_id, topic, payload, published, projection_stream_kind, \
                        projection_resource_id, projection_stream_sequence \
                 FROM outbox WHERE topic IN ('runtime.event.accepted.v1', \
                    'runtime-projection.invalidated.v1') ORDER BY event_id",
            )
            .expect("runtime outbox query")
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                ))
            })
            .expect("runtime outbox rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("runtime outbox row decode");
        let projection_heads = connection
            .prepare(
                "SELECT scope_key, stream_kind, resource_id, sequence, event_id \
                 FROM projection_event_stream_heads ORDER BY scope_key, stream_kind, resource_id",
            )
            .expect("projection cursor query")
            .query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .expect("projection cursor rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("projection cursor row decode");
        connection.close().expect("runtime snapshot close");
        RuntimeDurableSnapshot {
            ledger,
            receipts,
            outbox,
            projection_heads,
        }
    }

    fn assert_runtime_snapshots_equal(
        local: &RuntimeDurableSnapshot,
        remote: &RuntimeDurableSnapshot,
    ) {
        assert_eq!(local.ledger, remote.ledger, "runtime ledger differs");
        assert_eq!(local.receipts, remote.receipts, "runtime receipts differ");
        assert_eq!(local.outbox, remote.outbox, "runtime outbox differs");
        assert_eq!(
            local.projection_heads, remote.projection_heads,
            "projection cursors differ"
        );
    }

    fn authority_with_facts(
        fixture: &RuntimeFixture,
        fencing_token: FencingToken,
        worker_instance_id: WorkerInstanceId,
    ) -> SessionBindingAuthority {
        let active = fixture.authority.active_lease();
        session_binding_authority(
            active_lease_identity(
                active.execution_job_id().clone(),
                active.attempt(),
                active.lease_id().clone(),
                fencing_token,
                active.worker_id().clone(),
                worker_instance_id,
                active.worker_session_id().clone(),
            ),
            fixture.authority.issued_at().clone(),
            fixture.authority.expires_at().clone(),
        )
    }

    fn message_with_new_identity(
        mut message: RuntimeEventMessage,
        seed: u64,
    ) -> RuntimeEventMessage {
        message.message_id = ExecutionMessageId(canonical_id("xmsg", seed));
        message.event.event_id = ExecutionEventId(canonical_id("xevt", seed));
        message
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn runtime_adapter_parity_covers_replay_conflict_gap_stale_and_foreign_without_extra_writes() {
        let seed = 202;
        let mut local = runtime_fixture(seed, "outcomes-local");
        let mut remote = runtime_fixture(seed, "outcomes-remote");
        let local_route =
            RuntimeEventRoute::from_sealed_scheduler(local.scope.clone(), local.authority.clone());
        let remote_route = RuntimeEventRoute::from_sealed_scheduler(
            remote.scope.clone(),
            remote.authority.clone(),
        );
        let baseline_local = runtime_snapshot(&local.root);
        let baseline_remote = runtime_snapshot(&remote.root);
        assert_runtime_snapshots_equal(&baseline_local, &baseline_remote);

        let mut gap = local.runtime.clone();
        gap.event.sequence = ExecutionSequence(2);
        let (local_gap, remote_gap, _, _) = accept_with_adapters(
            &mut local.control_plane,
            &mut remote.control_plane,
            local_route.clone(),
            remote_route.clone(),
            gap,
        );
        assert_eq!(local_gap, remote_gap);
        assert_eq!(local_gap.status, LeaseWriteStatus::Gap);
        assert_runtime_snapshots_equal(
            &runtime_snapshot(&local.root),
            &runtime_snapshot(&remote.root),
        );
        assert_runtime_snapshots_equal(&baseline_local, &runtime_snapshot(&local.root));

        let (local_first, remote_first, _, _) = accept_with_adapters(
            &mut local.control_plane,
            &mut remote.control_plane,
            local_route.clone(),
            remote_route.clone(),
            local.runtime.clone(),
        );
        assert_eq!(local_first, remote_first);
        assert_eq!(local_first.status, LeaseWriteStatus::Accepted);
        let accepted_local = runtime_snapshot(&local.root);
        let accepted_remote = runtime_snapshot(&remote.root);
        assert_runtime_snapshots_equal(&accepted_local, &accepted_remote);
        assert_ne!(accepted_local.ledger, baseline_local.ledger);
        assert_ne!(accepted_local.receipts, baseline_local.receipts);
        assert_ne!(accepted_local.outbox, baseline_local.outbox);
        assert_ne!(
            accepted_local.projection_heads,
            baseline_local.projection_heads
        );

        let (local_duplicate, remote_duplicate, _, _) = accept_with_adapters(
            &mut local.control_plane,
            &mut remote.control_plane,
            local_route.clone(),
            remote_route.clone(),
            local.runtime.clone(),
        );
        assert_eq!(local_duplicate, remote_duplicate);
        assert_eq!(local_duplicate.status, LeaseWriteStatus::Duplicate);
        assert_runtime_snapshots_equal(&accepted_local, &runtime_snapshot(&local.root));
        assert_runtime_snapshots_equal(&accepted_remote, &runtime_snapshot(&remote.root));

        let mut conflict = local.runtime.clone();
        conflict.event.summary = "changed body".into();
        let (local_conflict, remote_conflict, _, _) = accept_with_adapters(
            &mut local.control_plane,
            &mut remote.control_plane,
            local_route.clone(),
            remote_route.clone(),
            conflict,
        );
        assert_eq!(local_conflict, remote_conflict);
        assert_eq!(local_conflict.status, LeaseWriteStatus::RejectedConflict);
        assert_runtime_snapshots_equal(&accepted_local, &runtime_snapshot(&local.root));
        assert_runtime_snapshots_equal(&accepted_remote, &runtime_snapshot(&remote.root));

        let stale_authority = authority_with_facts(
            &local,
            FencingToken((seed + 1).to_string()),
            local.authority.active_lease().worker_instance_id().clone(),
        );
        let stale_local_route =
            RuntimeEventRoute::from_sealed_scheduler(local.scope.clone(), stale_authority);
        let stale_remote_authority = authority_with_facts(
            &remote,
            FencingToken((seed + 1).to_string()),
            remote.authority.active_lease().worker_instance_id().clone(),
        );
        let stale_remote_route =
            RuntimeEventRoute::from_sealed_scheduler(remote.scope.clone(), stale_remote_authority);
        let stale = message_with_new_identity(local.runtime.clone(), seed + 300);
        let (local_stale, remote_stale, _, _) = accept_with_adapters(
            &mut local.control_plane,
            &mut remote.control_plane,
            stale_local_route,
            stale_remote_route,
            stale,
        );
        assert_eq!(local_stale, remote_stale);
        assert_eq!(
            local_stale.status,
            LeaseWriteStatus::RejectedStaleFencingToken
        );
        assert_runtime_snapshots_equal(&accepted_local, &runtime_snapshot(&local.root));
        assert_runtime_snapshots_equal(&accepted_remote, &runtime_snapshot(&remote.root));

        let foreign_authority = authority_with_facts(
            &local,
            local.authority.active_lease().fencing_token().clone(),
            WorkerInstanceId(canonical_id("wki", seed + 1)),
        );
        let foreign_local_route =
            RuntimeEventRoute::from_sealed_scheduler(local.scope.clone(), foreign_authority);
        let foreign_remote_authority = authority_with_facts(
            &remote,
            remote.authority.active_lease().fencing_token().clone(),
            WorkerInstanceId(canonical_id("wki", seed + 1)),
        );
        let foreign_remote_route = RuntimeEventRoute::from_sealed_scheduler(
            remote.scope.clone(),
            foreign_remote_authority,
        );
        let foreign = message_with_new_identity(local.runtime.clone(), seed + 301);
        let (local_foreign, remote_foreign, _, _) = accept_with_adapters(
            &mut local.control_plane,
            &mut remote.control_plane,
            foreign_local_route,
            foreign_remote_route,
            foreign,
        );
        assert_eq!(local_foreign, remote_foreign);
        assert_eq!(
            local_foreign.status,
            LeaseWriteStatus::RejectedWorkerInstance
        );
        assert_runtime_snapshots_equal(&accepted_local, &runtime_snapshot(&local.root));
        assert_runtime_snapshots_equal(&accepted_remote, &runtime_snapshot(&remote.root));

        local.control_plane.shutdown().expect("local shutdown");
        remote.control_plane.shutdown().expect("remote shutdown");
        fs::remove_dir_all(local.root).expect("local database release");
        fs::remove_dir_all(remote.root).expect("remote database release");
    }

    fn action_request(fixture: &RuntimeFixture, seed: u64) -> ActionEnforcementRequestMessage {
        ActionEnforcementRequestMessage {
            job_id: fixture.job.job_id.clone(),
            kind: ActionEnforcementRequestMessageKind::ActionEnforcementRequest,
            lease: fixture.runtime.lease.clone(),
            matched_condition_sha256: vec![Sha256Digest(format!("sha256:{}", "b".repeat(64)))],
            message_id: ExecutionMessageId(canonical_id("xmsg", seed + 700)),
            policy_kind: ActionPolicyKind::Tool,
            request_id: RequestId(canonical_id("req", seed + 700)),
            resource: "action:shell:cwd:.".to_owned(),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: Instant("2027-01-15T08:00:01.100Z".to_owned()),
            session_identity: fixture.runtime.session_identity.clone(),
            subject_sha256: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            worker_session_id: fixture.runtime.worker_session_id.clone(),
        }
    }

    fn seed_action_policy(
        storage: &mut SqliteStorage,
        organization_id: &OrganizationId,
        seed: u64,
    ) {
        let definition = EnterprisePolicyDefinition {
            default_effect: EnterprisePolicyEffect::Allow,
            child_override_mode: EnterprisePolicyChildOverrideMode::TightenOnly,
            rules: Vec::new(),
        };
        let definition_sha256 = Sha256Digest(format!(
            "sha256:{:x}",
            Sha256::digest(
                serde_json::to_vec(
                    &serde_json::to_value(&definition).expect("Policy definition value"),
                )
                .expect("Policy definition"),
            )
        ));
        storage
            .enterprise_policy_ledger()
            .expect("Policy ledger")
            .write(&EnterprisePolicyWrite {
                policy_id: EnterprisePolicyId(canonical_id("pol", seed)),
                policy_kind: EnterprisePolicyKind::Tool,
                scope: EnterprisePolicyScope::Organization {
                    organization_id: organization_id.clone(),
                },
                mode: EnterprisePolicyMode::Enforce,
                state: EnterprisePolicyState::Active,
                definition_sha256,
                definition,
                effective_at: Instant("2027-01-15T07:59:00.000Z".to_owned()),
                inheritance_mode: EnterprisePolicyInheritanceMode::Tighten,
                base_version: None,
                expected_revision: 0,
                source: EnterprisePolicyVersionSource {
                    actor: EnterprisePolicyActor::User {
                        id: UserId(canonical_id("usr", seed)),
                    },
                    request_id: RequestId(canonical_id("req", seed + 702)),
                },
                updated_at: Instant("2027-01-15T07:59:00.000Z".to_owned()),
            })
            .expect("active Tool Policy");
    }

    fn make_action_receipt_bytes_noncanonical(root: &Path) {
        let connection =
            Connection::open(root.join("control-plane.sqlite3")).expect("action receipt database");
        let mut payload = connection
            .query_row(
                "SELECT payload FROM product_state \
                 WHERE stream_id LIKE 'action-enforcement-receipt:%'",
                [],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .expect("action receipt payload");
        payload.insert(0, b' ');
        connection
            .execute(
                "UPDATE product_state SET payload = ?1 \
                 WHERE stream_id LIKE 'action-enforcement-receipt:%'",
                params![payload],
            )
            .expect("tamper action receipt payload");
    }

    #[test]
    fn action_receipt_binds_durable_scope_actor_policy_and_replays_after_restart() {
        let seed = 207;
        let fixture = runtime_fixture(seed, "action-enforcement");
        let root = fixture.root.clone();
        let request = action_request(&fixture, seed);
        let organization_id = fixture.scope.organization_id.clone();
        fixture
            .control_plane
            .shutdown()
            .expect("fixture Control Plane shutdown");
        let issuer = ActionEnforcementIssuer::new(
            ActionEnforcementSigningKey::from_bytes([11_u8; 32]).expect("signing key"),
        );
        let first = {
            let mut storage = SqliteStorage::open(&root).expect("storage reopen");
            install_runtime_lease(&mut storage, &fixture.runtime, &fixture.job, seed);
            seed_action_policy(&mut storage, &organization_id, seed);
            let mut service = winwincode_control_plane::ExecutionPortService::new(
                &mut storage,
                Instant("2027-01-15T08:00:01.200Z".to_owned()),
            );
            let message = service
                .handle_with_action_enforcement(
                    ExecutionPortMessage::ActionEnforcementRequestMessage(request.clone()),
                    &issuer,
                )
                .expect("action receipt");
            let ExecutionPortMessage::ActionEnforcementReceiptMessage(receipt) = message else {
                panic!("action enforcement receipt variant")
            };
            assert_eq!(receipt.decision, ActionEnforcementDecision::Permit);
            assert_eq!(receipt.request_id, request.request_id);
            assert_eq!(receipt.job_id, fixture.job.job_id);
            assert_eq!(receipt.scope, fixture.scope);
            assert_eq!(receipt.actor.id.0, canonical_id("usr", seed));
            let policy = receipt
                .policy_version
                .as_ref()
                .expect("Policy version seal");
            assert_eq!(policy.policy_id, canonical_id("pol", seed));
            assert_eq!(policy.version, 1);
            drop(service);
            Box::new(storage).close().expect("storage close");
            receipt
        };

        let mut restarted = SqliteStorage::open(&root).expect("storage restart");
        let mut service = winwincode_control_plane::ExecutionPortService::new(
            &mut restarted,
            Instant("2027-01-15T08:00:01.300Z".to_owned()),
        );
        let replay = service
            .enforce_action(&issuer, &request)
            .expect("exact action receipt replay");
        assert_eq!(replay, first);

        let mut changed = request.clone();
        changed.resource = "action:shell:changed".to_owned();
        assert!(
            service.enforce_action(&issuer, &changed).is_err(),
            "changed action reuse must fail closed"
        );
        drop(service);
        Box::new(restarted)
            .close()
            .expect("restarted storage close");

        make_action_receipt_bytes_noncanonical(&root);
        let mut corrupted = SqliteStorage::open(&root).expect("corrupt storage reopen");
        let mut corrupt_service = winwincode_control_plane::ExecutionPortService::new(
            &mut corrupted,
            Instant("2027-01-15T08:00:01.400Z".to_owned()),
        );
        assert!(
            corrupt_service.enforce_action(&issuer, &request).is_err(),
            "noncanonical durable receipt bytes must fail closed"
        );
        drop(corrupt_service);
        Box::new(corrupted).close().expect("corrupt storage close");
        fs::remove_dir_all(root).expect("database release");
    }

    #[test]
    fn runtime_replay_request_frame_uses_durable_job_binding_and_lease_authority() {
        let seed = 203;
        let mut fixture = runtime_fixture(seed, "replay-request");
        let root = fixture.root.clone();
        let accepted = fixture
            .control_plane
            .accept_runtime_event(
                &fixture.scope,
                &fixture.runtime,
                &fixture.authority,
                &fixture.runtime.sent_at,
            )
            .expect("durable runtime acknowledgement before reconnect");
        assert_eq!(accepted.status, LeaseWriteStatus::Accepted);
        assert_eq!(accepted.ack_sequence, ExecutionAckSequence(1));
        fixture
            .control_plane
            .shutdown()
            .expect("fixture Control Plane shutdown");

        let mut storage = SqliteStorage::open(&root).expect("storage reopen");
        install_runtime_lease(&mut storage, &fixture.runtime, &fixture.job, seed);
        let mut service = winwincode_control_plane::ExecutionPortService::new(
            &mut storage,
            Instant("2027-01-15T08:00:01.200Z".into()),
        );
        let command = replay_command(seed, fixture.runtime.lease.job_id.clone());

        let frame = service
            .build_runtime_replay_request(command)
            .expect("durable replay request frame");
        assert_eq!(frame.direction(), FrameDirection::ControlPlaneToWorker);
        let ExecutionPortMessage::RuntimeReplayRequestMessage(request) = frame.message() else {
            panic!("builder must return runtime.replay_request");
        };
        assert_eq!(request.after_sequence, ExecutionAckSequence(1));
        assert_eq!(request.max_events, 100);
        assert_eq!(request.message_id.0, canonical_id("xmsg", seed + 500));
        assert_eq!(request.request_id.0, canonical_id("req", seed + 500));
        assert_eq!(request.worker_session_id, fixture.runtime.worker_session_id);
        assert_eq!(request.session_identity, fixture.runtime.session_identity);
        assert_eq!(request.lease, fixture.runtime.lease);

        drop(service);
        Box::new(storage).close().expect("storage close");
        fs::remove_dir_all(root).expect("database release");
    }

    #[test]
    fn runtime_replay_request_rejects_expired_or_foreign_lease_without_a_frame() {
        let stale_seed = 204;
        let stale_fixture = runtime_fixture(stale_seed, "replay-request-stale");
        let stale_root = stale_fixture.root.clone();
        let stale_job_id = stale_fixture.runtime.lease.job_id.clone();
        stale_fixture
            .control_plane
            .shutdown()
            .expect("stale fixture Control Plane shutdown");
        let mut stale_storage = SqliteStorage::open(&stale_root).expect("stale storage reopen");
        install_runtime_lease(
            &mut stale_storage,
            &stale_fixture.runtime,
            &stale_fixture.job,
            stale_seed,
        );
        let mut stale_service = winwincode_control_plane::ExecutionPortService::new(
            &mut stale_storage,
            Instant("2027-01-15T08:05:00.001Z".into()),
        );
        let stale_error = stale_service
            .build_runtime_replay_request(replay_command(stale_seed, stale_job_id))
            .expect_err("expired lease must not produce a replay frame");
        assert!(matches!(
            stale_error,
            ExecutionPortServiceError::AuthorityRejected("current execution lease is expired")
        ));
        drop(stale_service);
        Box::new(stale_storage)
            .close()
            .expect("stale storage close");
        fs::remove_dir_all(stale_root).expect("stale database release");

        let foreign_seed = 205;
        let foreign_fixture = runtime_fixture(foreign_seed, "replay-request-foreign");
        let foreign_root = foreign_fixture.root.clone();
        let foreign_job_id = foreign_fixture.runtime.lease.job_id.clone();
        foreign_fixture
            .control_plane
            .shutdown()
            .expect("foreign fixture Control Plane shutdown");
        let mut foreign_storage =
            SqliteStorage::open(&foreign_root).expect("foreign storage reopen");
        install_runtime_lease(
            &mut foreign_storage,
            &foreign_fixture.runtime,
            &foreign_fixture.job,
            foreign_seed,
        );
        let connection = Connection::open(foreign_root.join("control-plane.sqlite3"))
            .expect("foreign registry connection");
        connection
            .execute(
                "UPDATE execution_leases SET worker_instance_id = ?1 WHERE job_id = ?2",
                params![
                    canonical_id("wki", foreign_seed + 1),
                    foreign_job_id.0.clone()
                ],
            )
            .expect("foreign lease mutation");
        connection.close().expect("foreign registry close");
        let mut foreign_service = winwincode_control_plane::ExecutionPortService::new(
            &mut foreign_storage,
            Instant("2027-01-15T08:00:01.200Z".into()),
        );
        let foreign_error = foreign_service
            .build_runtime_replay_request(replay_command(foreign_seed, foreign_job_id))
            .expect_err("foreign lease must not produce a replay frame");
        assert!(matches!(
            foreign_error,
            ExecutionPortServiceError::AuthorityRejected(
                "current execution lease is foreign or stale"
            )
        ));
        drop(foreign_service);
        Box::new(foreign_storage)
            .close()
            .expect("foreign storage close");
        fs::remove_dir_all(foreign_root).expect("foreign database release");
    }

    #[test]
    fn runtime_replay_request_rejects_pending_binding_before_lease_lookup() {
        let seed = 206;
        let root = temporary_directory("replay-request-pending");
        let pending = pending_execution(seed);
        seed_delivery(&root, &delivery_before_advance(seed));
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane start");
        control_plane
            .commit_delivery_execution(
                &delivery_advance_command(seed),
                &pending,
                &mut RecordingDispatcher,
            )
            .expect("Delivery execution commit");
        control_plane
            .shutdown()
            .expect("pending fixture Control Plane shutdown");

        let mut storage = SqliteStorage::open(&root).expect("pending storage reopen");
        let mut service = winwincode_control_plane::ExecutionPortService::new(
            &mut storage,
            Instant("2027-01-15T08:00:01.200Z".into()),
        );
        let error = service
            .build_runtime_replay_request(replay_command(seed, pending.job().job_id.clone()))
            .expect_err("pending binding must not produce a replay frame");
        assert!(matches!(
            error,
            ExecutionPortServiceError::AuthorityRejected("SessionBinding WorkerSession is pending")
        ));

        drop(service);
        Box::new(storage).close().expect("pending storage close");
        fs::remove_dir_all(root).expect("pending database release");
    }

    #[derive(Default)]
    struct CapturingJournal {
        publication: Mutex<Option<AtomicPublication>>,
    }

    impl DeliveryJournalPort for CapturingJournal {
        fn load(
            &self,
            _delivery_id: &DeliveryId,
        ) -> Result<Option<LoadedDeliveryJournal>, JournalBackendError> {
            Ok(None)
        }

        fn publish(&self, publication: AtomicPublication) -> Result<(), JournalBackendError> {
            *self.publication.lock().expect("publication lock") = Some(publication);
            Ok(())
        }
    }

    fn seed_delivery(root: &Path, delivery: &Delivery) {
        let capture = CapturingJournal::default();
        DeliveryStore::borrowed(&capture)
            .execute(DeliveryCommand::SeedForTest(CreateDelivery {
                request_id: RequestId("c".repeat(64)),
                request_digest: "b".repeat(64),
                snapshot: delivery.clone(),
            }))
            .expect("seed Delivery journal publication");
        let AtomicPublication::Create {
            delivery_id,
            manifest,
            first_record,
        } = capture
            .publication
            .into_inner()
            .expect("publication lock")
            .expect("seed publication")
        else {
            panic!("seed must create the Delivery journal");
        };
        let publication = AggregateJournalPublication::Create {
            key: AggregateJournalKey::new("delivery", delivery_id.0).expect("journal key"),
            manifest,
            first_record: AggregateJournalRecord::new(
                first_record.sequence,
                first_record.digest,
                first_record.bytes,
            ),
        };
        let mut storage = SqliteStorage::open(root).expect("seed storage");
        let receipt = storage
            .commit(
                &StateCommit::new(
                    ReceiptIdentity::new(
                        ReceiptActorKey::from_encoded(b"seed-actor".to_vec()).expect("seed actor"),
                        ReceiptScopeKey::from_encoded(b"seed-scope".to_vec()).expect("seed scope"),
                        RequestId("c".repeat(64)),
                    )
                    .expect("seed identity"),
                    Sha256Digest(format!("sha256:{}", "b".repeat(64))),
                    format!("delivery:{}", delivery.id().0),
                    0,
                    delivery.encode_json().expect("seed Delivery JSON"),
                    vec![NewOutboxEvent::internal(
                        format!("seed-event-{}", delivery.id().0),
                        "delivery.seeded",
                        b"seed".to_vec(),
                    )],
                )
                .with_journal_publication(publication),
            )
            .expect("seed transaction");
        storage
            .mark_published(&receipt.events[0].event_id)
            .expect("seed event acknowledgement");
        Box::new(storage).close().expect("seed storage close");
    }

    #[derive(Default)]
    struct RecordingPublisher;

    impl EventPublisher for RecordingPublisher {
        fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingDispatcher;

    impl ExecutionJobDispatcher for RecordingDispatcher {
        fn dispatch(&mut self, _job: &ExecutionJob) -> Result<(), DeliveryExecutionPortError> {
            Ok(())
        }
    }
}
