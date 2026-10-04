use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_api::generated::{Actor, CommandRequest, Scope};
use winwincode_control_plane::{
    ControlPlane, ControlPlaneConfig, EventPublishError, EventPublisher,
    LocalDeliveryAdapterConfig, OutboxEvent, ProductSessionExecutionConfig,
};
use winwincode_domain::{
    DeliveryId, OrganizationId, ProjectId, RepositoryId, RepositoryScope, RepositoryScopeKind,
    UserActor, UserActorKind, UserId, WorkspaceId,
};
use winwincode_execution_port::runtime_trace_outbox::ExecutionMode;
use winwincode_server::{
    AuthenticatedPrincipal, CommandDispatchResponse, CommandFamily, DurableEventHub,
    DurableEventHubConfig, StandaloneApplicationClock, StandaloneControlPlaneApplication,
    TypedControlPlaneApiPort,
};
use winwincode_storage::{
    EXECUTION_PROTOCOL_VERSION, ExecutionJobState, RepositorySchedulerClaimRequest,
    RepositorySchedulerScope, SqliteStorage, WorkerAuthenticationIdentity, WorkerHeartbeatRequest,
    WorkerOutboundQueueConfig, WorkerPlatform, WorkerRegistrationRequest,
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct NoopPublisher;

impl EventPublisher for NoopPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Ok(())
    }
}

struct FixedClock;

impl StandaloneApplicationClock for FixedClock {
    fn now_millis(&self) -> u64 {
        1_800_000_000_000
    }

    fn now_instant(&self) -> winwincode_domain::Instant {
        winwincode_domain::Instant("2027-01-15T08:00:00.000Z".into())
    }
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn scope(seed: u64) -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(id("org", seed)),
        workspace_id: WorkspaceId(id("wsp", seed)),
        project_id: ProjectId(id("prj", seed)),
        repository_id: RepositoryId(id("rep", seed)),
    }
}

fn principal(scope: &RepositoryScope, seed: u64) -> AuthenticatedPrincipal {
    AuthenticatedPrincipal::new(
        Actor::UserActor(UserActor {
            id: UserId(id("usr", seed)),
            kind: UserActorKind::User,
        }),
        vec![Scope::RepositoryScope(scope.clone())],
    )
    .expect("principal")
}

fn execution_config(scope: &RepositoryScope) -> ProductSessionExecutionConfig {
    ProductSessionExecutionConfig::try_new(
        scope.clone(),
        "fixture-checkout-revision",
        "codex-chat",
        Some(3_600),
        1_073_741_824,
    )
    .expect("execution config")
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git command");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repository(root: &Path) -> String {
    fs::create_dir_all(root.join("src")).expect("repository");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    )
    .expect("manifest");
    fs::write(root.join("src/lib.rs"), "pub fn fixture() {}\n").expect("source");
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "fixture@example.invalid"]);
    git(root, &["config", "user.name", "Fixture"]);
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "fixture"]);
    let output = Command::new("git")
        .args([
            "-C",
            root.to_str().expect("repository path"),
            "rev-parse",
            "HEAD",
        ])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git revision");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .expect("git revision utf8")
        .trim()
        .to_owned()
}

fn command(
    scope: &RepositoryScope,
    delivery_id: &DeliveryId,
    work_run_id: &winwincode_domain::WorkRunId,
    actor_id: &str,
    expected_revision: u64,
) -> CommandRequest {
    serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": id("req", 40),
        "command": "workrun.cancel",
        "actor": { "kind": "user", "id": actor_id },
        "scope": scope,
        "expectedRevision": expected_revision,
        "payload": {
            "deliveryId": delivery_id,
            "workRunId": work_run_id
        }
    }))
    .expect("generated workrun.cancel command")
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the server cancellation test keeps public command, queue state, and replay together"
)]
fn server_workrun_cancel_uses_public_envelope_and_real_queued_workrun() {
    cancel_before_dispatch_acceptance(false, false);
}

#[test]
fn server_workrun_cancel_releases_unaccepted_lease_without_a_delivery_run() {
    cancel_before_dispatch_acceptance(true, false);
}

#[test]
fn server_workrun_cancel_releases_unaccepted_reviewer_lease() {
    cancel_before_dispatch_acceptance(true, true);
}

#[test]
fn server_workrun_cancel_stops_queued_reviewer_before_acceptance() {
    cancel_before_dispatch_acceptance(false, true);
}

#[allow(
    clippy::too_many_lines,
    reason = "exercise a public cancellation across production Delivery, queue, Registry, and restart"
)]
fn cancel_before_dispatch_acceptance(claim: bool, reviewer: bool) {
    let suffix = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "winwincode-server-workrun-cancel-{}-{suffix}",
        std::process::id()
    ));
    let data = root.join("data");
    let repository_root = root.join("repository");
    let scope = scope(1);
    let baseline = repository(&repository_root);
    let delivery_id = DeliveryId(id("dlv", 1));

    let mut control_plane = ControlPlane::start_local_with_delivery_adapters(
        ControlPlaneConfig::local(&data),
        Box::new(NoopPublisher),
        LocalDeliveryAdapterConfig::new(&repository_root, scope.clone())
            .with_execution_mode(ExecutionMode::DelegatedPatch),
    )
    .expect("production Delivery adapters");
    let create: CommandRequest = serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": id("req", 1),
        "command": "delivery.create",
        "actor": { "kind": "user", "id": id("usr", 1) },
        "scope": scope,
        "expectedRevision": 0,
        "payload": {
            "deliveryId": delivery_id,
            "spec": {
                "acceptanceCriteria": [{
                    "id": "criterion-1",
                    "required": true,
                    "title": "Repository tests pass"
                }],
                "baseRevision": baseline,
                "goal": "Ship the exact repository change",
                "scope": ["src"],
                "outOfScope": ["target"],
                "constraints": ["tests pass"],
                "sourceProductSessionId": null,
                "publicationTarget": null,
                "repositoryId": scope.repository_id,
                "title": "Production Delivery"
            }
        }
    }))
    .expect("generated delivery.create command");
    control_plane
        .delivery_create(match &create {
            CommandRequest::DeliveryCreateCommand(command) => command,
            _ => panic!("delivery.create variant"),
        })
        .expect("public Delivery create");
    let created_state = control_plane
        .load_state(&format!("delivery:{}", delivery_id.0))
        .expect("created Delivery state")
        .expect("created Delivery state exists");
    let created_delivery =
        winwincode_delivery::domain::Delivery::decode_json(&created_state.payload)
            .expect("decode created Delivery");
    let contract_revision = created_delivery
        .snapshot()
        .work_run_aggregate
        .contract
        .revision
        .0;
    let criterion_id = created_delivery
        .snapshot()
        .work_run_aggregate
        .contract
        .criteria
        .first()
        .expect("default acceptance criterion")
        .id
        .clone();
    let breakdown: CommandRequest = serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": id("req", 3),
        "command": "workitems.create",
        "actor": { "kind": "user", "id": id("usr", 1) },
        "scope": scope,
        "expectedRevision": 1,
        "payload": {
            "deliveryId": delivery_id,
            "expectedRevision": 1,
            "contractRevision": contract_revision,
            "items": [{
                "id": "wit_00000000000000000000000001",
                "title": "Implement the approved task",
                "goal": "Implement the approved repository change",
                "criterionIds": [criterion_id],
                "dependsOn": []
            }]
        }
    }))
    .expect("generated task-breakdown command");
    control_plane
        .work_items_create(match &breakdown {
            CommandRequest::WorkItemsCreateCommand(command) => command,
            _ => panic!("workitems.create variant"),
        })
        .expect("public WorkItem creation");
    let advance: CommandRequest = serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": id("req", 2),
        "command": "workrun.start",
        "actor": { "kind": "user", "id": id("usr", 1) },
        "scope": scope,
        "expectedRevision": 2,
        "payload": { "deliveryId": delivery_id, "dispatchProfile": "executor" }
    }))
    .expect("generated workrun.start command");
    control_plane
        .workrun_start(match &advance {
            CommandRequest::WorkRunStartCommand(command) => command,
            _ => panic!("workrun.start variant"),
        })
        .expect("public Delivery advance");
    let scheduler_scope = RepositorySchedulerScope {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
    };
    let mut storage = SqliteStorage::open(&data).expect("queue storage");
    let jobs = storage
        .repository_scheduler()
        .expect("repository scheduler")
        .list_jobs(&scheduler_scope, &[])
        .expect("queued jobs");
    let queued = jobs
        .iter()
        .find(|job| job.state == ExecutionJobState::Queued)
        .expect("generated queued WorkRun job");
    let mut work_run_id = queued.work_run_id.clone().expect("queued WorkRun id");
    let state = control_plane
        .load_state(&format!("delivery:{}", delivery_id.0))
        .expect("advanced Delivery state")
        .expect("advanced Delivery state exists");
    let mut expected_delivery_revision = state.revision;
    let delivery = winwincode_delivery::domain::Delivery::decode_json(&state.payload)
        .expect("Delivery before dispatch acceptance");
    assert!(delivery.snapshot().work_run_aggregate.runs.is_empty());
    if reviewer {
        let queued = queued.clone();
        work_run_id = seed_reviewer(&mut storage, &queued, delivery, &scheduler_scope);
        expected_delivery_revision += 1;
    }
    if claim {
        claim_without_acceptance(&mut storage, &scheduler_scope);
        assert_eq!(
            storage
                .repository_scheduler()
                .expect("scheduler")
                .list_jobs(&scheduler_scope, &[])
                .expect("leased jobs")
                .iter()
                .find(|job| job.work_run_id.as_ref() == Some(&work_run_id))
                .expect("exact leased job")
                .state,
            ExecutionJobState::Leased,
        );
    }
    drop(storage);
    control_plane.shutdown().expect("close Delivery host");

    let storage = SqliteStorage::open(&data).expect("application storage");
    let worker_outbound = winwincode_control_plane::DurableWorkerInteractionOutbound::new(
        SqliteStorage::open(&data).expect("Worker outbound storage"),
        WorkerOutboundQueueConfig::default(),
    )
    .expect("Worker outbound");
    let hub = Arc::new(
        DurableEventHub::open(data.join("events"), DurableEventHubConfig::default())
            .expect("event hub"),
    );
    let application = StandaloneControlPlaneApplication::new_with_clock(
        ControlPlane::start_local(ControlPlaneConfig::local(&data), Box::new(NoopPublisher))
            .expect("application Control Plane"),
        storage,
        worker_outbound,
        hub,
        Arc::new(FixedClock),
        execution_config(&scope),
    )
    .expect("server application");
    let user = principal(&scope, 1);
    let request = command(
        &scope,
        &delivery_id,
        &work_run_id,
        &id("usr", 1),
        expected_delivery_revision,
    );
    let mut wrong_revision = request.clone();
    if let CommandRequest::WorkRunCancelCommand(command) = &mut wrong_revision {
        command.expected_revision.0 += 1;
    }
    assert!(
        application
            .command(&user, CommandFamily::Delivery, wrong_revision)
            .is_err()
    );
    let wrong_delivery = command(
        &scope,
        &DeliveryId(id("dlv", 99)),
        &work_run_id,
        &id("usr", 1),
        expected_delivery_revision,
    );
    assert!(
        application
            .command(&user, CommandFamily::Delivery, wrong_delivery)
            .is_err()
    );
    let completed = application
        .command(&user, CommandFamily::Delivery, request.clone())
        .expect("server workrun.cancel");
    let completed = match completed {
        CommandDispatchResponse::Completed(response) => response,
        CommandDispatchResponse::Accepted(_) => panic!("queued cancellation must complete"),
    };
    let encoded = serde_json::to_value(&completed).expect("completed response");
    assert_eq!(encoded["command"], "workrun.cancel");
    assert_eq!(encoded["requestId"], id("req", 40));
    assert_eq!(encoded["outcome"], "completed");

    assert_eq!(
        application
            .command(&user, CommandFamily::Delivery, request.clone())
            .expect("server replay"),
        CommandDispatchResponse::Completed(completed.clone()),
    );
    let changed_actor = command(
        &scope,
        &delivery_id,
        &work_run_id,
        &id("usr", 2),
        expected_delivery_revision,
    );
    assert!(
        application
            .command(&user, CommandFamily::Delivery, changed_actor)
            .is_err(),
        "same requestId with changed public actor must conflict"
    );
    let mut facts = SqliteStorage::open(&data).expect("cancelled queue facts");
    let cancelled = facts
        .repository_scheduler()
        .expect("scheduler")
        .list_jobs(&scheduler_scope, &[])
        .expect("terminal jobs")
        .into_iter()
        .find(|job| job.work_run_id.as_ref() == Some(&work_run_id))
        .expect("cancelled job");
    assert_eq!(cancelled.state, ExecutionJobState::Failed);
    assert!(cancelled.cancellation.is_some());
    assert!(
        facts
            .execution_registry()
            .expect("registry")
            .load_dispatch_authority(&cancelled.job_id)
            .expect("dispatch authority")
            .is_none(),
        "unaccepted cancellation must not invent a WorkerSession or dispatch"
    );
    if claim {
        assert!(
            facts
                .execution_registry()
                .expect("registry")
                .load_live_lease(&cancelled.job_id, &FixedClock.now_instant())
                .expect("remaining lease")
                .is_none(),
            "cancellation must release the exact Registry lease"
        );
    }
    drop(facts);
    application.shutdown().expect("shutdown application");
    let restarted = StandaloneControlPlaneApplication::new_with_clock(
        ControlPlane::start_local(ControlPlaneConfig::local(&data), Box::new(NoopPublisher))
            .expect("restart application Control Plane"),
        SqliteStorage::open(&data).expect("restart application storage"),
        winwincode_control_plane::DurableWorkerInteractionOutbound::new(
            SqliteStorage::open(&data).expect("restart Worker outbound storage"),
            WorkerOutboundQueueConfig::default(),
        )
        .expect("restart Worker outbound"),
        Arc::new(
            DurableEventHub::open(data.join("events"), DurableEventHubConfig::default())
                .expect("restart event hub"),
        ),
        Arc::new(FixedClock),
        execution_config(&scope),
    )
    .expect("restart server application");
    assert_eq!(
        restarted
            .command(&user, CommandFamily::Delivery, request)
            .expect("server replay after restart"),
        CommandDispatchResponse::Completed(completed),
    );
    restarted
        .shutdown()
        .expect("shutdown restarted application");
    fs::remove_dir_all(root).expect("cleanup");
}

fn claim_without_acceptance(storage: &mut SqliteStorage, scope: &RepositorySchedulerScope) {
    use winwincode_domain::{
        ExecutionMessageId, ExecutionSequence, Instant, RequestId, Sha256Digest, WorkerId,
        WorkerInstanceId,
    };
    let now = Instant("2027-01-15T08:00:00.000Z".into());
    let worker_id = WorkerId(id("wrk", 50));
    let worker_instance_id = WorkerInstanceId(id("wki", 51));
    let mut registry = storage.execution_registry().expect("Registry");
    registry
        .register_worker(&WorkerRegistrationRequest {
            authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
                control_plane_principal: "fixture-control-plane".into(),
            },
            protocol_version: EXECUTION_PROTOCOL_VERSION.into(),
            platform: WorkerPlatform::Aarch64AppleDarwin,
            capabilities: vec!["codex".into()],
            capability_digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
            security_zone: "local".into(),
            max_slots: 1,
            message_id: ExecutionMessageId(id("xmsg", 52)),
            request_id: RequestId(id("req", 52)),
            sent_at: now.clone(),
            started_at: now.clone(),
            worker_id: worker_id.clone(),
            worker_instance_id: worker_instance_id.clone(),
        })
        .expect("register");
    registry
        .record_heartbeat(&WorkerHeartbeatRequest {
            active_leases: Vec::new(),
            available_slots: 1,
            heartbeat_sequence: ExecutionSequence(1),
            max_slots: 1,
            running_slots: 0,
            message_id: ExecutionMessageId(id("xmsg", 53)),
            observed_at: now.clone(),
            sent_at: now.clone(),
            worker_id: worker_id.clone(),
            worker_instance_id: worker_instance_id.clone(),
        })
        .expect("heartbeat");
    let dispatch = winwincode_control_plane::RepositoryExecutionScheduler::new(storage)
        .claim_next(&RepositorySchedulerClaimRequest {
            scope: scope.clone(),
            request_id: RequestId(id("req", 54)),
            scheduler_generation: "cancellation-before-acceptance".into(),
            worker_id,
            worker_instance_id,
            issued_at: now,
            expires_at: Instant("2027-01-15T08:05:00.000Z".into()),
        })
        .expect("claim")
        .expect("dispatch");
    assert!(
        storage
            .execution_registry()
            .expect("Registry")
            .load_dispatch_authority(&dispatch.job.job_id)
            .expect("accepted authority")
            .is_none()
    );
}

// Seed the candidate-ready input phase without invoking an external Provider.
// The original Executor queue intent is closed through the scheduler; a fresh
// immutable Reviewer job is submitted through the production storage port.
fn seed_reviewer(
    storage: &mut SqliteStorage,
    executor: &winwincode_storage::ExecutionJobRecord,
    delivery: winwincode_delivery::domain::Delivery,
    scope: &RepositorySchedulerScope,
) -> winwincode_domain::WorkRunId {
    use winwincode_domain::{
        ExecutionJobId, Instant, RequestId, Sha256Digest, WorkItemState, WorkRunId,
    };
    use winwincode_execution_port::generated::{
        ExecutionJob, ExecutionScope, ExecutionWorkspaceWriteMode,
    };
    use winwincode_storage::{
        ExecutionJobSubmission, NewOutboxEvent, ProductStateStorage, ReceiptActorKey,
        ReceiptIdentity, ReceiptScopeKey, RepositorySchedulerCancellationRequest, StateCommit,
    };
    let now = Instant("2027-01-15T08:00:00.000Z".into());
    winwincode_control_plane::RepositoryExecutionScheduler::new(storage)
        .request_cancellation(&RepositorySchedulerCancellationRequest {
            scope: scope.clone(),
            job_id: executor.job_id.clone(),
            request_id: RequestId(id("req", 61)),
            expected_revision: executor.revision,
            requested_at: now.clone(),
        })
        .expect("close unaccepted Executor intent");
    let mut snapshot = delivery.into_snapshot();
    let previous_revision = snapshot.revision;
    snapshot.revision += 1;
    snapshot.work_run_aggregate.items[0].state = WorkItemState::CandidateReady;
    let delivery = winwincode_delivery::domain::Delivery::try_from_snapshot(snapshot)
        .expect("candidate-ready Delivery fixture");
    storage
        .commit(&StateCommit::new(
            ReceiptIdentity::new(
                ReceiptActorKey::from_encoded(b"fixture-actor".to_vec()).expect("fixture actor"),
                ReceiptScopeKey::from_encoded(b"fixture-scope".to_vec()).expect("fixture scope"),
                RequestId(id("req", 62)),
            )
            .expect("fixture receipt"),
            Sha256Digest(format!("sha256:{}", "c".repeat(64))),
            format!("delivery:{}", delivery.id().0),
            previous_revision,
            delivery.encode_json().expect("candidate-ready Delivery"),
            vec![NewOutboxEvent::internal(
                "fixture-candidate-ready",
                "fixture.phase",
                b"candidate-ready".to_vec(),
            )],
        ))
        .expect("candidate-ready fixture commit");
    let mut job: ExecutionJob =
        serde_json::from_slice(&executor.dispatch_payload).expect("Executor input");
    job.job_id = ExecutionJobId(id("job", 63));
    job.execution_profile = "reviewer".into();
    job.workspace.write_mode = ExecutionWorkspaceWriteMode::ReadOnly;
    job.workspace.checkout_revision = "a".repeat(40);
    let work_run_id = WorkRunId(id("wrn", 63));
    let ExecutionScope::WorkRunExecutionScope(execution_scope) = &mut job.scope else {
        panic!("WorkRun scope")
    };
    execution_scope.work_run_id = work_run_id.clone();
    let input = job.work_input.as_mut().expect("workInput");
    input.work_item = delivery.snapshot().work_run_aggregate.items[0].clone();
    input.work_plan = Some(delivery.snapshot().work_run_aggregate.items.clone());
    input.candidate_ref = Some(format!("refs/winwincode/candidates/{}", "a".repeat(40)));
    storage
        .execution_queue()
        .expect("queue")
        .submit(&ExecutionJobSubmission {
            scope: executor.scope.clone(),
            job_id: job.job_id.clone(),
            request_id: RequestId(id("req", 63)),
            payload_digest: job.payload_digest.clone(),
            dispatch_payload: serde_json::to_vec(&job).expect("Reviewer payload"),
            attempt: 1,
            dependencies: Vec::new(),
            work_run_id: Some(work_run_id.clone()),
            submitted_at: now,
        })
        .expect("immutable Reviewer job");
    work_run_id
}
