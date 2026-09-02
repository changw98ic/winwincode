// SPDX-License-Identifier: Apache-2.0

//! Local/remote Worker transport parity and browser-visible Fleet recovery.

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use serde_json::{Value, from_value};
use winwincode_api::generated::{
    Actor, EnterpriseFleetListParameters, EnterpriseFleetListQuery, EnterpriseFleetListQueryQuery,
    PageRequest, RepositoryScope, RepositoryScopeKind, Scope, SystemActor, SystemActorKind,
};
use winwincode_control_plane::{
    ExecutionPortService, RemoteWorkerAuthenticationError, RemoteWorkerAuthenticator,
    RemoteWorkerConnection, RemoteWorkerConnectionState, RemoteWorkerCredential,
    RemoteWorkerPoolAdapter, RemoteWorkerPrincipal, WorkerFleetProjectionService,
};
use winwincode_domain::{
    ExecutionMessageId, FencingToken, Instant, LeaseId, OpaqueCursor, OrganizationId, ProjectId,
    RepositoryId, RequestId, SchemaVersion, Sha256Digest, SystemActorId, WorkerInstanceId,
    WorkspaceId,
};
use winwincode_execution_port::{
    generated::{
        ExecutionPortErrorCode, ExecutionPortMessage, ExecutionScope, JobDispatchMessage,
        JobDispatchResultMessage, JobDispatchResultMessageStatus, WorkerHeartbeatMessage,
        WorkerRegisterMessage, WorkerRegistrationResultMessageLeaseRecovery,
        WorkerRegistrationResultMessageStatus,
    },
    transport::{
        EndpointSide, FrameDirection, LocalWorkerAdapter, RemoteTransportAdapter, TypedFrame,
    },
};
use winwincode_storage::{
    ExecutionJobSubmission, ExecutionLeaseClaim, ExecutionLeaseTerminalOutcome,
    ExecutionLeaseTerminalRequest, ExecutionQueueScope, LeaseWriteStatus, NewOutboxEvent,
    ProductStateStorage, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey, SqliteStorage,
    StateCommit, WorkerPoolId, WorkerRegistryScope,
};

const REMOTE_PROOF: &[u8] = b"REMOTE_WORKER_FIXTURE_PROOF";
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "winwincode-local-remote-recovery-{label}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        Self(root)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone)]
struct FixedAuthenticator {
    principal: RemoteWorkerPrincipal,
}

impl RemoteWorkerAuthenticator for FixedAuthenticator {
    fn authenticate(
        &self,
        credential: &RemoteWorkerCredential,
        _now: &Instant,
    ) -> Result<RemoteWorkerPrincipal, RemoteWorkerAuthenticationError> {
        if credential.expose_for_verification() == REMOTE_PROOF {
            Ok(self.principal.clone())
        } else {
            Err(RemoteWorkerAuthenticationError::rejected())
        }
    }

    fn ensure_active(
        &self,
        principal: &RemoteWorkerPrincipal,
        _now: &Instant,
    ) -> Result<(), RemoteWorkerAuthenticationError> {
        if principal == &self.principal {
            Ok(())
        } else {
            Err(RemoteWorkerAuthenticationError::rejected())
        }
    }
}

fn fixture_message<T: serde::de::DeserializeOwned>(kind: &str) -> T {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .expect("canonical Execution Port fixture");
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

fn commit_durable_dispatch_intent(storage: &mut SqliteStorage, dispatch: &JobDispatchMessage) {
    let job = &dispatch.job;
    let (product_session_id, delivery_id, stage_run_id) = match &job.scope {
        ExecutionScope::ProductSessionExecutionScope(scope) => {
            (scope.product_session_id.clone(), None, None)
        }
        ExecutionScope::DeliveryStageExecutionScope(scope) => (
            scope.product_session_id.clone(),
            Some(scope.delivery_id.clone()),
            Some(scope.stage_run_id.clone()),
        ),
    };
    let identity = ReceiptIdentity::new(
        ReceiptActorKey::from_encoded(b"local-remote-adapter-fixture".to_vec())
            .expect("fixture actor"),
        ReceiptScopeKey::from_encoded(format!("adapter-job:{}", job.job_id.0).into_bytes())
            .expect("fixture scope"),
        RequestId("req_00000000000000000000000901".to_owned()),
    )
    .expect("fixture receipt identity");
    storage
        .commit(&StateCommit::new(
            identity,
            Sha256Digest(format!("sha256:{}", "d".repeat(64))),
            format!("delivery-execution-intent:{}", job.job_id.0),
            0,
            b"{}".to_vec(),
            vec![NewOutboxEvent::internal(
                format!("execution-job:{}", job.job_id.0),
                "execution.job.dispatch",
                serde_json::to_vec(job).expect("wire Job payload"),
            )],
        ))
        .expect("durable dispatch intent");
    storage
        .execution_queue()
        .expect("execution queue")
        .submit(&ExecutionJobSubmission {
            scope: ExecutionQueueScope {
                organization_id: OrganizationId("org_00000000000000000000000901".to_owned()),
                workspace_id: WorkspaceId("wsp_00000000000000000000000902".to_owned()),
                project_id: ProjectId("prj_00000000000000000000000903".to_owned()),
                repository_id: job.workspace.repository_id.clone(),
                product_session_id,
                delivery_id,
            },
            job_id: job.job_id.clone(),
            request_id: RequestId("req_00000000000000000000000902".to_owned()),
            payload_digest: job.payload_digest.clone(),
            dispatch_payload: serde_json::to_vec(job).expect("wire Job payload"),
            attempt: u64::try_from(job.attempt).expect("positive Job attempt"),
            dependencies: Vec::new(),
            stage_run_id,
            submitted_at: dispatch.sent_at.clone(),
        })
        .expect("durable scheduler Job");
}

fn accept_local(
    service: &mut ExecutionPortService<'_>,
    message: ExecutionPortMessage,
) -> ExecutionPortMessage {
    let frame =
        TypedFrame::new(FrameDirection::WorkerToControlPlane, message).expect("local Worker frame");
    LocalWorkerAdapter::new(service, EndpointSide::ControlPlane)
        .accept(&frame)
        .expect("local adapter output")
}

fn accept_remote_transport(
    service: &mut ExecutionPortService<'_>,
    message: ExecutionPortMessage,
) -> ExecutionPortMessage {
    let frame = TypedFrame::new(FrameDirection::WorkerToControlPlane, message)
        .expect("remote Worker frame");
    let encoded = RemoteTransportAdapter::<ExecutionPortService<'_>>::encode(&frame)
        .expect("remote frame encoding");
    RemoteTransportAdapter::new(service, EndpointSide::ControlPlane)
        .accept(&encoded)
        .expect("remote adapter output")
}

fn register_heartbeat_and_claim(
    storage: &mut SqliteStorage,
    dispatch: &JobDispatchMessage,
    remote_transport: bool,
) {
    let mut service =
        ExecutionPortService::new(storage, Instant("2026-08-24T12:00:01.000Z".to_owned()));
    let register = ExecutionPortMessage::WorkerRegisterMessage(fixture_message("worker.register"));
    let mut heartbeat: WorkerHeartbeatMessage = fixture_message("worker.heartbeat");
    heartbeat.active_leases.clear();
    heartbeat.capacity.running_jobs = 0;
    heartbeat.capacity.available_slots = 4;
    if remote_transport {
        accept_remote_transport(&mut service, register);
        accept_remote_transport(
            &mut service,
            ExecutionPortMessage::WorkerHeartbeatMessage(heartbeat),
        );
    } else {
        accept_local(&mut service, register);
        accept_local(
            &mut service,
            ExecutionPortMessage::WorkerHeartbeatMessage(heartbeat),
        );
    }
    service
        .claim_execution_job(dispatch.job.clone(), claim_from_dispatch(dispatch))
        .expect("durable Job claim");
}

fn result_status(message: &ExecutionPortMessage) -> &JobDispatchResultMessage {
    let ExecutionPortMessage::JobDispatchResultMessage(result) = message else {
        panic!("dispatch-result response")
    };
    result
}

fn assert_public_fault(message: &ExecutionPortMessage, status: &str, code: &str) {
    let public = serde_json::to_value(message).expect("public fault JSON");
    assert_eq!(public["status"], status);
    assert_eq!(public["error"]["code"], code);
}

fn replacement_registration() -> WorkerRegisterMessage {
    let mut replacement: WorkerRegisterMessage = fixture_message("worker.register");
    replacement.message_id = ExecutionMessageId("xmsg_00000000000000000000000920".to_owned());
    replacement.request_id = RequestId("req_00000000000000000000000920".to_owned());
    replacement.worker_instance_id = WorkerInstanceId("wki_00000000000000000000000920".to_owned());
    replacement.started_at = Instant("2026-08-24T12:00:02.000Z".to_owned());
    replacement.sent_at = replacement.started_at.clone();
    replacement
}

fn exercise_faults_after_restart(
    storage: &mut SqliteStorage,
    remote_transport: bool,
) -> Vec<ExecutionPortMessage> {
    let mut service =
        ExecutionPortService::new(storage, Instant("2026-08-24T12:00:03.000Z".to_owned()));
    let accept = |service: &mut ExecutionPortService<'_>, message| {
        if remote_transport {
            accept_remote_transport(service, message)
        } else {
            accept_local(service, message)
        }
    };
    let mut stale: JobDispatchResultMessage = fixture_message("job.dispatch_result");
    stale.request_id = RequestId("req_00000000000000000000000921".to_owned());
    stale.lease.fencing_token = FencingToken("41".to_owned());
    let stale = accept(
        &mut service,
        ExecutionPortMessage::JobDispatchResultMessage(stale),
    );

    let mut expired: JobDispatchResultMessage = fixture_message("job.dispatch_result");
    expired.request_id = RequestId("req_00000000000000000000000922".to_owned());
    expired.sent_at = expired.lease.expires_at.clone();
    let expired = accept(
        &mut service,
        ExecutionPortMessage::JobDispatchResultMessage(expired),
    );
    let replacement = accept(
        &mut service,
        ExecutionPortMessage::WorkerRegisterMessage(replacement_registration()),
    );
    let mut old_instance: JobDispatchResultMessage = fixture_message("job.dispatch_result");
    old_instance.request_id = RequestId("req_00000000000000000000000923".to_owned());
    let old_instance = accept(
        &mut service,
        ExecutionPortMessage::JobDispatchResultMessage(old_instance),
    );
    vec![stale, expired, replacement, old_instance]
}

fn assert_zero_fault_receipts(storage: &mut SqliteStorage, dispatch: &JobDispatchMessage) {
    let registry = storage.execution_registry().expect("execution Registry");
    for request in [921_u64, 922, 923] {
        assert!(
            !registry
                .has_request(
                    "dispatch_result",
                    &dispatch.job.job_id,
                    &RequestId(format!("req_{request:026}")),
                )
                .expect("fault receipt query"),
            "rejected fault must not create a durable request receipt"
        );
    }
    let lease = registry
        .load_lease(&dispatch.job.job_id)
        .expect("lease read")
        .expect("durable lease");
    assert_eq!(lease.lease_id, dispatch.lease.lease_id);
    assert_eq!(lease.fencing_token, dispatch.lease.fencing_token);
    assert_eq!(lease.worker_instance_id, dispatch.lease.worker_instance_id);
}

#[test]
fn local_and_remote_transports_replay_faults_after_restart_without_double_writes() {
    let local_root = TestDirectory::new("transport-local");
    let remote_root = TestDirectory::new("transport-remote");
    let expected: JobDispatchMessage = fixture_message("job.dispatch");
    for (root, remote_transport) in [(&local_root, false), (&remote_root, true)] {
        let mut storage = SqliteStorage::open(&root.0).expect("transport storage");
        commit_durable_dispatch_intent(&mut storage, &expected);
        register_heartbeat_and_claim(&mut storage, &expected, remote_transport);
        Box::new(storage).close().expect("transport storage close");
    }

    let mut local_storage = SqliteStorage::open(&local_root.0).expect("local restart");
    let mut remote_storage = SqliteStorage::open(&remote_root.0).expect("remote restart");
    let local = exercise_faults_after_restart(&mut local_storage, false);
    let remote = exercise_faults_after_restart(&mut remote_storage, true);
    assert_eq!(
        local, remote,
        "generated fault DTOs must be adapter-identical"
    );

    assert_eq!(
        result_status(&local[0]).status,
        JobDispatchResultMessageStatus::RejectedStaleFencingToken
    );
    assert_eq!(
        result_status(&local[0])
            .error
            .as_ref()
            .map(|error| &error.code),
        Some(&ExecutionPortErrorCode::StaleFencingToken)
    );
    assert_public_fault(
        &local[0],
        "rejected_stale_fencing_token",
        "STALE_FENCING_TOKEN",
    );
    assert_eq!(
        result_status(&local[1]).status,
        JobDispatchResultMessageStatus::RejectedExpiredLease
    );
    assert_eq!(
        result_status(&local[1])
            .error
            .as_ref()
            .map(|error| &error.code),
        Some(&ExecutionPortErrorCode::LeaseExpired)
    );
    assert_public_fault(&local[1], "rejected_expired_lease", "LEASE_EXPIRED");
    let ExecutionPortMessage::WorkerRegistrationResultMessage(replacement) = &local[2] else {
        panic!("replacement registration result")
    };
    assert_eq!(
        replacement.status,
        WorkerRegistrationResultMessageStatus::Accepted
    );
    assert_eq!(
        replacement.lease_recovery,
        WorkerRegistrationResultMessageLeaseRecovery::ReacquireRequired
    );
    assert_eq!(
        result_status(&local[3]).status,
        JobDispatchResultMessageStatus::RejectedWorkerInstance
    );
    assert_eq!(
        result_status(&local[3])
            .error
            .as_ref()
            .map(|error| &error.code),
        Some(&ExecutionPortErrorCode::WorkerInstanceChanged)
    );
    assert_public_fault(
        &local[3],
        "rejected_worker_instance",
        "WORKER_INSTANCE_CHANGED",
    );
    assert_zero_fault_receipts(&mut local_storage, &expected);
    assert_zero_fault_receipts(&mut remote_storage, &expected);
}

fn repository_scope() -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId("org_00000000000000000000000901".to_owned()),
        workspace_id: WorkspaceId("wsp_00000000000000000000000902".to_owned()),
        project_id: ProjectId("prj_00000000000000000000000903".to_owned()),
        repository_id: RepositoryId("rep_00000000000000000000000904".to_owned()),
    }
}

fn registry_scope(scope: &RepositoryScope) -> WorkerRegistryScope {
    WorkerRegistryScope::Repository {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
    }
}

fn remote_authenticator(scope: &RepositoryScope) -> FixedAuthenticator {
    FixedAuthenticator {
        principal: RemoteWorkerPrincipal::new(
            fixture_message::<WorkerRegisterMessage>("worker.register").worker_id,
            WorkerPoolId("wpl_00000000000000000000000905".to_owned()),
            registry_scope(scope),
            "remote-worker-issuer".to_owned(),
            "remote-worker-subject".to_owned(),
            Sha256Digest(format!("sha256:{}", "9".repeat(64))),
            "remote-zone".to_owned(),
        )
        .expect("remote principal"),
    }
}

fn credential() -> RemoteWorkerCredential {
    RemoteWorkerCredential::new(REMOTE_PROOF.to_vec()).expect("remote credential")
}

fn connect_and_register(
    storage: &mut SqliteStorage,
    authenticator: &FixedAuthenticator,
    registration: &WorkerRegisterMessage,
) -> (RemoteWorkerConnection, ExecutionPortMessage) {
    let mut adapter = RemoteWorkerPoolAdapter::new(storage, authenticator);
    let mut connection = adapter
        .connect(&credential(), &registration.sent_at)
        .expect("authenticated remote connection");
    let response = adapter
        .accept(
            &mut connection,
            &ExecutionPortMessage::WorkerRegisterMessage(registration.clone()),
            &registration.sent_at,
        )
        .expect("remote Worker registration");
    (connection, response)
}

fn fleet_query(scope: &RepositoryScope, request: u64) -> EnterpriseFleetListQuery {
    EnterpriseFleetListQuery {
        actor: Actor::SystemActor(SystemActor {
            id: SystemActorId(format!("sys_{request:026}")),
            kind: SystemActorKind::System,
        }),
        page: PageRequest {
            cursor: Option::<OpaqueCursor>::None,
            limit: 10,
        },
        parameters: EnterpriseFleetListParameters { states: Vec::new() },
        query: EnterpriseFleetListQueryQuery::EnterpriseFleetList,
        request_id: RequestId(format!("req_{request:026}")),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::RepositoryScope(scope.clone()),
    }
}

fn assert_public_fleet(
    storage: &mut SqliteStorage,
    scope: &RepositoryScope,
    observed_at: &Instant,
    state: &str,
    active_leases: i64,
    available_capacity: i64,
) {
    let response = WorkerFleetProjectionService::with_stale_after_ms(storage, 5_000)
        .list(&fleet_query(scope, 950), observed_at)
        .expect("generated Fleet projection");
    assert_eq!(response.result.items.len(), 1);
    let pool = &response.result.items[0];
    assert_eq!(pool.state, state);
    assert_eq!(pool.active_leases, active_leases);
    assert_eq!(pool.available_capacity, available_capacity);
    assert_eq!(pool.registered_workers, 1);
    assert!(response.result.snapshot_revision.0 >= 1);
    let public = serde_json::to_value(&response).expect("public Fleet JSON");
    assert_eq!(public["result"]["items"][0]["state"], state);
    assert_eq!(public["result"]["items"][0]["activeLeases"], active_leases);
    assert_eq!(
        public["result"]["items"][0]["availableCapacity"],
        available_capacity
    );
}

fn heartbeat_for(
    registration: &WorkerRegisterMessage,
    sequence: i64,
    second: u64,
    active: bool,
) -> WorkerHeartbeatMessage {
    let mut heartbeat: WorkerHeartbeatMessage = fixture_message("worker.heartbeat");
    heartbeat.worker_id = registration.worker_id.clone();
    heartbeat.worker_instance_id = registration.worker_instance_id.clone();
    heartbeat.heartbeat_sequence = winwincode_domain::ExecutionSequence(sequence);
    heartbeat.message_id = ExecutionMessageId(format!("xmsg_{:026}", 960 + second));
    heartbeat.observed_at = Instant(format!("2026-08-24T12:00:{second:02}.000Z"));
    heartbeat.sent_at = heartbeat.observed_at.clone();
    if active {
        heartbeat.capacity.running_jobs = 1;
        heartbeat.capacity.available_slots = 3;
    } else {
        heartbeat.active_leases.clear();
        heartbeat.capacity.running_jobs = 0;
        heartbeat.capacity.available_slots = 4;
    }
    heartbeat
}

fn remote_accept(
    storage: &mut SqliteStorage,
    authenticator: &FixedAuthenticator,
    connection: &mut RemoteWorkerConnection,
    message: &ExecutionPortMessage,
    now: &Instant,
) -> ExecutionPortMessage {
    RemoteWorkerPoolAdapter::new(storage, authenticator)
        .accept(connection, message, now)
        .expect("remote Worker message")
}

fn activate_remote_worker(
    storage: &mut SqliteStorage,
    authenticator: &FixedAuthenticator,
    registration: &WorkerRegisterMessage,
    lease: &ExecutionLeaseClaim,
) -> RemoteWorkerConnection {
    let (mut connection, registered) = connect_and_register(storage, authenticator, registration);
    let ExecutionPortMessage::WorkerRegistrationResultMessage(registered) = registered else {
        panic!("remote registration result")
    };
    assert_eq!(
        registered.status,
        WorkerRegistrationResultMessageStatus::Accepted
    );
    assert_eq!(connection.state(), RemoteWorkerConnectionState::Registered);
    assert_eq!(
        storage
            .execution_registry()
            .expect("execution Registry")
            .claim_execution_job_with_authenticated_placement(lease)
            .expect("authenticated lease claim")
            .status,
        LeaseWriteStatus::Accepted
    );

    let mut stale: JobDispatchResultMessage = fixture_message("job.dispatch_result");
    stale.request_id = RequestId("req_00000000000000000000000973".to_owned());
    stale.lease.fencing_token = FencingToken("41".to_owned());
    let stale = remote_accept(
        storage,
        authenticator,
        &mut connection,
        &ExecutionPortMessage::JobDispatchResultMessage(stale),
        &Instant("2026-08-24T12:00:01.000Z".to_owned()),
    );
    assert_public_fault(
        &stale,
        "rejected_stale_fencing_token",
        "STALE_FENCING_TOKEN",
    );
    let mut expired: JobDispatchResultMessage = fixture_message("job.dispatch_result");
    expired.request_id = RequestId("req_00000000000000000000000974".to_owned());
    expired.sent_at = expired.lease.expires_at.clone();
    let expired = remote_accept(
        storage,
        authenticator,
        &mut connection,
        &ExecutionPortMessage::JobDispatchResultMessage(expired),
        &Instant("2026-08-24T12:00:02.000Z".to_owned()),
    );
    assert_public_fault(&expired, "rejected_expired_lease", "LEASE_EXPIRED");
    let registry = storage.execution_registry().expect("execution Registry");
    for request in [973_u64, 974] {
        assert!(
            !registry
                .has_request(
                    "dispatch_result",
                    &lease.job_id,
                    &RequestId(format!("req_{request:026}")),
                )
                .expect("remote fault receipt query")
        );
    }

    let active_heartbeat = heartbeat_for(registration, 1, 1, true);
    remote_accept(
        storage,
        authenticator,
        &mut connection,
        &ExecutionPortMessage::WorkerHeartbeatMessage(active_heartbeat.clone()),
        &active_heartbeat.observed_at,
    );
    connection
}

fn disconnect_and_replace(
    storage: &mut SqliteStorage,
    authenticator: &FixedAuthenticator,
    scope: &RepositoryScope,
    connection: &mut RemoteWorkerConnection,
    replacement: &WorkerRegisterMessage,
    lease: &ExecutionLeaseClaim,
) -> RemoteWorkerConnection {
    assert_public_fleet(
        storage,
        scope,
        &Instant("2026-08-24T12:00:02.000Z".to_owned()),
        "healthy",
        1,
        3,
    );

    assert!(
        RemoteWorkerPoolAdapter::new(storage, authenticator)
            .disconnect(connection)
            .expect("remote disconnect")
    );
    assert_eq!(
        connection.state(),
        RemoteWorkerConnectionState::Disconnected
    );
    assert_public_fleet(
        storage,
        scope,
        &Instant("2026-08-24T12:00:03.000Z".to_owned()),
        "offline",
        1,
        0,
    );

    let (replacement_connection, replacement_result) =
        connect_and_register(storage, authenticator, replacement);
    let ExecutionPortMessage::WorkerRegistrationResultMessage(replacement_result) =
        replacement_result
    else {
        panic!("remote replacement result")
    };
    assert_eq!(
        replacement_result.lease_recovery,
        WorkerRegistrationResultMessageLeaseRecovery::ReacquireRequired
    );
    let mut duplicate_execution = lease.clone();
    duplicate_execution.worker_instance_id = replacement.worker_instance_id.clone();
    duplicate_execution.lease_id = LeaseId("lse_00000000000000000000000971".to_owned());
    duplicate_execution.message_id =
        ExecutionMessageId("xmsg_00000000000000000000000971".to_owned());
    duplicate_execution.request_id = RequestId("req_00000000000000000000000971".to_owned());
    duplicate_execution.fencing_token = FencingToken("43".to_owned());
    duplicate_execution.attempt = 2;
    duplicate_execution.issued_at = Instant("2026-08-24T12:00:04.000Z".to_owned());
    let duplicate_receipt = storage
        .execution_registry()
        .expect("execution Registry")
        .claim_execution_job_with_authenticated_placement(&duplicate_execution)
        .expect("duplicate execution decision");
    assert_eq!(duplicate_receipt.status, LeaseWriteStatus::RejectedConflict);
    assert!(
        !storage
            .execution_registry()
            .expect("execution Registry")
            .has_request("claim", &lease.job_id, &duplicate_execution.request_id)
            .expect("duplicate execution receipt query")
    );
    replacement_connection
}

fn finish_and_report_capacity(
    storage: &mut SqliteStorage,
    authenticator: &FixedAuthenticator,
    connection: &mut RemoteWorkerConnection,
    replacement: &WorkerRegisterMessage,
    lease: &ExecutionLeaseClaim,
) -> ExecutionLeaseTerminalRequest {
    let terminal = ExecutionLeaseTerminalRequest {
        job_id: lease.job_id.clone(),
        lease_id: lease.lease_id.clone(),
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
        attempt: lease.attempt,
        fencing_token: lease.fencing_token.clone(),
        outcome: ExecutionLeaseTerminalOutcome::Completed,
        terminal_at: Instant("2026-08-24T12:00:05.000Z".to_owned()),
        request_id: RequestId("req_00000000000000000000000972".to_owned()),
    };
    assert!(
        storage
            .execution_registry()
            .expect("execution Registry")
            .finish_execution_lease(&terminal)
            .expect("terminal lease")
    );
    let recovered_heartbeat = heartbeat_for(replacement, 1, 5, false);
    remote_accept(
        storage,
        authenticator,
        connection,
        &ExecutionPortMessage::WorkerHeartbeatMessage(recovered_heartbeat.clone()),
        &recovered_heartbeat.observed_at,
    );
    terminal
}

#[test]
fn remote_disconnect_replacement_and_terminal_restore_browser_fleet_after_restart() {
    let root = TestDirectory::new("remote-fleet");
    let scope = repository_scope();
    let authenticator = remote_authenticator(&scope);
    let registration: WorkerRegisterMessage = fixture_message("worker.register");
    let dispatch: JobDispatchMessage = fixture_message("job.dispatch");
    let lease = claim_from_dispatch(&dispatch);
    let mut storage = SqliteStorage::open(&root.0).expect("remote Fleet storage");
    commit_durable_dispatch_intent(&mut storage, &dispatch);
    let mut connection =
        activate_remote_worker(&mut storage, &authenticator, &registration, &lease);
    let replacement = replacement_registration();
    let mut replacement_connection = disconnect_and_replace(
        &mut storage,
        &authenticator,
        &scope,
        &mut connection,
        &replacement,
        &lease,
    );
    let terminal = finish_and_report_capacity(
        &mut storage,
        &authenticator,
        &mut replacement_connection,
        &replacement,
        &lease,
    );
    Box::new(storage).close().expect("remote Fleet close");

    let mut restarted = SqliteStorage::open(&root.0).expect("remote Fleet restart");
    assert!(
        !restarted
            .execution_registry()
            .expect("execution Registry")
            .finish_execution_lease(&terminal)
            .expect("exact terminal replay")
    );
    assert_public_fleet(
        &mut restarted,
        &scope,
        &Instant("2026-08-24T12:00:06.000Z".to_owned()),
        "healthy",
        0,
        4,
    );
}
