// SPDX-License-Identifier: Apache-2.0

#[path = "support/device_provider.rs"]
mod device_provider;

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use winwincode_api::generated::{
    Actor, CommandRequest, OrganizationScope, OrganizationScopeKind, Scope,
};
use winwincode_control_plane::{
    ControlPlane, ControlPlaneConfig, DeviceExecutionBindingService,
    DurableWorkerInteractionOutbound, EventPublishError, EventPublisher, OutboxEvent,
    ProductSessionExecutionConfig, RepositoryExecutionScheduler, WorkerLaunchGrantService,
};
use winwincode_domain::{
    ExecutionJobId, ExecutionMessageId, Instant, OrganizationId, ProjectId, RepositoryId,
    RequestId, Sha256Digest, UserActor, UserActorKind, UserId, WorkerId, WorkerInstanceId,
    WorkspaceId,
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

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one fixture crosses production authentication, durable import, and replay"
)]
fn production_accounting_routes_device_receipts_and_rejects_conflicting_replay() {
    use winwincode_execution_port::{
        accounting::{
            AttemptAccountingStatement, PendingAccountingPage, ProviderAccountingReceipt,
        },
        generated::{ExecutionLeaseStamp, ExecutionPortMessage},
        transport::ExecutionPortCore,
    };
    use winwincode_server::{
        ProductionRemoteWorkerExchange, RemoteWorkerExchangePort, RepositoryRuntimeScheduler,
    };
    struct NoExecution;
    impl ExecutionPortCore for NoExecution {
        type Output = Vec<ExecutionPortMessage>;
        type Error = std::convert::Infallible;
        fn accept(&mut self, _: &ExecutionPortMessage) -> Result<Self::Output, Self::Error> {
            panic!("financial authority must not execute")
        }
    }
    let root = temporary_root("production-accounting");
    let application = compose_application(&root);
    let repository = serde_json::from_value(repository_scope_json(1)).unwrap();
    let scheduler = RepositoryRuntimeScheduler::from_application(
        &application,
        repository,
        WorkerId(id("wrk", 1)),
        WorkerInstanceId(id("wki", 1)),
        "financial-test",
        std::time::Duration::from_secs(30),
    )
    .unwrap();
    let mut storage = SqliteStorage::open(&root).unwrap();
    let user = id("usr", 1);
    let node = stage_node(&mut storage, 5_000);
    stage_client_grant(&mut storage, 5_001, &node, &user);
    let binding = stage_visible_binding(&mut storage, 5_002, &node, &user);
    let (occupancy, fencing) = stage_occupied_lease(&mut storage, 5_003, &node, &user);
    let anchor = stage_anchor(
        &mut storage,
        5_004,
        &node,
        &user,
        &occupancy,
        fencing,
        &binding,
        &id("psn", 1),
    );
    let other = id("cnd", 5_100);
    let exchange = ProductionRemoteWorkerExchange::new(
        &root,
        serde_json::from_value(repository_scope_json(1)).unwrap(),
        scheduler,
        NoExecution,
    );
    assert!(storage.pending_accounting_leases(0).unwrap().is_empty());
    storage.execution_registry().unwrap();
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let mut lease: ExecutionLeaseStamp = serde_json::from_value(
        fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["kind"] == "model.open")
            .unwrap()["lease"]
            .clone(),
    )
    .unwrap();
    lease.worker_id = WorkerId(anchor.worker_id.clone());
    lease.worker_instance_id = WorkerInstanceId(anchor.worker_instance_id.clone());
    let record = winwincode_storage::ExecutionLeaseRecord {
        job_id: lease.job_id.clone(),
        lease_id: lease.lease_id.clone(),
        payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
        attempt: u64::try_from(lease.attempt).unwrap(),
        fencing_token: lease.fencing_token.clone(),
        issued_at: lease.issued_at.clone(),
        expires_at: lease.expires_at.clone(),
    };
    let fixture_db = rusqlite::Connection::open(storage.database_path()).unwrap();
    fixture_db
        .execute(
            "INSERT INTO execution_attempt_accounting_pending VALUES(?1,?2,?3,?4)",
            rusqlite::params![
                lease.job_id.0,
                lease.attempt,
                serde_json::to_string(&record).unwrap(),
                lease.expires_at.0
            ],
        )
        .unwrap();
    drop(fixture_db);
    let now = instant("2027-01-15T08:00:00.000Z");
    assert_eq!(
        exchange
            .accounting(
                &other,
                &anchor.worker_id,
                &anchor.worker_instance_id,
                None,
                0,
                now.clone()
            )
            .unwrap_err()
            .status_code(),
        400
    );
    let page: PendingAccountingPage = serde_json::from_slice(
        &exchange
            .accounting(
                &node,
                &anchor.worker_id,
                &anchor.worker_instance_id,
                None,
                0,
                now.clone(),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(page.leases, vec![lease.clone()]);
    let mut statement = AttemptAccountingStatement {
        lease,
        manifest: vec!["primary:receipt".into()],
        receipts: vec![ProviderAccountingReceipt {
            known_tokens: 0,
            slot: "primary:receipt".into(),
            model_exchange_id: winwincode_domain::ModelExchangeId(id("mdl", 1)),
            provider_id: "provider-fixture".into(),
            provider_receipt_id: "receipt-1".into(),
            source_digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
            tokens: Some(9),
            cost_microunits: Some(4),
        }],
    };
    let body = statement.encode().unwrap();
    assert_eq!(
        exchange
            .accounting(
                &other,
                &anchor.worker_id,
                &anchor.worker_instance_id,
                None,
                0,
                now.clone()
            )
            .unwrap_err()
            .status_code(),
        400
    );
    let mut foreign = statement.clone();
    foreign.lease.worker_instance_id = WorkerInstanceId(id("wki", 9_999));
    assert_eq!(
        exchange
            .accounting(
                &node,
                &anchor.worker_id,
                &anchor.worker_instance_id,
                Some(&foreign.encode().unwrap()),
                0,
                now.clone()
            )
            .unwrap_err()
            .status_code(),
        400
    );

    for _ in 0..2 {
        assert_eq!(
            exchange
                .accounting(
                    &node,
                    &anchor.worker_id,
                    &anchor.worker_instance_id,
                    Some(&body),
                    0,
                    now.clone()
                )
                .unwrap(),
            b"{}"
        );
    }
    let page: PendingAccountingPage = serde_json::from_slice(
        &exchange
            .accounting(
                &node,
                &anchor.worker_id,
                &anchor.worker_instance_id,
                None,
                0,
                now.clone(),
            )
            .unwrap(),
    )
    .unwrap();
    assert!(page.leases.is_empty());
    statement.receipts[0].tokens = Some(10);
    assert_eq!(
        exchange
            .accounting(
                &node,
                &anchor.worker_id,
                &anchor.worker_instance_id,
                Some(&statement.encode().unwrap()),
                0,
                now
            )
            .unwrap_err()
            .status_code(),
        400
    );
    drop(storage);
    drop(exchange);
    drop(application);
    std::fs::remove_dir_all(root).unwrap();
}

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
    compose_application_with_runtime(root, Some(3_600))
}

fn compose_application_with_runtime(
    root: &Path,
    runtime: Option<i64>,
) -> StandaloneControlPlaneApplication {
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
        runtime,
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

fn stage_node(storage: &mut SqliteStorage, seed: u64) -> String {
    let node = format!("cnd_{}", suffix(seed));
    let registration = ClientNodeRegistration::try_new(
        node.clone(),
        format!("{seed:010}"),
        "Quick Device Test Device".to_owned(),
        "aarch64-apple-darwin",
        "aarch64",
        "1.2.3",
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
    device_provider::stage(storage, &node);
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
    let client_instance = storage
        .client_node_registry()
        .unwrap()
        .snapshot(node)
        .unwrap()
        .unwrap()
        .current_instance_id
        .unwrap();
    let worker_launch_grant_id = format!("wlg_{}", suffix(seed));
    let worker_session_id = format!("wsn_{}", suffix(seed + 50));
    let worker_id = format!("wrk_{}", suffix(seed + 51));
    let worker_instance_id = format!("wki_{}", suffix(seed + 52));
    let issuance = LaunchGrantIssuance::try_new(
        worker_launch_grant_id.clone(),
        node,
        client_instance,
        user,
        lease_id,
        fencing_token,
        binding,
        worker_session_id.clone(),
        worker_id.clone(),
        worker_instance_id.clone(),
        Some(product_session_id.to_owned()),
        None,
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

fn session_create_request(request: u64, session: u64, user: &str, node: &str) -> CommandRequest {
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
            "modelRoute": device_provider::route(node)
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
    assert_anchored_runtime(Some(3_600));
}

#[test]
fn unlimited_device_turn_persists_unlimited_job_and_reservation() {
    assert_anchored_runtime(None);
}

#[allow(clippy::too_many_lines)]
fn assert_anchored_runtime(runtime: Option<i64>) {
    let root = temporary_root("anchored-dispatch");
    let application = compose_application_with_runtime(&root, runtime);
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
                session_create_request(10, 1, &holder, &node),
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
        let bound = binding_snapshot(&mut storage, &format!("wsn_{}", suffix(154)))
            .expect("the device session is bound");
        assert_eq!(bound.state.as_str(), "bound");
        assert_eq!(bound.worker_launch_grant_id, format!("wlg_{}", suffix(104)));
        assert_eq!(bound.client_node_id, format!("cnd_{}", suffix(100)));
        let facts = DeviceExecutionBindingService::new(&mut storage)
            .facts(job_id.0.as_str())
            .expect("facts lookup")
            .expect("the job carries device facts");
        assert_eq!(facts.worker_session_id, format!("wsn_{}", suffix(154)));
        assert_eq!(facts.worker_id, format!("wrk_{}", suffix(155)));
        assert_eq!(facts.worker_instance_id, format!("wki_{}", suffix(156)));
        assert_eq!(facts.holder_user_id, holder);
        assert_eq!(facts.repository_binding_id, format!("rbd_{}", suffix(102)));
        assert_eq!(facts.product_session_id, Some(id("psn", 1)));
        let record = storage
            .load_execution_job_record(&job_id)
            .expect("job record")
            .expect("queued job");
        let job: serde_json::Value =
            serde_json::from_slice(&record.dispatch_payload).expect("durable job");
        assert_eq!(
            job["limits"]["maxRuntimeSeconds"],
            serde_json::json!(runtime)
        );
        assert_eq!(job["limits"]["deadlineAt"].is_null(), runtime.is_none());
        let reservation = storage
            .execution_admission()
            .expect("admission")
            .load_reservation_by_job(&job_id)
            .expect("reservation lookup")
            .expect("reservation");
        assert_eq!(reservation.reserved_tokens, None);
        assert_eq!(reservation.reserved_cost_microunits, None);
        assert_eq!(
            reservation.runtime_limit_millis,
            runtime.map(|seconds| u64::try_from(seconds).unwrap() * 1000)
        );
        assert_eq!(record.state, ExecutionJobState::Queued);
        job_id
    };
    {
        let mut storage = open_storage(&root);
        let (worker_id, worker_instance_id) = register_local_worker(&mut storage, 400);
        assert!(claim_locally(&mut storage, 401, &worker_id, &worker_instance_id).is_none());
    }
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
                session_create_request(50, 1, &holder, &node),
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
    assert_eq!(facts.worker_session_id, format!("wsn_{}", suffix(554)));
    assert_eq!(facts.attached_at, instant("2027-01-15T08:00:00.000Z"));

    application.shutdown().expect("shutdown");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_unanchored_session_turn_cannot_dispatch() {
    let root = temporary_root("local-path");
    let application = compose_application(&root);
    let user = id("usr", 1);
    application
        .command(
            &principal(&user),
            CommandFamily::Session,
            session_create_request(60, 1, &user, "cnd_00000000000000000000000001"),
        )
        .expect("create ProductSession");
    let submitted = application
        .command(
            &principal(&user),
            CommandFamily::Session,
            chat_submit_request(61, 1, &user, 1),
        )
        .expect_err("unanchored sessions cannot dispatch");
    assert_eq!(submitted.code(), "DEVICE_SESSION_REQUIRED");
    let mut storage = open_storage(&root);
    let (worker_id, worker_instance_id) = register_local_worker(&mut storage, 700);
    assert_eq!(
        claim_locally(&mut storage, 701, &worker_id, &worker_instance_id),
        None
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
                session_create_request(80, 1, &holder, &node),
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

    let mut storage = open_storage(&root);
    assert!(binding_snapshot(&mut storage, &format!("wsn_{}", suffix(854))).is_none());
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
                session_create_request(90, 1, &holder, &node),
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

    let mut storage = open_storage(&root);
    assert!(binding_snapshot(&mut storage, &format!("wsn_{}", suffix(954))).is_none());
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

fn stage_replay_session(root: &Path, seed: u64) -> StandaloneControlPlaneApplication {
    let application = compose_application(root);
    let holder = id("usr", 1);
    let mut storage = open_storage(root);
    let node = stage_node(&mut storage, seed);
    stage_client_grant(&mut storage, seed + 1, &node, &holder);
    let binding = stage_visible_binding(&mut storage, seed + 2, &node, &holder);
    let (lease_id, fencing_token) = stage_occupied_lease(&mut storage, seed + 3, &node, &holder);
    drop(storage);
    application
        .command(
            &principal(&holder),
            CommandFamily::Session,
            session_create_request(seed + 10, 1, &holder, &node),
        )
        .unwrap();
    let mut storage = open_storage(root);
    let anchor = stage_anchor(
        &mut storage,
        seed + 4,
        &node,
        &holder,
        &lease_id,
        fencing_token,
        &binding,
        &id("psn", 1),
    );
    settle_launch(&mut storage, &anchor, &lease_id, fencing_token);
    application
}

#[test]
#[allow(clippy::too_many_lines)]
fn a_terminal_chat_receipt_replays_after_its_original_device_binding_is_released() {
    let root = temporary_root("terminal-dispatch-replay");
    let application = stage_replay_session(&root, 1_100);
    let holder = id("usr", 1);
    let request = chat_submit_request(1_111, 1, &holder, 1);
    let first = completed(
        application
            .command(&principal(&holder), CommandFamily::Session, request.clone())
            .unwrap(),
    );
    let mut storage = open_storage(&root);
    let job_id = queued_job_id(&mut storage);
    let facts = DeviceExecutionBindingService::new(&mut storage)
        .facts(&job_id.0)
        .unwrap()
        .unwrap();
    let mut terminal = storage.load_execution_job_record(&job_id).unwrap().unwrap();
    // This focused storage seam uses legal CP queue transitions. The production
    // Load scenario independently requires the actual Worker outcome and ACK.
    for (index, to) in [
        ExecutionJobState::Leased,
        ExecutionJobState::Running,
        ExecutionJobState::Completed,
    ]
    .into_iter()
    .enumerate()
    {
        terminal = storage
            .execution_queue()
            .unwrap()
            .transition(&winwincode_storage::ExecutionJobTransitionRequest {
                scope: terminal.scope.clone(),
                job_id: job_id.clone(),
                request_id: RequestId(id("req", 1_120 + u64::try_from(index).unwrap())),
                expected_revision: terminal.revision,
                from: terminal.state,
                to,
                occurred_at: instant(&format!("2027-01-15T08:00:0{}.000Z", index + 1)),
            })
            .unwrap()
            .job;
    }
    let binding = DeviceExecutionBindingService::new(&mut storage)
        .snapshot(&facts.worker_session_id)
        .unwrap()
        .unwrap();
    let released = DeviceExecutionBindingService::new(&mut storage)
        .release(
            &winwincode_storage::DeviceExecutionBindingRelease::try_new(
                &facts.worker_session_id,
                id("req", 1_124),
                binding.revision,
                instant("2027-01-15T08:00:04.000Z"),
            )
            .unwrap(),
            &instant("2027-01-15T08:00:04.000Z"),
        )
        .unwrap()
        .binding;
    assert_eq!(released.state.as_str(), "released");
    let replay = completed(
        application
            .command(&principal(&holder), CommandFamily::Session, request.clone())
            .unwrap(),
    );
    assert_eq!(
        replay, first,
        "the exact original public response survives a released execution binding"
    );
    assert_eq!(
        storage.load_execution_job_record(&job_id).unwrap().unwrap(),
        terminal
    );
    assert_eq!(
        DeviceExecutionBindingService::new(&mut storage)
            .facts(&job_id.0)
            .unwrap()
            .unwrap(),
        facts
    );
    assert_eq!(
        DeviceExecutionBindingService::new(&mut storage)
            .snapshot(&facts.worker_session_id)
            .unwrap()
            .unwrap(),
        released
    );
    let CommandRequest::ChatSubmitCommand(mut changed) = request else {
        panic!("chat command")
    };
    changed.payload.message = "changed replay input".into();
    let denied = application
        .command(
            &principal(&holder),
            CommandFamily::Session,
            CommandRequest::ChatSubmitCommand(changed),
        )
        .unwrap_err();
    assert_eq!(denied.code(), "IDEMPOTENCY_CONFLICT");
    let anchor = storage
        .worker_launch_grant_ledger()
        .unwrap()
        .snapshot(&facts.worker_launch_grant_id)
        .unwrap()
        .unwrap();
    for delta in ["worker", "fence", "session", "grant"] {
        let mut foreign = anchor.clone();
        match delta {
            "worker" => foreign.worker_instance_id = id("wki", 9_991),
            "fence" => foreign.occupancy_fencing_token += 1,
            "session" => foreign.product_session_id = Some(id("psn", 9_992)),
            "grant" => foreign.worker_launch_grant_id = id("wlg", 9_993),
            _ => unreachable!(),
        }
        assert!(
            winwincode_control_plane::dispatch_chat_job_to_device_worker(
                &mut storage,
                &job_id,
                &foreign,
                &instant("2027-01-15T08:00:05.000Z")
            )
            .is_err(),
            "changed terminal identity {delta} must fail"
        );
    }
    application.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_committed_chat_retry_restores_missing_device_dispatch_and_keeps_live_binding_guards() {
    let root = temporary_root("committed-before-dispatch-replay");
    let application = stage_replay_session(&root, 1_200);
    let holder = id("usr", 1);
    let request = chat_submit_request(1_211, 1, &holder, 1);
    let mut storage = open_storage(&root);
    storage.device_execution_binding_ledger().unwrap();
    let connection = rusqlite::Connection::open(storage.database_path()).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_dispatch_facts BEFORE INSERT ON device_execution_reservation_facts BEGIN SELECT RAISE(ABORT, 'injected after Chat commit before dispatch facts'); END;").unwrap();
    let failed = application
        .command(&principal(&holder), CommandFamily::Session, request.clone())
        .unwrap_err();
    assert_eq!(failed.status(), 503);
    let job_id = queued_job_id(&mut storage);
    let original = storage.load_execution_job_record(&job_id).unwrap().unwrap();
    assert!(
        DeviceExecutionBindingService::new(&mut storage)
            .facts(&job_id.0)
            .unwrap()
            .is_none()
    );
    connection
        .execute_batch("DROP TRIGGER fail_dispatch_facts;")
        .unwrap();
    let recovered = completed(
        application
            .command(&principal(&holder), CommandFamily::Session, request.clone())
            .unwrap(),
    );
    assert_eq!(recovered["previousRevision"], 1);
    assert_eq!(recovered["currentRevision"], 2);
    assert_eq!(
        storage.load_execution_job_record(&job_id).unwrap().unwrap(),
        original
    );
    let facts = DeviceExecutionBindingService::new(&mut storage)
        .facts(&job_id.0)
        .unwrap()
        .unwrap();
    assert_eq!(
        completed(
            application
                .command(&principal(&holder), CommandFamily::Session, request.clone())
                .unwrap()
        ),
        recovered
    );
    let binding = DeviceExecutionBindingService::new(&mut storage)
        .snapshot(&facts.worker_session_id)
        .unwrap()
        .unwrap();
    DeviceExecutionBindingService::new(&mut storage)
        .release(
            &winwincode_storage::DeviceExecutionBindingRelease::try_new(
                &facts.worker_session_id,
                id("req", 1_224),
                binding.revision,
                instant("2027-01-15T08:00:04.000Z"),
            )
            .unwrap(),
            &instant("2027-01-15T08:00:04.000Z"),
        )
        .unwrap();
    assert_eq!(
        application
            .command(&principal(&holder), CommandFamily::Session, request)
            .unwrap_err()
            .code(),
        "WRONG_STATE",
        "an active Job cannot recover invocation rights from a released Worker binding"
    );
    assert_eq!(
        storage.load_execution_job_record(&job_id).unwrap().unwrap(),
        original
    );
    assert_eq!(
        DeviceExecutionBindingService::new(&mut storage)
            .facts(&job_id.0)
            .unwrap()
            .unwrap(),
        facts
    );
    application.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn revoke_releases_a_prepared_quick_dispatch_without_a_lease() {
    revoke_prepared_quick_dispatch(false, false);
}

#[test]
fn revoke_releases_a_started_admission_after_storage_reopens_before_claim() {
    revoke_prepared_quick_dispatch(true, false);
}

#[test]
fn revoke_releases_a_prepared_admission_after_facts_fail_and_binding_releases() {
    revoke_prepared_quick_dispatch(false, true);
}

#[allow(clippy::too_many_lines)]
fn revoke_prepared_quick_dispatch(started: bool, fail_facts: bool) {
    use winwincode_storage::{
        ExecutionAdmissionBoundary, ExecutionReservationReleaseReason, ExecutionReservationRequest,
        ExecutionReservationStart, ExecutionReservationState,
    };
    let root = temporary_root(if fail_facts {
        "revoke-before-facts"
    } else if started {
        "revoke-started"
    } else {
        "revoke-queued"
    });
    let application = stage_replay_session(&root, 1_500);
    let holder = id("usr", 1);
    let request = chat_submit_request(1_511, 1, &holder, 1);
    let mut storage = open_storage(&root);
    storage.device_execution_binding_ledger().unwrap();
    let connection = rusqlite::Connection::open(storage.database_path()).unwrap();
    if fail_facts {
        connection.execute_batch("CREATE TRIGGER fail_revoke_dispatch_facts BEFORE INSERT ON device_execution_reservation_facts BEGIN SELECT RAISE(ABORT, 'injected before dispatch facts'); END;").unwrap();
        assert_eq!(
            application
                .command(&principal(&holder), CommandFamily::Session, request)
                .unwrap_err()
                .status(),
            503
        );
        connection
            .execute_batch("DROP TRIGGER fail_revoke_dispatch_facts")
            .unwrap();
    } else {
        completed(
            application
                .command(&principal(&holder), CommandFamily::Session, request)
                .unwrap(),
        );
    }
    let job_id = queued_job_id(&mut storage);
    let facts = DeviceExecutionBindingService::new(&mut storage)
        .facts(&job_id.0)
        .unwrap();
    assert_eq!(facts.is_none(), fail_facts);
    let grant = storage
        .worker_launch_grant_ledger()
        .unwrap()
        .newest_grant_for_product_session(&id("psn", 1))
        .unwrap()
        .unwrap();
    if fail_facts {
        let binding = DeviceExecutionBindingService::new(&mut storage)
            .snapshot(&grant.worker_session_id)
            .unwrap()
            .unwrap();
        let now = instant("2027-01-15T08:00:02.000Z");
        DeviceExecutionBindingService::new(&mut storage)
            .release(
                &winwincode_storage::DeviceExecutionBindingRelease::try_new(
                    &grant.worker_session_id,
                    id("req", 1_516),
                    binding.revision,
                    now.clone(),
                )
                .unwrap(),
                &now,
            )
            .unwrap();
    }
    assert!(
        storage
            .execution_registry()
            .unwrap()
            .load_lease(&job_id)
            .unwrap()
            .is_none()
    );
    storage.initialize_device_auth().unwrap();
    connection.execute(
        "INSERT INTO device_identity_bindings VALUES(?1,'fixture-public-key',?2,?3,'fixture-intent','fixture-token','{}',?4,NULL)",
        rusqlite::params![grant.client_node_id,holder,id("req",1_512),"2027-01-15T07:59:00.000Z"],
    ).unwrap();
    let original = storage
        .execution_admission()
        .unwrap()
        .load_reservation_by_job(&job_id)
        .unwrap()
        .unwrap();
    let boundary = ExecutionAdmissionBoundary::ProductSession {
        organization_id: original.scope.organization_id.clone(),
        project_id: original.scope.project_id.clone(),
        product_session_id: original.scope.product_session_id.clone(),
    };
    connection
        .execute(
            "UPDATE execution_admission_policies SET token_budget=100,cost_budget_microunits=100",
            [],
        )
        .unwrap();
    assert!(storage.pending_accounting_leases(0).unwrap().is_empty());
    if started {
        storage
            .execution_admission()
            .unwrap()
            .start(&ExecutionReservationStart {
                scope: original.scope.clone(),
                worker_pool_id: original.worker_pool_id.clone(),
                job_id: job_id.clone(),
                request_id: RequestId(id("req", 1_513)),
                expected_revision: original.revision,
                started_at: instant("2027-01-15T08:00:01.000Z"),
            })
            .unwrap();
    }
    let usage = storage
        .execution_admission()
        .unwrap()
        .usage(&boundary)
        .unwrap();
    assert_eq!(
        (usage.queued, usage.running),
        if started { (0, 1) } else { (1, 0) }
    );
    application.shutdown().unwrap();
    drop(application);
    drop(storage);
    let mut storage = open_storage(&root);
    assert!(
        storage
            .execution_registry()
            .unwrap()
            .load_lease(&job_id)
            .unwrap()
            .is_none()
    );
    storage
        .revoke_device_identity(&grant.client_node_id, &holder, "2027-01-15T08:00:07.000Z")
        .unwrap();
    let job = storage.load_execution_job_record(&job_id).unwrap().unwrap();
    assert_eq!(job.state, ExecutionJobState::Failed);
    assert!(job.cancellation.is_some());
    assert!(
        storage
            .execution_registry()
            .unwrap()
            .load_lease(&job_id)
            .unwrap()
            .is_none()
    );
    let released = storage
        .execution_admission()
        .unwrap()
        .load_reservation_by_job(&job_id)
        .unwrap()
        .unwrap();
    assert_eq!(released.state, ExecutionReservationState::Released);
    assert_eq!(
        released.release_reason,
        Some(ExecutionReservationReleaseReason::Cancelled)
    );
    assert_eq!(released.actual_tokens, None);
    assert_eq!(released.actual_cost_microunits, None);
    let reconciliations: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM execution_admission_reconciliation WHERE job_id=?1",
            [&job_id.0],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(reconciliations, 0);
    assert!(storage.pending_accounting_leases(0).unwrap().is_empty());
    let boundary_usage = storage
        .execution_admission()
        .unwrap()
        .usage(&boundary)
        .unwrap();
    assert_eq!(
        (
            boundary_usage.queued,
            boundary_usage.running,
            boundary_usage.reserved_tokens,
            boundary_usage.reserved_cost_microunits
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(boundary_usage.accounting_pending, 0);
    assert_eq!(boundary_usage.committed_cost_microunits, Some(0));
    assert_eq!(
        DeviceExecutionBindingService::new(&mut storage)
            .facts(&job_id.0)
            .unwrap(),
        facts
    );
    storage
        .revoke_device_identity(&grant.client_node_id, &holder, "2027-01-15T08:00:08.000Z")
        .unwrap();
    assert_eq!(
        storage.load_execution_job_record(&job_id).unwrap().unwrap(),
        job
    );
    assert_eq!(
        storage
            .execution_admission()
            .unwrap()
            .load_reservation_by_job(&job_id)
            .unwrap()
            .unwrap(),
        released
    );
    let next_job = ExecutionJobId(id("job", 1_514));
    let next = storage
        .execution_admission()
        .unwrap()
        .reserve(&ExecutionReservationRequest {
            scope: original.scope.clone(),
            user_id: original.user_id,
            worker_pool_id: original.worker_pool_id.clone(),
            job_id: next_job.clone(),
            request_id: RequestId(id("req", 1_514)),
            repository_access: original.repository_access,
            reserved_tokens: Some(1),
            reserved_cost_microunits: Some(1),
            runtime_limit_millis: original.runtime_limit_millis,
            submitted_at: instant("2027-01-15T08:00:09.000Z"),
        })
        .unwrap();
    storage
        .execution_admission()
        .unwrap()
        .start(&ExecutionReservationStart {
            scope: original.scope,
            worker_pool_id: original.worker_pool_id,
            job_id: next_job,
            request_id: RequestId(id("req", 1_515)),
            expected_revision: next.reservation.revision,
            started_at: instant("2027-01-15T08:00:10.000Z"),
        })
        .unwrap();
    drop(connection);
    drop(storage);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[allow(clippy::too_many_lines)]
fn revoke_preserves_a_prepared_admission_with_a_successor_device_grant() {
    let root = temporary_root("revoke-successor-before-facts");
    let application = stage_replay_session(&root, 1_600);
    let holder = id("usr", 1);
    let mut storage = open_storage(&root);
    storage.device_execution_binding_ledger().unwrap();
    let connection = rusqlite::Connection::open(storage.database_path()).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_successor_dispatch_facts BEFORE INSERT ON device_execution_reservation_facts BEGIN SELECT RAISE(ABORT, 'injected before dispatch facts'); END;").unwrap();
    assert_eq!(
        application
            .command(
                &principal(&holder),
                CommandFamily::Session,
                chat_submit_request(1_611, 1, &holder, 1)
            )
            .unwrap_err()
            .status(),
        503
    );
    connection
        .execute_batch("DROP TRIGGER fail_successor_dispatch_facts")
        .unwrap();
    let job_id = queued_job_id(&mut storage);
    assert!(
        DeviceExecutionBindingService::new(&mut storage)
            .facts(&job_id.0)
            .unwrap()
            .is_none()
    );
    let predecessor = storage
        .worker_launch_grant_ledger()
        .unwrap()
        .newest_grant_for_product_session(&id("psn", 1))
        .unwrap()
        .unwrap();
    let binding = DeviceExecutionBindingService::new(&mut storage)
        .snapshot(&predecessor.worker_session_id)
        .unwrap()
        .unwrap();
    let now = instant("2027-01-15T08:00:02.000Z");
    DeviceExecutionBindingService::new(&mut storage)
        .release(
            &winwincode_storage::DeviceExecutionBindingRelease::try_new(
                &predecessor.worker_session_id,
                id("req", 1_612),
                binding.revision,
                now.clone(),
            )
            .unwrap(),
            &now,
        )
        .unwrap();
    let other = stage_node(&mut storage, 1_700);
    stage_client_grant(&mut storage, 1_701, &other, &holder);
    let repository_binding = stage_visible_binding(&mut storage, 1_702, &other, &holder);
    let (occupancy, fencing) = stage_occupied_lease(&mut storage, 1_703, &other, &holder);
    let successor = stage_anchor(
        &mut storage,
        1_704,
        &other,
        &holder,
        &occupancy,
        fencing,
        &repository_binding,
        &id("psn", 1),
    );
    assert_eq!(
        storage
            .worker_launch_grant_ledger()
            .unwrap()
            .newest_grant_for_product_session(&id("psn", 1))
            .unwrap()
            .unwrap()
            .worker_launch_grant_id,
        successor.worker_launch_grant_id
    );
    storage.initialize_device_auth().unwrap();
    connection.execute(
        "INSERT INTO device_identity_bindings VALUES(?1,'predecessor-public-key',?2,?3,'fixture-intent','fixture-token','{}',?4,NULL)",
        rusqlite::params![predecessor.client_node_id,holder,id("req",1_613),"2027-01-15T07:59:00.000Z"],
    ).unwrap();
    let original = storage.load_execution_job_record(&job_id).unwrap().unwrap();
    let reservation = storage
        .execution_admission()
        .unwrap()
        .load_reservation_by_job(&job_id)
        .unwrap()
        .unwrap();
    let successor_before = storage
        .worker_launch_grant_ledger()
        .unwrap()
        .snapshot(&successor.worker_launch_grant_id)
        .unwrap()
        .unwrap();
    storage
        .revoke_device_identity(
            &predecessor.client_node_id,
            &holder,
            "2027-01-15T08:00:07.000Z",
        )
        .unwrap();
    assert_eq!(
        storage.load_execution_job_record(&job_id).unwrap().unwrap(),
        original
    );
    assert_eq!(
        storage
            .execution_admission()
            .unwrap()
            .load_reservation_by_job(&job_id)
            .unwrap()
            .unwrap(),
        reservation
    );
    assert_eq!(
        storage
            .worker_launch_grant_ledger()
            .unwrap()
            .snapshot(&successor.worker_launch_grant_id)
            .unwrap()
            .unwrap(),
        successor_before
    );
    assert!(
        storage
            .execution_registry()
            .unwrap()
            .load_lease(&job_id)
            .unwrap()
            .is_none()
    );
    assert!(storage.pending_accounting_leases(0).unwrap().is_empty());
    application.shutdown().unwrap();
    drop(connection);
    drop(storage);
    std::fs::remove_dir_all(root).unwrap();
}
