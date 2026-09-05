// SPDX-License-Identifier: Apache-2.0

//! The FLOW-100.4 Quick Chat native Codex continuous execution path over the
//! real composed Server application: a `chat.submit` turn of a session with a
//! durable launch anchor is dispatched to the launched Device `WorkerSession`
//! (the launch material becomes the device session's bound `ExecutionPort`
//! identity and the queued job receives its device facts, which exclude it
//! from every local claim), while a session without an anchor keeps the
//! supervised local execution path unchanged. A permission-gate denial
//! dispatches nothing.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use winwincode_api::generated::{
    Actor, CommandRequest, OrganizationScope, OrganizationScopeKind, Scope, UserActor,
    UserActorKind,
};
use winwincode_control_plane::{
    ControlPlane, ControlPlaneConfig, DeviceExecutionBindingService,
    DurableWorkerInteractionOutbound, EventPublishError, EventPublisher, OutboxEvent,
    ProductSessionExecutionConfig, RepositoryExecutionScheduler, WorkerLaunchGrantService,
};
use winwincode_domain::{
    ExecutionJobId, ExecutionMessageId, Instant, OrganizationId, ProjectId, RepositoryId,
    RequestId, Sha256Digest, UserId, WorkerId, WorkerInstanceId, WorkspaceId,
};
use winwincode_server::{
    ApiError, AuthenticatedPrincipal, CommandDispatchResponse, CommandFamily, DurableEventHub,
    DurableEventHubConfig, StandaloneApplicationClock, StandaloneControlPlaneApplication,
    TypedControlPlaneApiPort,
};
use winwincode_storage::{
    AccessGrantIssuance, ClientNodeRegistration, ClientPresenceState, EXECUTION_PROTOCOL_VERSION,
    ExecutionJobState, GrantPermissions, GrantSource, GrantTrustMode, LaunchAckSettlement,
    LaunchGrantIssuance, OccupancyClaim, OccupancyLeaseState, ProductStateStorage,
    RepositoryAccessGrantIssuance, RepositoryAvailability, RepositoryBindingProjection,
    RepositoryDirtyState, RepositoryGrantPermissions, RepositorySchedulerClaimRequest,
    RepositorySchedulerScope, SqliteStorage, WorkerAuthenticationIdentity,
    WorkerOutboundQueueConfig, WorkerPlatform, WorkerRegistrationRequest, WorkerRegistrationStatus,
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

struct RecordingPublisher;

impl EventPublisher for RecordingPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Ok(())
    }
}

struct FixedClock;

impl StandaloneApplicationClock for FixedClock {
    fn now_millis(&self) -> u64 {
        1_800_000_000_000
    }

    fn now_instant(&self) -> Instant {
        Instant("2027-01-15T08:00:00.000Z".to_owned())
    }
}

fn temporary_root(label: &str) -> PathBuf {
    let suffix = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-server-quick-device-execution-{label}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn suffix(seed: u64) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut identity = String::with_capacity(26);
    let mut value = seed;
    for _ in 0..26 {
        identity.push(ALPHABET[usize::try_from(value % 32).expect("digit fits")] as char);
        value /= 32;
    }
    identity
}

fn instant(value: &str) -> Instant {
    Instant(value.to_owned())
}

fn repository_scope_json(seed: u64) -> serde_json::Value {
    serde_json::json!({
        "kind": "repository",
        "organizationId": id("org", seed),
        "workspaceId": id("wsp", seed),
        "projectId": id("prj", seed),
        "repositoryId": id("rep", seed)
    })
}

fn scheduler_scope(seed: u64) -> RepositorySchedulerScope {
    RepositorySchedulerScope {
        organization_id: OrganizationId(id("org", seed)),
        workspace_id: WorkspaceId(id("wsp", seed)),
        project_id: ProjectId(id("prj", seed)),
        repository_id: RepositoryId(id("rep", seed)),
    }
}

fn actor(user: &str) -> Actor {
    Actor::UserActor(UserActor {
        id: UserId(user.to_owned()),
        kind: UserActorKind::User,
    })
}

fn principal(user: &str) -> AuthenticatedPrincipal {
    let scopes = vec![
        Scope::OrganizationScope(OrganizationScope {
            kind: OrganizationScopeKind::Organization,
            organization_id: OrganizationId(id("org", 1)),
        }),
        serde_json::from_value::<Scope>(repository_scope_json(1)).expect("repository scope"),
    ];
    AuthenticatedPrincipal::new(actor(user), scopes).expect("principal")
}

fn compose_application(root: &Path) -> StandaloneControlPlaneApplication {
    let hub = Arc::new(
        DurableEventHub::open(root.join("events"), DurableEventHubConfig::default())
            .expect("open event hub"),
    );
    let control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(root),
        Box::new(RecordingPublisher),
    )
    .expect("open Control Plane");
    let storage = SqliteStorage::open(root).expect("open application storage");
    let worker_outbound = DurableWorkerInteractionOutbound::new(
        SqliteStorage::open(root).expect("open Worker outbound storage"),
        WorkerOutboundQueueConfig::default(),
    )
    .expect("open Worker outbound adapter");
    let repository_scope =
        serde_json::from_value(repository_scope_json(1)).expect("repository scope");
    let execution = ProductSessionExecutionConfig::try_new(
        repository_scope,
        "fixture-checkout-revision",
        "codex-chat",
        3_600,
        1_073_741_824,
    )
    .expect("execution config");
    StandaloneControlPlaneApplication::new_with_clock(
        control_plane,
        storage,
        worker_outbound,
        hub,
        Arc::new(FixedClock),
        execution,
    )
    .expect("compose application")
}

// ---- durable staging over the same product-state database ------------------

fn stage_node(storage: &mut SqliteStorage, seed: u64) -> String {
    let node = format!("cnd_{}", suffix(seed));
    let registration = ClientNodeRegistration::try_new(
        node.clone(),
        format!("{seed:010}"),
        "Quick Device Test Device".to_owned(),
        "aarch64-apple-darwin",
        "aarch64",
        "1.2.3",
        None,
        Some(format!("cix_{}", suffix(seed + 40))),
        4,
    )
    .expect("registration");
    let mut registry = storage.client_node_registry().expect("registry");
    registry
        .register(&registration, 0, &instant("2026-09-04T12:00:00.000Z"))
        .expect("register");
    registry
        .update_presence(&node, ClientPresenceState::Online, 1)
        .expect("presence");
    node
}

fn stage_client_grant(storage: &mut SqliteStorage, seed: u64, node: &str, user: &str) {
    let issuance = AccessGrantIssuance::try_new(
        format!("cag_{}", suffix(seed)),
        node,
        user,
        user,
        GrantTrustMode::Trusted,
        None,
    )
    .expect("issuance");
    storage
        .client_connect_ledger()
        .expect("ledger")
        .create_grant(
            &issuance,
            GrantSource::Administrator,
            GrantPermissions::USE,
            &instant("2026-09-04T12:00:10.000Z"),
        )
        .expect("grant");
}

fn stage_visible_binding(storage: &mut SqliteStorage, seed: u64, node: &str, user: &str) -> String {
    let binding = format!("rbd_{}", suffix(seed));
    let mut ledger = storage.repository_binding_ledger().expect("ledger");
    let projection = RepositoryBindingProjection::try_new(
        binding.clone(),
        node,
        "winwincode",
        Some("main".to_owned()),
        Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
        RepositoryDirtyState::Clean,
        RepositoryAvailability::Available,
        format!("sha256:{seed:064}"),
    )
    .expect("projection");
    ledger
        .upsert(&projection, None, 0, &instant("2026-09-04T12:00:30.000Z"))
        .expect("upsert");
    let issuance = RepositoryAccessGrantIssuance::try_new(
        format!("rag_{}", suffix(seed + 20)),
        &binding,
        user,
        user,
    )
    .expect("repo issuance");
    ledger
        .create_grant(
            &issuance,
            RepositoryGrantPermissions::Use,
            &instant("2026-09-04T12:00:31.000Z"),
        )
        .expect("repo grant");
    binding
}

fn stage_occupied_lease(
    storage: &mut SqliteStorage,
    seed: u64,
    node: &str,
    user: &str,
) -> (String, u64) {
    let mut occupancy = storage.client_occupancy_ledger().expect("ledger");
    let claim = OccupancyClaim::try_new(
        format!("ocl_{}", suffix(seed)),
        node,
        user,
        format!("req_{}", suffix(seed + 10)),
    )
    .expect("claim");
    let lease = occupancy
        .atomic_claim(&claim, &instant("2026-09-04T12:01:00.000Z"))
        .expect("claim");
    let occupied = occupancy
        .record_acknowledgement(
            &lease.occupancy_lease_id,
            lease.fencing_token,
            None,
            &instant("2026-09-04T12:01:01.000Z"),
        )
        .expect("ack");
    assert_eq!(occupied.state, OccupancyLeaseState::Occupied);
    (occupied.occupancy_lease_id, occupied.fencing_token)
}

/// The device identities one staged launch anchor exposes to the test.
#[allow(clippy::struct_field_names)]
struct AnchorLaunch {
    worker_launch_grant_id: String,
    worker_session_id: String,
    worker_id: String,
    worker_instance_id: String,
}

#[allow(clippy::too_many_arguments)]
fn stage_anchor(
    storage: &mut SqliteStorage,
    seed: u64,
    node: &str,
    user: &str,
    lease_id: &str,
    fencing_token: u64,
    binding: &str,
    product_session_id: &str,
) -> AnchorLaunch {
    let worker_launch_grant_id = format!("wlg_{}", suffix(seed));
    let worker_session_id = format!("ws_{}", suffix(seed + 50));
    let worker_id = format!("wkr_{}", suffix(seed + 51));
    let worker_instance_id = format!("winst_{}", suffix(seed + 52));
    let issuance = LaunchGrantIssuance::try_new(
        worker_launch_grant_id.clone(),
        node,
        format!("cix_{}", suffix(seed + 40)),
        user,
        lease_id,
        fencing_token,
        binding,
        worker_session_id.clone(),
        worker_id.clone(),
        worker_instance_id.clone(),
        "sha256:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
        Some(product_session_id.to_owned()),
        Some(format!("run_{}", suffix(seed + 53))),
        instant("2100-01-01T00:00:00.000Z"),
    )
    .expect("issuance");
    WorkerLaunchGrantService::new(storage)
        .issue(&issuance, &instant("2026-09-04T12:02:00.000Z"))
        .expect("issue anchor grant");
    AnchorLaunch {
        worker_launch_grant_id,
        worker_session_id,
        worker_id,
        worker_instance_id,
    }
}

/// Settles the device launch acknowledgement exactly like the client exchange
/// does, so the anchor grant reaches its post-launch `consumed` state.
fn settle_launch(storage: &mut SqliteStorage, anchor: &AnchorLaunch, lease_id: &str, token: u64) {
    let settlement = LaunchAckSettlement::try_new(
        &anchor.worker_launch_grant_id,
        lease_id,
        token,
        &anchor.worker_session_id,
        &anchor.worker_id,
        &anchor.worker_instance_id,
        true,
        None,
    )
    .expect("settlement");
    let outcome = WorkerLaunchGrantService::new(storage)
        .settle_launch_ack(&settlement, &instant("2026-09-04T12:02:30.000Z"))
        .expect("settle launch ack");
    assert!(matches!(
        outcome,
        winwincode_storage::LaunchAckOutcome::Consumed(_)
    ));
}

// ---- generated command helpers ---------------------------------------------

fn session_create_request(request: u64, session: u64, user: &str) -> CommandRequest {
    serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": id("req", request),
        "command": "session.create",
        "actor": { "kind": "user", "id": user },
        "scope": repository_scope_json(1),
        "expectedRevision": 0,
        "payload": {
            "productSessionId": id("psn", session),
            "projectId": id("prj", 1),
            "repositoryId": id("rep", 1),
            "title": "Quick device session",
            "modelRoute": {
                "providerId": "provider-main",
                "modelId": "model-main",
                "credentialReferenceId": id("crd", 1)
            }
        }
    }))
    .expect("generated session.create command")
}

fn chat_submit_request(
    request: u64,
    session: u64,
    user: &str,
    expected_revision: i64,
) -> CommandRequest {
    serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": id("req", request),
        "command": "chat.submit",
        "actor": { "kind": "user", "id": user },
        "scope": repository_scope_json(1),
        "expectedRevision": expected_revision,
        "payload": {
            "productSessionId": id("psn", session),
            "message": "Continue on the device worker"
        }
    }))
    .expect("generated chat.submit command")
}

fn completed(response: CommandDispatchResponse) -> serde_json::Value {
    let CommandDispatchResponse::Completed(response) = response else {
        panic!("ProductSession command must complete synchronously");
    };
    serde_json::to_value(response).expect("encode completed response")
}

fn open_storage(root: &Path) -> SqliteStorage {
    SqliteStorage::open(root).expect("open staging storage")
}

/// The one queued job of the test's repository scope.
fn queued_job_id(storage: &mut SqliteStorage) -> ExecutionJobId {
    let jobs = storage
        .repository_scheduler()
        .expect("scheduler")
        .list_jobs(&scheduler_scope(1), &[ExecutionJobState::Queued])
        .expect("queued jobs");
    assert_eq!(jobs.len(), 1, "the test scope holds exactly one queued job");
    jobs[0].job_id.clone()
}

fn binding_snapshot(
    storage: &mut SqliteStorage,
    worker_session_id: &str,
) -> Option<winwincode_storage::DeviceExecutionBindingRecord> {
    DeviceExecutionBindingService::new(storage)
        .snapshot(worker_session_id)
        .expect("binding snapshot")
}

fn register_local_worker(storage: &mut SqliteStorage, seed: u64) -> (WorkerId, WorkerInstanceId) {
    let worker_id = WorkerId(id("wrk", seed));
    let worker_instance_id = WorkerInstanceId(id("wki", seed));
    let request = WorkerRegistrationRequest {
        authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
            control_plane_principal: "quick-device-test".to_owned(),
        },
        protocol_version: EXECUTION_PROTOCOL_VERSION.to_owned(),
        platform: WorkerPlatform::Aarch64AppleDarwin,
        capabilities: vec!["codex".to_owned()],
        capability_digest: Sha256Digest(format!("sha256:{}", "0".repeat(64))),
        security_zone: "local".to_owned(),
        max_slots: 4,
        message_id: ExecutionMessageId(id("xmsg", seed)),
        request_id: RequestId(id("req", seed)),
        sent_at: instant("2027-01-15T08:00:00.000Z"),
        started_at: instant("2027-01-15T07:59:00.000Z"),
        worker_id: worker_id.clone(),
        worker_instance_id: worker_instance_id.clone(),
    };
    let mut registry = storage.execution_registry().expect("registry");
    let receipt = registry.register_worker(&request).expect("register");
    assert!(matches!(
        receipt.status,
        WorkerRegistrationStatus::Accepted | WorkerRegistrationStatus::Duplicate
    ));
    (worker_id, worker_instance_id)
}

fn claim_locally(
    storage: &mut SqliteStorage,
    request: u64,
    worker_id: &WorkerId,
    worker_instance_id: &WorkerInstanceId,
) -> Option<ExecutionJobId> {
    let claim = RepositorySchedulerClaimRequest {
        scope: scheduler_scope(1),
        request_id: RequestId(id("req", request)),
        scheduler_generation: "gen-quick-device-test".to_owned(),
        worker_id: worker_id.clone(),
        worker_instance_id: worker_instance_id.clone(),
        issued_at: instant("2027-01-15T08:00:00.000Z"),
        expires_at: instant("2027-01-15T09:00:00.000Z"),
    };
    RepositoryExecutionScheduler::new(storage)
        .claim_next(&claim)
        .expect("claim next job")
        .map(|dispatch| dispatch.job.job_id)
}

#[test]
fn a_device_anchored_turn_is_dispatched_to_the_launched_worker_session() {
    let root = temporary_root("anchored-dispatch");
    let application = compose_application(&root);
    let holder = id("usr", 1);
    {
        let mut storage = open_storage(&root);
        let node = stage_node(&mut storage, 100);
        stage_client_grant(&mut storage, 101, &node, &holder);
        let binding = stage_visible_binding(&mut storage, 102, &node, &holder);
        let (lease_id, fencing_token) = stage_occupied_lease(&mut storage, 103, &node, &holder);
        drop(storage);
        application
            .command(
                &principal(&holder),
                CommandFamily::Session,
                session_create_request(10, 1, &holder),
            )
            .expect("create ProductSession");
        let mut storage = open_storage(&root);
        let anchor = stage_anchor(
            &mut storage,
            104,
            &node,
            &holder,
            &lease_id,
            fencing_token,
            &binding,
            &id("psn", 1),
        );
        settle_launch(&mut storage, &anchor, &lease_id, fencing_token);
    }

    let submitted = application
        .command(
            &principal(&holder),
            CommandFamily::Session,
            chat_submit_request(11, 1, &holder, 1),
        )
        .expect("anchored turn continues");
    let body = completed(submitted);
    assert_eq!(body["currentRevision"], 2);
    assert_eq!(body["result"]["state"], "running");

    let job_id = {
        let mut storage = open_storage(&root);
        let job_id = queued_job_id(&mut storage);
        // The launch material is the device session's durable ExecutionPort
        // identity, and the job carries the exact device worker facts.
        let bound = binding_snapshot(&mut storage, &format!("ws_{}", suffix(154)))
            .expect("the device session is bound");
        assert_eq!(bound.state.as_str(), "bound");
        assert_eq!(bound.worker_launch_grant_id, format!("wlg_{}", suffix(104)));
        assert_eq!(bound.client_node_id, format!("cnd_{}", suffix(100)));
        let facts = DeviceExecutionBindingService::new(&mut storage)
            .facts(job_id.0.as_str())
            .expect("facts lookup")
            .expect("the job carries device facts");
        assert_eq!(facts.worker_session_id, format!("ws_{}", suffix(154)));
        assert_eq!(facts.worker_id, format!("wkr_{}", suffix(155)));
        assert_eq!(facts.worker_instance_id, format!("winst_{}", suffix(156)));
        assert_eq!(facts.holder_user_id, holder);
        assert_eq!(facts.repository_binding_id, format!("rbd_{}", suffix(102)));
        assert_eq!(facts.product_session_id, Some(id("psn", 1)));
        // The job itself stays queued for the device worker.
        let record = storage
            .load_execution_job_record(&job_id)
            .expect("job record")
            .expect("queued job");
        assert_eq!(record.state, ExecutionJobState::Queued);
        job_id
    };
    // The local embedded worker cannot claim the device-owned job: the
    // queue selection excludes it, so a local drive finds nothing.
    {
        let mut storage = open_storage(&root);
        let (worker_id, worker_instance_id) = register_local_worker(&mut storage, 400);
        assert!(claim_locally(&mut storage, 401, &worker_id, &worker_instance_id).is_none());
    }
    // The job still waits for its device worker, not for a local slot.
    let storage = open_storage(&root);
    let record = storage
        .load_execution_job_record(&job_id)
        .expect("job record")
        .expect("queued job");
    assert_eq!(record.state, ExecutionJobState::Queued);

    application.shutdown().expect("shutdown");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_dispatch_replays_exactly_without_new_facts() {
    let root = temporary_root("dispatch-replay");
    let application = compose_application(&root);
    let holder = id("usr", 1);
    {
        let mut storage = open_storage(&root);
        let node = stage_node(&mut storage, 500);
        stage_client_grant(&mut storage, 501, &node, &holder);
        let binding = stage_visible_binding(&mut storage, 502, &node, &holder);
        let (lease_id, fencing_token) = stage_occupied_lease(&mut storage, 503, &node, &holder);
        drop(storage);
        application
            .command(
                &principal(&holder),
                CommandFamily::Session,
                session_create_request(50, 1, &holder),
            )
            .expect("create ProductSession");
        let mut storage = open_storage(&root);
        let anchor = stage_anchor(
            &mut storage,
            504,
            &node,
            &holder,
            &lease_id,
            fencing_token,
            &binding,
            &id("psn", 1),
        );
        settle_launch(&mut storage, &anchor, &lease_id, fencing_token);
    }
    let request = chat_submit_request(51, 1, &holder, 1);
    let first = completed(
        application
            .command(&principal(&holder), CommandFamily::Session, request.clone())
            .expect("first submit"),
    );
    let replay = completed(
        application
            .command(&principal(&holder), CommandFamily::Session, request)
            .expect("exact replay"),
    );
    assert_eq!(first["currentRevision"], replay["currentRevision"]);

    let mut storage = open_storage(&root);
    let job_id = queued_job_id(&mut storage);
    let facts = DeviceExecutionBindingService::new(&mut storage)
        .facts(job_id.0.as_str())
        .expect("facts lookup")
        .expect("the job carries device facts");
    assert_eq!(facts.worker_session_id, format!("ws_{}", suffix(554)));
    assert_eq!(facts.attached_at, instant("2027-01-15T08:00:00.000Z"));

    application.shutdown().expect("shutdown");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_unanchored_session_turn_keeps_the_local_execution_path() {
    let root = temporary_root("local-path");
    let application = compose_application(&root);
    let user = id("usr", 1);
    application
        .command(
            &principal(&user),
            CommandFamily::Session,
            session_create_request(60, 1, &user),
        )
        .expect("create ProductSession");
    let submitted = application
        .command(
            &principal(&user),
            CommandFamily::Session,
            chat_submit_request(61, 1, &user, 1),
        )
        .expect("unanchored turn continues unchanged");
    let body = completed(submitted);
    assert_eq!(body["result"]["state"], "running");

    let mut storage = open_storage(&root);
    let job_id = queued_job_id(&mut storage);
    assert!(
        DeviceExecutionBindingService::new(&mut storage)
            .facts(job_id.0.as_str())
            .expect("facts lookup")
            .is_none(),
        "an unanchored turn must not carry device facts"
    );
    // The local embedded worker claims the turn exactly as before.
    let (worker_id, worker_instance_id) = register_local_worker(&mut storage, 700);
    assert_eq!(
        claim_locally(&mut storage, 701, &worker_id, &worker_instance_id),
        Some(job_id)
    );

    application.shutdown().expect("shutdown");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_gate_denial_dispatches_nothing() {
    let root = temporary_root("gate-denial");
    let application = compose_application(&root);
    let holder = id("usr", 1);
    let member = id("usr", 2);
    {
        let mut storage = open_storage(&root);
        let node = stage_node(&mut storage, 800);
        stage_client_grant(&mut storage, 801, &node, &holder);
        let binding = stage_visible_binding(&mut storage, 802, &node, &holder);
        let (lease_id, fencing_token) = stage_occupied_lease(&mut storage, 803, &node, &holder);
        drop(storage);
        application
            .command(
                &principal(&holder),
                CommandFamily::Session,
                session_create_request(80, 1, &holder),
            )
            .expect("create ProductSession");
        let mut storage = open_storage(&root);
        stage_anchor(
            &mut storage,
            804,
            &node,
            &holder,
            &lease_id,
            fencing_token,
            &binding,
            &id("psn", 1),
        );
    }

    let denial: ApiError = application
        .command(
            &principal(&member),
            CommandFamily::Session,
            chat_submit_request(81, 1, &member, 1),
        )
        .expect_err("a non-holder must be refused");
    assert_eq!(denial.status(), 403);
    assert_eq!(denial.code(), "ACCESS_DENIED");

    // No dispatch fact exists for the refused turn: the gate runs before the
    // turn is committed, so nothing was bound and nothing was queued.
    let mut storage = open_storage(&root);
    assert!(binding_snapshot(&mut storage, &format!("ws_{}", suffix(854))).is_none());
    let jobs = storage
        .repository_scheduler()
        .expect("scheduler")
        .list_jobs(&scheduler_scope(1), &[ExecutionJobState::Queued])
        .expect("queued jobs");
    assert!(jobs.is_empty(), "a denied turn must not create a job");

    application.shutdown().expect("shutdown");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_ended_launch_refuses_the_dispatch_without_binding() {
    let root = temporary_root("ended-launch");
    let application = compose_application(&root);
    let holder = id("usr", 1);
    {
        let mut storage = open_storage(&root);
        let node = stage_node(&mut storage, 900);
        stage_client_grant(&mut storage, 901, &node, &holder);
        let binding = stage_visible_binding(&mut storage, 902, &node, &holder);
        let (lease_id, fencing_token) = stage_occupied_lease(&mut storage, 903, &node, &holder);
        drop(storage);
        application
            .command(
                &principal(&holder),
                CommandFamily::Session,
                session_create_request(90, 1, &holder),
            )
            .expect("create ProductSession");
        let mut storage = open_storage(&root);
        let anchor = stage_anchor(
            &mut storage,
            904,
            &node,
            &holder,
            &lease_id,
            fencing_token,
            &binding,
            &id("psn", 1),
        );
        // The launch is revoked before the device ever accepted it.
        WorkerLaunchGrantService::new(&mut storage)
            .revoke(
                &anchor.worker_launch_grant_id,
                &holder,
                Some("test revoked"),
                &instant("2026-09-04T12:02:30.000Z"),
            )
            .expect("revoke anchor grant");
    }

    let denial: ApiError = application
        .command(
            &principal(&holder),
            CommandFamily::Session,
            chat_submit_request(91, 1, &holder, 1),
        )
        .expect_err("a dead launch anchor must refuse the dispatch");
    assert_eq!(denial.status(), 409);
    assert_eq!(denial.code(), "WRONG_STATE");

    // The dead anchor never binds a worker session and never attaches facts.
    let mut storage = open_storage(&root);
    assert!(binding_snapshot(&mut storage, &format!("ws_{}", suffix(954))).is_none());
    let jobs = storage
        .repository_scheduler()
        .expect("scheduler")
        .list_jobs(&scheduler_scope(1), &[ExecutionJobState::Queued])
        .expect("queued jobs");
    assert_eq!(jobs.len(), 1, "the committed turn stays queued");
    assert!(
        DeviceExecutionBindingService::new(&mut storage)
            .facts(jobs[0].job_id.0.as_str())
            .expect("facts lookup")
            .is_none(),
        "a dead anchor must not attach device facts"
    );

    application.shutdown().expect("shutdown");
    let _ = std::fs::remove_dir_all(&root);
}
