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
    ExecutionJobState, RepositorySchedulerScope, SqliteStorage, WorkerOutboundQueueConfig,
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
        3_600,
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
            },
            "tasks": []
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
        "command": "delivery.task_breakdown.create",
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
        .delivery_task_breakdown_create(match &breakdown {
            CommandRequest::DeliveryTaskBreakdownCreateCommand(command) => command,
            _ => panic!("delivery.task_breakdown.create variant"),
        })
        .expect("public WorkItem creation");
    let advance: CommandRequest = serde_json::from_value(serde_json::json!({
        "schemaVersion": "winwincode/v1",
        "requestId": id("req", 2),
        "command": "delivery.advance",
        "actor": { "kind": "user", "id": id("usr", 1) },
        "scope": scope,
        "expectedRevision": 2,
        "payload": { "deliveryId": delivery_id, "dispatchProfile": "executor" }
    }))
    .expect("generated delivery.advance command");
    control_plane
        .delivery_advance(match &advance {
            CommandRequest::DeliveryAdvanceCommand(command) => command,
            _ => panic!("delivery.advance variant"),
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
    let work_run_id = queued.work_run_id.clone().expect("queued WorkRun id");
    let state = control_plane
        .load_state(&format!("delivery:{}", delivery_id.0))
        .expect("advanced Delivery state")
        .expect("advanced Delivery state exists");
    let expected_delivery_revision = state.revision;
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
