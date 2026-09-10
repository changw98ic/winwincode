// SPDX-License-Identifier: Apache-2.0

//! The `FLOW-100.5` `StrongFlow` role-to-Device `WorkerSession` routing over the
//! real composed Server application with its production-local Delivery
//! authority: the `delivery.advance` that commits a Codex `WorkRun`'s
//! `ExecutionJob` routes that job to the `WorkRun`'s launched Device
//! `WorkerSession` when a durable launch anchor exists (after the
//! FLOW-100.3 permission gate approves the acting user), while a `WorkRun`
//! without an anchor keeps the supervised local execution path unchanged —
//! and a gate denial routes nothing.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use winwincode_api::generated::{
    Actor, CommandRequest, OrganizationScope, OrganizationScopeKind, Scope,
};
use winwincode_control_plane::{
    ControlPlane, ControlPlaneConfig, DeviceExecutionBindingService,
    DurableWorkerInteractionOutbound, EventPublishError, EventPublisher,
    LocalDeliveryAdapterConfig, OutboxEvent, ProductSessionExecutionConfig,
    RepositoryExecutionScheduler, WorkerLaunchGrantService,
};
use winwincode_domain::{
    ExecutionJobId, ExecutionMessageId, Instant, OrganizationId, ProjectId, RepositoryId,
    RepositoryScope, RepositoryScopeKind, RequestId, Sha256Digest, UserActor, UserActorKind,
    UserId, WorkerId, WorkerInstanceId, WorkspaceId,
};
use winwincode_server::{
    ApiError, AuthenticatedPrincipal, CommandDispatchResponse, CommandFamily, DurableEventHub,
    DurableEventHubConfig, StandaloneApplicationClock, StandaloneControlPlaneApplication,
    TypedControlPlaneApiPort,
};
use winwincode_storage::{
    AccessGrantIssuance, ClientNodeRegistration, ClientPresenceState, EXECUTION_PROTOCOL_VERSION,
    ExecutionJobState, GrantPermissions, GrantSource, GrantTrustMode, LaunchGrantIssuance,
    OccupancyClaim, OccupancyLeaseState, ProductStateStorage, RepositoryAccessGrantIssuance,
    RepositoryAvailability, RepositoryBindingProjection, RepositoryDirtyState,
    RepositoryGrantPermissions, RepositorySchedulerClaimRequest, RepositorySchedulerScope,
    SqliteStorage, WorkerAuthenticationIdentity, WorkerPlatform, WorkerRegistrationRequest,
    WorkerRegistrationStatus,
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

struct HubPublisher {
    hub: Arc<DurableEventHub>,
}

impl EventPublisher for HubPublisher {
    fn publish(&mut self, event: &OutboxEvent) -> Result<(), EventPublishError> {
        self.hub
            .publish_committed(event)
            .map(|_| ())
            .map_err(|error| EventPublishError::new(error.to_string()))
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
        "winwincode-server-strongflow-device-execution-{label}-{}-{suffix}",
        std::process::id()
    ))
}

fn canonical_id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn ulid(seed: u64) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut identity = String::with_capacity(26);
    let mut value = seed;
    for _ in 0..26 {
        identity.push(ALPHABET[usize::try_from(value % 32).expect("digit fits")] as char);
        value /= 32;
    }
    identity
}

fn ulid_id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{}", ulid(seed))
}

fn instant(value: &str) -> Instant {
    Instant(value.to_owned())
}

fn api_scope() -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(canonical_id("org", 1)),
        workspace_id: WorkspaceId(canonical_id("wsp", 1)),
        project_id: ProjectId(canonical_id("prj", 1)),
        repository_id: RepositoryId(canonical_id("rep", 1)),
    }
}

fn scheduler_scope() -> RepositorySchedulerScope {
    RepositorySchedulerScope {
        organization_id: OrganizationId(canonical_id("org", 1)),
        workspace_id: WorkspaceId(canonical_id("wsp", 1)),
        project_id: ProjectId(canonical_id("prj", 1)),
        repository_id: RepositoryId(canonical_id("rep", 1)),
    }
}

fn repository_scope_json() -> serde_json::Value {
    serde_json::json!({
        "kind": "repository",
        "organizationId": canonical_id("org", 1),
        "workspaceId": canonical_id("wsp", 1),
        "projectId": canonical_id("prj", 1),
        "repositoryId": canonical_id("rep", 1)
    })
}

fn principal_scope() -> Vec<Scope> {
    vec![
        Scope::OrganizationScope(OrganizationScope {
            kind: OrganizationScopeKind::Organization,
            organization_id: OrganizationId(canonical_id("org", 1)),
        }),
        serde_json::from_value::<Scope>(repository_scope_json()).expect("repository scope"),
    ]
}

fn actor(user: &str) -> Actor {
    Actor::UserActor(UserActor {
        id: UserId(user.to_owned()),
        kind: UserActorKind::User,
    })
}

fn principal(user: &str) -> AuthenticatedPrincipal {
    AuthenticatedPrincipal::new(actor(user), principal_scope()).expect("principal")
}

// ---- fixture repository (the production-local Delivery authority input) ----

fn initialize_repository(repository: &Path) -> String {
    std::fs::create_dir_all(repository.join("src")).expect("create repository");
    std::fs::write(
        repository.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    )
    .expect("write manifest");
    std::fs::write(repository.join("src/lib.rs"), "pub fn fixture() {}\n").expect("write source");
    git(repository, &["init", "-q"]);
    git(
        repository,
        &["config", "user.email", "fixture@example.invalid"],
    );
    git(repository, &["config", "user.name", "Fixture"]);
    git(repository, &["add", "."]);
    git(repository, &["commit", "-q", "-m", "fixture"]);
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repository)
        .output()
        .expect("read baseline");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .expect("UTF-8 baseline")
        .trim()
        .to_owned()
}

fn git(repository: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .args(arguments)
        .current_dir(repository)
        .status()
        .expect("run Git");
    assert!(status.success(), "Git command failed: {arguments:?}");
}

// ---- composed application with the production-local Delivery authority -----

fn compose_application(root: &Path) -> StandaloneControlPlaneApplication {
    let hub = Arc::new(
        DurableEventHub::open(root.join("events"), DurableEventHubConfig::default())
            .expect("open event hub"),
    );
    let control_plane = ControlPlane::start_local_with_delivery_adapters(
        ControlPlaneConfig::local(root),
        Box::new(HubPublisher {
            hub: Arc::clone(&hub),
        }),
        LocalDeliveryAdapterConfig::new(root.join("repository"), api_scope()),
    )
    .expect("open Control Plane with the local Delivery authority");
    let storage = SqliteStorage::open(root).expect("open application storage");
    let worker_outbound = DurableWorkerInteractionOutbound::new(
        SqliteStorage::open(root).expect("open Worker outbound storage"),
        winwincode_storage::WorkerOutboundQueueConfig::default(),
    )
    .expect("open Worker outbound adapter");
    let execution = ProductSessionExecutionConfig::try_new(
        serde_json::from_value(repository_scope_json()).expect("repository scope"),
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

#[allow(clippy::too_many_lines)]
fn work_run_device_fixture(
    root: &Path,
    seed: u64,
    user: &str,
) -> (String, String, String, u64, String) {
    let mut storage = SqliteStorage::open(root).expect("open staging storage");
    let node = canonical_id("cnd", seed);
    let instance = canonical_id("cix", seed + 1);
    {
        let registration = ClientNodeRegistration::try_new(
            node.clone(),
            format!("{seed:010}"),
            "StrongFlow Device Test Device".to_owned(),
            "aarch64-apple-darwin",
            "aarch64",
            "1.2.3",
            None,
            Some(instance.clone()),
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
    }
    {
        let issuance = AccessGrantIssuance::try_new(
            canonical_id("cag", seed + 2),
            &node,
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
    let binding = canonical_id("rbd", seed + 3);
    {
        let mut ledger = storage.repository_binding_ledger().expect("ledger");
        let projection = RepositoryBindingProjection::try_new(
            binding.clone(),
            &node,
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
            canonical_id("rag", seed + 4),
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
    }
    let (lease_id, fencing_token) = {
        let mut occupancy = storage.client_occupancy_ledger().expect("ledger");
        let claim = OccupancyClaim::try_new(
            canonical_id("ocl", seed + 5),
            &node,
            user,
            canonical_id("req", seed + 6),
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
    };
    (node, instance, lease_id, fencing_token, binding)
}

/// The device identities one staged `WorkRun` anchor exposes to the test.
#[allow(clippy::struct_field_names)]
struct AnchorLaunch {
    worker_launch_grant_id: String,
    worker_session_id: String,
    worker_id: String,
    worker_instance_id: String,
}

/// Stages the launch anchor of one Delivery `WorkRun` exactly as the Client's
/// two-phase scheduler would after the user asked the device to run it.
#[allow(clippy::too_many_arguments)]
fn work_run_anchor(
    root: &Path,
    seed: u64,
    node: &str,
    instance: &str,
    user: &str,
    lease_id: &str,
    fencing_token: u64,
    binding: &str,
    product_session_id: &str,
    work_run_id: &str,
) -> AnchorLaunch {
    let mut storage = SqliteStorage::open(root).expect("open staging storage");
    let worker_launch_grant_id = ulid_id("wlg", seed);
    let worker_session_id = ulid_id("ws", seed + 1);
    let worker_id = ulid_id("wkr", seed + 2);
    let worker_instance_id = ulid_id("winst", seed + 3);
    let issuance = LaunchGrantIssuance::try_new(
        worker_launch_grant_id.clone(),
        node,
        instance,
        user,
        lease_id,
        fencing_token,
        binding,
        worker_session_id.clone(),
        worker_id.clone(),
        worker_instance_id.clone(),
        "sha256:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
        Some(product_session_id.to_owned()),
        Some(winwincode_domain::WorkRunId(work_run_id.to_owned())),
        instant("2100-01-01T00:00:00.000Z"),
    )
    .expect("issuance");
    WorkerLaunchGrantService::new(&mut storage)
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
fn settle_launch(root: &Path, anchor: &AnchorLaunch, lease_id: &str, token: u64) {
    let mut storage = SqliteStorage::open(root).expect("open staging storage");
    let settlement = winwincode_storage::LaunchAckSettlement::try_new(
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
    let outcome = WorkerLaunchGrantService::new(&mut storage)
        .settle_launch_ack(&settlement, &instant("2026-09-04T12:02:30.000Z"))
        .expect("settle launch ack");
    assert!(matches!(
        outcome,
        winwincode_storage::LaunchAckOutcome::Consumed(_)
    ));
}

// ---- generated command helpers ---------------------------------------------

fn delivery_create_request(
    request: u64,
    delivery: u64,
    user: &str,
    baseline: &str,
) -> CommandRequest {
    serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": canonical_id("req", request),
        "command": "delivery.create",
        "actor": { "kind": "user", "id": user },
        "scope": repository_scope_json(),
        "expectedRevision": 0,
        "payload": {
            "deliveryId": ulid_id("dlv", delivery),
            "spec": {
                "acceptanceCriteria": [
                    { "id": canonical_id("crt", 1), "required": true, "title": "Repository tests pass" }
                ],
                "baseRevision": baseline,
                "constraints": ["tests pass"],
                "goal": "Ship the exact repository change",
                "outOfScope": ["target"],
                "publicationTarget": null,
                "repositoryId": canonical_id("rep", 1),
                "scope": ["src"],
                "sourceProductSessionId": null,
                "title": "StrongFlow Device Delivery"
            },
            "tasks": []
        }
    }))
    .expect("generated delivery.create command")
}

fn delivery_advance_request(
    request: u64,
    delivery: u64,
    user: &str,
    expected_revision: i64,
) -> CommandRequest {
    serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": canonical_id("req", request),
        "command": "delivery.advance",
        "actor": { "kind": "user", "id": user },
        "scope": repository_scope_json(),
        "expectedRevision": expected_revision,
        "payload": {
            "deliveryId": ulid_id("dlv", delivery),
            "dispatchProfile": "executor"
        }
    }))
    .expect("generated delivery.advance command")
}

fn delivery_task_breakdown_request(request: u64, delivery: u64, user: &str) -> CommandRequest {
    let command = serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": canonical_id("req", request),
        "command": "delivery.task_breakdown.create",
        "actor": { "kind": "user", "id": user },
        "scope": repository_scope_json(),
        "expectedRevision": 1,
        "payload": {
            "deliveryId": ulid_id("dlv", delivery),
            "expectedRevision": 1,
            "contractRevision": 1,
            "items": [{
                "criterionIds": ["crt_00000000000000000000000001"],
                "dependsOn": [],
                "goal": "Ship the exact repository change",
                "id": "wit_00000000000000000000000001",
                "title": "Ship the exact repository change"
            }]
        }
    }))
    .expect("generated delivery.task_breakdown.create command");
    CommandRequest::DeliveryTaskBreakdownCreateCommand(command)
}

fn completed(response: CommandDispatchResponse) -> serde_json::Value {
    let CommandDispatchResponse::Completed(response) = response else {
        panic!("Delivery command must complete synchronously");
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
        .list_jobs(&scheduler_scope(), &[ExecutionJobState::Queued])
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
    let worker_id = WorkerId(ulid_id("wrk", seed));
    let worker_instance_id = WorkerInstanceId(ulid_id("wki", seed));
    let request = WorkerRegistrationRequest {
        authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
            control_plane_principal: "strongflow-device-test".to_owned(),
        },
        protocol_version: EXECUTION_PROTOCOL_VERSION.to_owned(),
        platform: WorkerPlatform::Aarch64AppleDarwin,
        capabilities: vec!["codex".to_owned()],
        capability_digest: Sha256Digest(format!("sha256:{}", "0".repeat(64))),
        security_zone: "local".to_owned(),
        max_slots: 4,
        message_id: ExecutionMessageId(ulid_id("xmsg", seed)),
        request_id: RequestId(ulid_id("req", seed)),
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
        scope: scheduler_scope(),
        request_id: RequestId(ulid_id("req", request)),
        scheduler_generation: "gen-strongflow-device-test".to_owned(),
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
fn a_device_anchored_work_run_is_dispatched_to_its_launched_worker_session() {
    let root = temporary_root("anchored-work-run-dispatch");
    std::fs::create_dir_all(root.join("repository")).expect("repository directory");
    let baseline = initialize_repository(&root.join("repository"));
    let application = compose_application(&root);
    let holder = canonical_id("usr", 1);
    // The holder creates the Delivery and advances it once: the first Codex
    // WorkRun's job (the requirements role) is committed with no device anchor
    // yet.
    application
        .command(
            &principal(&holder),
            CommandFamily::Delivery,
            delivery_create_request(10, 1, &holder, &baseline),
        )
        .expect("create Delivery");
    application
        .command(
            &principal(&holder),
            CommandFamily::Delivery,
            delivery_task_breakdown_request(12, 1, &holder),
        )
        .expect("create canonical WorkItem");
    application
        .command(
            &principal(&holder),
            CommandFamily::Delivery,
            delivery_advance_request(11, 1, &holder, 2),
        )
        .expect("advance Delivery");
    let (job_id, work_run_id, product_session_id) = {
        let mut storage = open_storage(&root);
        let job_id = queued_job_id(&mut storage);
        let record = storage
            .load_execution_job_record(&job_id)
            .expect("job record")
            .expect("queued job");
        let work_run_id = record.work_run_id.clone().expect("Delivery WorkRun");
        (job_id, work_run_id, record.scope.product_session_id.clone())
    };
    // The Client launches this role's WorkerSession for exactly this WorkRun
    // run, and the control plane settles the launch acknowledgement.
    let (node, instance, lease_id, fencing_token, binding) =
        work_run_device_fixture(&root, 100, &holder);
    let anchor = work_run_anchor(
        &root,
        104,
        &node,
        &instance,
        &holder,
        &lease_id,
        fencing_token,
        &binding,
        product_session_id.0.as_str(),
        work_run_id.0.as_str(),
    );
    settle_launch(&root, &anchor, &lease_id, fencing_token);
    // The exact advance replay re-runs the routing: the anchor now exists,
    // so the committed planner job is dispatched to the launched session.
    application
        .command(
            &principal(&holder),
            CommandFamily::Delivery,
            delivery_advance_request(11, 1, &holder, 2),
        )
        .expect("receipt-first advance replay completes the dispatch");
    let facts = {
        let mut storage = open_storage(&root);
        // The launch material is the device session's durable ExecutionPort
        // identity, and the WorkRun job carries the exact device facts with the
        // WorkRun's role stamped on them.
        let bound = binding_snapshot(&mut storage, &anchor.worker_session_id)
            .expect("the device session is bound");
        assert_eq!(bound.state.as_str(), "bound");
        assert_eq!(
            &bound.worker_launch_grant_id,
            &anchor.worker_launch_grant_id
        );
        let facts = DeviceExecutionBindingService::new(&mut storage)
            .facts(job_id.0.as_str())
            .expect("facts lookup")
            .expect("the WorkRun job carries device facts");
        assert_eq!(facts.role.as_deref(), Some("executor"));
        assert_eq!(facts.worker_session_id, anchor.worker_session_id);
        assert_eq!(facts.holder_user_id, holder);
        assert_eq!(
            facts.product_session_id.as_deref(),
            Some(product_session_id.0.as_str())
        );
        assert_eq!(facts.work_run_id.as_deref(), Some(work_run_id.0.as_str()));
        // The job itself stays queued for the device worker.
        let record = storage
            .load_execution_job_record(&job_id)
            .expect("job record")
            .expect("queued job");
        assert_eq!(record.state, ExecutionJobState::Queued);
        facts
    };
    // The local embedded worker cannot claim the device-owned WorkRun job: the
    // queue selection excludes it, so a local drive finds nothing.
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
    let _ = facts;

    application.shutdown().expect("shutdown");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_unanchored_work_run_keeps_the_local_execution_path() {
    let root = temporary_root("work-run-local-path");
    std::fs::create_dir_all(root.join("repository")).expect("repository directory");
    let baseline = initialize_repository(&root.join("repository"));
    let application = compose_application(&root);
    let user = canonical_id("usr", 1);
    application
        .command(
            &principal(&user),
            CommandFamily::Delivery,
            delivery_create_request(60, 1, &user, &baseline),
        )
        .expect("create Delivery");
    application
        .command(
            &principal(&user),
            CommandFamily::Delivery,
            delivery_task_breakdown_request(62, 1, &user),
        )
        .expect("create canonical WorkItem");
    application
        .command(
            &principal(&user),
            CommandFamily::Delivery,
            delivery_advance_request(61, 1, &user, 2),
        )
        .expect("advance Delivery");
    let mut storage = open_storage(&root);
    let job_id = queued_job_id(&mut storage);
    assert!(
        DeviceExecutionBindingService::new(&mut storage)
            .facts(job_id.0.as_str())
            .expect("facts lookup")
            .is_none(),
        "an unanchored WorkRun job must not carry device facts"
    );
    assert!(binding_snapshot(&mut storage, &ulid_id("ws", 105)).is_none());
    // The local embedded worker claims the WorkRun job exactly as before.
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
    let root = temporary_root("work-run-gate-denial");
    std::fs::create_dir_all(root.join("repository")).expect("repository directory");
    let baseline = initialize_repository(&root.join("repository"));
    let application = compose_application(&root);
    let holder = canonical_id("usr", 1);
    let member = canonical_id("usr", 2);
    // The member drives their own Delivery: the advance that commits the
    // first Codex WorkRun's job carries no device anchor yet.
    application
        .command(
            &principal(&member),
            CommandFamily::Delivery,
            delivery_create_request(80, 1, &member, &baseline),
        )
        .expect("create Delivery");
    application
        .command(
            &principal(&member),
            CommandFamily::Delivery,
            delivery_task_breakdown_request(82, 1, &member),
        )
        .expect("create canonical WorkItem");
    let advance = delivery_advance_request(81, 1, &member, 2);
    application
        .command(
            &principal(&member),
            CommandFamily::Delivery,
            advance.clone(),
        )
        .expect("advance Delivery");
    let (work_run_id, product_session_id) = {
        let mut storage = open_storage(&root);
        let job_id = queued_job_id(&mut storage);
        let record = storage
            .load_execution_job_record(&job_id)
            .expect("job record")
            .expect("queued job");
        (
            record.work_run_id.clone().expect("Delivery WorkRun"),
            record.scope.product_session_id.clone(),
        )
    };
    // The WorkRun's launch anchor belongs to the holder's occupied device.
    let (node, instance, lease_id, fencing_token, binding) =
        work_run_device_fixture(&root, 800, &holder);
    let anchor = work_run_anchor(
        &root,
        804,
        &node,
        &instance,
        &holder,
        &lease_id,
        fencing_token,
        &binding,
        product_session_id.0.as_str(),
        work_run_id.0.as_str(),
    );
    // The member's exact replay commits as a receipt-first replay, but the
    // FLOW-100.3 gate denies the device dispatch with the central gate wire
    // code: nothing is bound and no device facts are attached.
    let denial: ApiError = application
        .command(&principal(&member), CommandFamily::Delivery, advance)
        .expect_err("a non-holder must not dispatch to the holder's device");
    assert_eq!(denial.status(), 403);
    assert_eq!(denial.code(), "ACCESS_DENIED");
    let mut storage = open_storage(&root);
    assert!(binding_snapshot(&mut storage, &anchor.worker_session_id).is_none());
    let jobs = storage
        .repository_scheduler()
        .expect("scheduler")
        .list_jobs(&scheduler_scope(), &[ExecutionJobState::Queued])
        .expect("queued jobs");
    for job in &jobs {
        assert!(
            DeviceExecutionBindingService::new(&mut storage)
                .facts(job.job_id.0.as_str())
                .expect("facts lookup")
                .is_none(),
            "a denied WorkRun job must not carry device facts"
        );
    }

    application.shutdown().expect("shutdown");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_work_run_dispatch_replays_exactly_without_new_facts() {
    let root = temporary_root("work-run-dispatch-replay");
    std::fs::create_dir_all(root.join("repository")).expect("repository directory");
    let baseline = initialize_repository(&root.join("repository"));
    let application = compose_application(&root);
    let holder = canonical_id("usr", 1);
    application
        .command(
            &principal(&holder),
            CommandFamily::Delivery,
            delivery_create_request(50, 1, &holder, &baseline),
        )
        .expect("create Delivery");
    application
        .command(
            &principal(&holder),
            CommandFamily::Delivery,
            delivery_task_breakdown_request(52, 1, &holder),
        )
        .expect("create canonical WorkItem");
    let advance = delivery_advance_request(51, 1, &holder, 2);
    application
        .command(
            &principal(&holder),
            CommandFamily::Delivery,
            advance.clone(),
        )
        .expect("advance Delivery");
    let (job_id, work_run_id, product_session_id) = {
        let mut storage = open_storage(&root);
        let job_id = queued_job_id(&mut storage);
        let record = storage
            .load_execution_job_record(&job_id)
            .expect("job record")
            .expect("queued job");
        (
            job_id,
            record.work_run_id.clone().expect("Delivery WorkRun"),
            record.scope.product_session_id.clone(),
        )
    };
    let (node, instance, lease_id, fencing_token, binding) =
        work_run_device_fixture(&root, 500, &holder);
    let anchor = work_run_anchor(
        &root,
        504,
        &node,
        &instance,
        &holder,
        &lease_id,
        fencing_token,
        &binding,
        product_session_id.0.as_str(),
        work_run_id.0.as_str(),
    );
    settle_launch(&root, &anchor, &lease_id, fencing_token);
    let first = completed(
        application
            .command(
                &principal(&holder),
                CommandFamily::Delivery,
                advance.clone(),
            )
            .expect("first dispatching advance"),
    );
    let replay = completed(
        application
            .command(&principal(&holder), CommandFamily::Delivery, advance)
            .expect("exact replay"),
    );
    assert_eq!(first, replay);
    let mut storage = open_storage(&root);
    let facts = DeviceExecutionBindingService::new(&mut storage)
        .facts(job_id.0.as_str())
        .expect("facts lookup")
        .expect("the WorkRun job carries device facts");
    assert_eq!(facts.role.as_deref(), Some("executor"));
    assert_eq!(facts.worker_session_id, anchor.worker_session_id);
    assert_eq!(facts.attached_at, instant("2027-01-15T08:00:00.000Z"));
    assert_eq!(queued_job_id(&mut storage), job_id);

    application.shutdown().expect("shutdown");
    let _ = std::fs::remove_dir_all(&root);
}
