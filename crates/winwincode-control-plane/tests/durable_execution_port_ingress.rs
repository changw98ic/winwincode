// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};

use serde_json::{Value, from_value};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, CommandEnvelope, CommandName, DeliveryAdvanceCommand, Scope,
};
use winwincode_control_plane::{
    ControlPlane, ControlPlaneConfig, DurableExecutionPortContext, DurableExecutionPortDelegate,
    DurableExecutionPortError, DurableExecutionPortIngress, DurableExecutionPortSupplement,
    EventPublishError, EventPublisher, OutboxEvent, RepositoryExecutionScheduler, StateChange,
};
use winwincode_delivery::{
    domain::{DELIVERY_SCHEMA_VERSION, Delivery, DeliveryStatus, DeliveryTask, DeliveryTaskStatus},
    store::{
        AtomicPublication, CreateDelivery, DeliveryCommand, DeliveryCommandPort,
        DeliveryJournalPort, DeliveryStore, JournalBackendError, LoadedDeliveryJournal,
    },
};
use winwincode_domain::{
    CodexThreadId, DeliveryId, ExecutionEventId, ExecutionJobId, ExecutionMessageId,
    ExecutionSequence, FencingToken, Instant, LeaseId, ProductSessionId, RequestId, Revision,
    SchemaVersion, SessionBindingSourceIdentity, SessionBindingSourceIdentityKind, SessionIdentity,
    Sha256Digest, UserId, WorkerInstanceId, WorkerSessionId,
};
use winwincode_domain::{
    OrganizationId, ProjectId, RepositoryId, RepositoryScope, RepositoryScopeKind, UserActor,
    WorkContractId, WorkItemId, WorkItemState, WorkRunId, WorkspaceId,
};
use winwincode_execution_port::generated::{
    ExecutionEventCategory, ExecutionEventRecord, ExecutionJob, ExecutionLeaseStamp,
    ExecutionLimits, ExecutionOutcomeStatus, ExecutionPortMessage, ExecutionScope,
    ExecutionWorkspace, ExecutionWorkspaceWriteMode, JobDispatchResultMessage,
    JobDispatchResultMessageKind, JobDispatchResultMessageStatus, LeaseWriteStatus,
    ProductSessionExecutionScope, ProductSessionExecutionScopeKind, RuntimeEventMessage,
    RuntimeEventMessageKind, SessionBindingMessage, SessionBindingMessageKind, WorkerCapacity,
    WorkerRegisterMessage, WorkerRegistrationResultMessageStatus,
};
use winwincode_execution_port::transport::{
    EndpointSide, FrameDirection, LocalWorkerAdapter, RemoteTransportAdapter, TypedFrame,
};
use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord,
    ExecutionAdmissionBoundary, ExecutionAdmissionLimits, ExecutionAdmissionPolicy,
    ExecutionJobState, ExecutionJobSubmission, ExecutionJobTransitionRequest, ExecutionLeaseClaim,
    ExecutionLeaseTerminalOutcome, ExecutionLeaseTerminalRequest, ExecutionQueueScope,
    ExecutionRepositoryAccess, ExecutionReservationRequest, ExecutionReservationStart,
    NewOutboxEvent, ProductStateStorage, PublicEventActor, PublicEventScope,
    RepositorySchedulerClaimRequest, RepositorySchedulerRetryRequest, RepositorySchedulerScope,
    SchedulerRetryPolicy, SqliteStorage, StateCommit, WorkerPoolId, WorkerSlotAuthority,
    WorkerSlotOpenRequest, WorkerSlotResourceLimits, WorkerSlotResources, receipt_actor_key,
    receipt_scope_key,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-durable-execution-port-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn canonical_id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn repository_scope(seed: u64) -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(canonical_id("org", seed)),
        workspace_id: WorkspaceId(canonical_id("wsp", seed)),
        project_id: ProjectId(canonical_id("prj", seed)),
        repository_id: RepositoryId(canonical_id("rep", seed)),
    }
}

fn product_session_catalog_stream_id(scope: &RepositoryScope) -> String {
    let scope_key = receipt_scope_key(&PublicEventScope::Repository {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
    })
    .expect("repository receipt scope");
    format!(
        "product-sessions:{:x}",
        Sha256::digest(scope_key.as_bytes())
    )
}

fn seed_queue_lease(
    storage: &mut SqliteStorage,
    repository: &RepositoryScope,
    job: &ExecutionJob,
    claim: &ExecutionLeaseClaim,
    seed: u64,
    workrun_delivery_id: Option<DeliveryId>,
) {
    let (product_session_id, delivery_id, work_run_id) = match &job.scope {
        ExecutionScope::ProductSessionExecutionScope(scope) => {
            (scope.product_session_id.clone(), None, None)
        }
        ExecutionScope::WorkRunExecutionScope(scope) => (
            scope.product_session_id.clone(),
            workrun_delivery_id,
            Some(scope.work_run_id.clone()),
        ),
    };
    let queue_scope = ExecutionQueueScope {
        organization_id: repository.organization_id.clone(),
        workspace_id: repository.workspace_id.clone(),
        project_id: repository.project_id.clone(),
        repository_id: repository.repository_id.clone(),
        product_session_id,
        delivery_id,
    };
    let submitted = storage
        .execution_queue()
        .expect("queue")
        .submit(&ExecutionJobSubmission {
            scope: queue_scope.clone(),
            job_id: job.job_id.clone(),
            request_id: RequestId(canonical_id("req", seed + 10_000)),
            payload_digest: job.payload_digest.clone(),
            dispatch_payload: serde_json::to_vec(job).expect("job JSON"),
            attempt: 1,
            dependencies: Vec::new(),
            work_run_id,
            submitted_at: Instant("2027-01-15T08:00:00.100Z".to_owned()),
        })
        .expect("queue submit");
    storage
        .execution_queue()
        .expect("queue")
        .transition(&ExecutionJobTransitionRequest {
            scope: queue_scope,
            job_id: job.job_id.clone(),
            request_id: RequestId(canonical_id("req", seed + 10_001)),
            expected_revision: submitted.job.revision,
            from: ExecutionJobState::Queued,
            to: ExecutionJobState::Leased,
            occurred_at: claim.issued_at.clone(),
        })
        .expect("queue lease");
}

fn worker_register() -> WorkerRegisterMessage {
    let ExecutionPortMessage::WorkerRegisterMessage(message) = execution_message("worker.register")
    else {
        panic!("worker.register variant");
    };
    message
}

fn execution_message(kind: &str) -> ExecutionPortMessage {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .expect("ExecutionPort fixture");
    fixture["messages"]
        .as_array()
        .expect("fixture messages")
        .iter()
        .find(|message| message["kind"] == kind)
        .cloned()
        .map_or_else(|| panic!("{kind} fixture"), from_value)
        .unwrap_or_else(|error| panic!("{kind} decode: {error}"))
}

struct RecordingPublisher;

impl EventPublisher for RecordingPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Ok(())
    }
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
        *self.publication.lock().expect("journal lock") = Some(publication);
        Ok(())
    }
}

fn initial_workrun_delivery(seed: u64) -> Delivery {
    let mut value: Value = serde_json::from_slice(include_bytes!(
        "../../winwincode-delivery/tests/fixtures/delivery-main.json"
    ))
    .expect("delivery fixture");
    let delivery_id = canonical_id("dlv", seed);
    value["id"] = delivery_id.clone().into();
    value["spec"]["deliveryId"] = delivery_id.into();
    value["revision"] = 1.into();
    value["status"] = "draft".into();
    value["updatedAtMillis"] = value["createdAtMillis"].clone();
    for field in [
        "tasks",
        "stageRuns",
        "sessionBindings",
        "attentionItems",
        "evidence",
    ] {
        value[field] = serde_json::json!([]);
    }
    value["verdict"] = Value::Null;
    value["workRunAggregate"]["items"] = serde_json::json!([]);
    value["workRunAggregate"]["runs"] = serde_json::json!([]);
    Delivery::decode_json(&serde_json::to_vec(&value).expect("delivery JSON"))
        .expect("initial Delivery")
}

fn git_command(root: &Path, args: &[&str]) -> Vec<u8> {
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
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn git_repository(root: &Path) -> String {
    fs::create_dir_all(root).expect("repository root");
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["init", "-q", "-b", "main"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("git init")
            .success()
    );
    fs::write(root.join("source.txt"), b"base\n").expect("base source");
    git_command(root, &["add", "--", "source.txt"]);
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["commit", "-q", "-m", "base"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "WinWinCode Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@winwincode.invalid")
        .env("GIT_COMMITTER_NAME", "WinWinCode Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@winwincode.invalid")
        .status()
        .expect("git commit");
    assert!(status.success());
    String::from_utf8(git_command(root, &["rev-parse", "HEAD"]))
        .expect("git UTF-8")
        .trim()
        .to_owned()
}

fn public_delivery_before_advance(seed: u64, base_revision: String) -> Delivery {
    let mut snapshot = Delivery::decode_json(include_bytes!(
        "../../winwincode-delivery/tests/fixtures/delivery-main.json"
    ))
    .expect("canonical fixture")
    .into_snapshot();
    let delivery_id = DeliveryId(canonical_id("dlv", seed));
    snapshot.id = delivery_id.clone();
    snapshot.spec.delivery_id = delivery_id.clone();
    snapshot.spec.repository.locator = "project-one".into();
    snapshot.spec.base_revision = base_revision;
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
    snapshot.work_run_aggregate.contract.id = WorkContractId(canonical_id("wct", seed));
    snapshot.work_run_aggregate.contract.revision = Revision(1);
    snapshot.work_run_aggregate.items.truncate(1);
    let item = &mut snapshot.work_run_aggregate.items[0];
    item.id = WorkItemId(canonical_id("wit", seed));
    item.work_contract_id = snapshot.work_run_aggregate.contract.id.clone();
    item.work_contract_revision = Revision(1);
    item.revision = Revision(1);
    item.state = WorkItemState::Ready;
    item.depends_on.clear();
    snapshot.work_run_aggregate.runs.clear();
    snapshot.stage_runs.clear();
    snapshot.session_bindings.clear();
    snapshot.attention_items.clear();
    snapshot.evidence.clear();
    snapshot.verdict = None;
    snapshot.updated_at_millis = snapshot.created_at_millis;
    Delivery::try_from_snapshot(snapshot).expect("public Delivery before advance")
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicDeliveryCatalogEntry<'a> {
    schema_version: u8,
    repository_scope: &'a RepositoryScope,
    delivery_id: &'a DeliveryId,
}

fn seed_public_delivery(root: &Path, delivery: &Delivery) {
    let journal = CapturingJournal::default();
    DeliveryStore::borrowed(&journal)
        .execute(DeliveryCommand::SeedForTest(CreateDelivery {
            request_id: RequestId(canonical_id("req", 80_000)),
            request_digest: "b".repeat(64),
            snapshot: delivery.clone(),
        }))
        .expect("seed Delivery journal");
    let AtomicPublication::Create {
        delivery_id,
        manifest,
        first_record,
    } = journal
        .publication
        .into_inner()
        .expect("journal lock")
        .expect("journal publication")
    else {
        panic!("seed create")
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
                winwincode_control_plane::command_receipt_identity(
                    &Actor::UserActor(UserActor {
                        id: UserId(canonical_id("usr", 80_001)),
                        kind: winwincode_domain::UserActorKind::User,
                    }),
                    &Scope::RepositoryScope(repository_scope(80_000)),
                    RequestId(canonical_id("req", 80_001)),
                )
                .expect("seed identity"),
                Sha256Digest(format!("sha256:{}", "b".repeat(64))),
                format!("delivery:{}", delivery.id().0),
                0,
                delivery.encode_json().expect("Delivery JSON"),
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
        .expect("seed event");
    Box::new(storage).close().expect("seed storage close");
}

fn seed_public_catalog(root: &Path, scope: &RepositoryScope, delivery: &Delivery) {
    let payload = serde_json::to_vec(&PublicDeliveryCatalogEntry {
        schema_version: 1,
        repository_scope: scope,
        delivery_id: delivery.id(),
    })
    .expect("catalog JSON");
    let stream_id = format!(
        "delivery-catalog:{:x}:{}",
        Sha256::digest(serde_json::to_vec(scope).expect("scope JSON")),
        delivery.id().0
    );
    let scope_key = receipt_scope_key(&PublicEventScope::Repository {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
    })
    .expect("catalog scope");
    let actor_key = receipt_actor_key(&PublicEventActor::User {
        id: UserId(canonical_id("usr", 90_000 + delivery.revision())),
    })
    .expect("catalog actor");
    let mut storage = SqliteStorage::open(root).expect("catalog storage");
    let receipt = storage
        .commit(&StateCommit::new(
            winwincode_storage::ReceiptIdentity::new(
                actor_key,
                scope_key,
                RequestId(canonical_id("req", 90_000 + delivery.revision())),
            )
            .expect("catalog identity"),
            Sha256Digest(format!("sha256:{:x}", Sha256::digest(&payload))),
            stream_id,
            0,
            payload,
            vec![NewOutboxEvent::internal(
                format!("catalog-seeded:{}", delivery.id().0),
                "delivery.catalog.seeded",
                b"{}".to_vec(),
            )],
        ))
        .expect("catalog seed");
    storage
        .mark_published(&receipt.events[0].event_id)
        .expect("catalog event");
    Box::new(storage).close().expect("catalog close");
}

fn workrun_job(seed: u64, delivery: &Delivery, scope: &RepositoryScope) -> ExecutionJob {
    let aggregate = &delivery.snapshot().work_run_aggregate;
    let contract = aggregate.contract.clone();
    let item = from_value::<winwincode_domain::WorkItem>(serde_json::json!({
        "schemaVersion":"winwincode/v1", "id":canonical_id("wit", seed), "workContractId":contract.id,
        "workContractRevision":1, "revision":1, "state":"ready", "title":"Ingress item",
        "goal":"Run ingress fixture", "criterionIds":[contract.criteria[0].id], "dependsOn":[]
    })).expect("WorkItem");
    let run_id = WorkRunId(canonical_id("wrn", seed));
    let job_id = ExecutionJobId(canonical_id("job", seed));
    let wr_scope = winwincode_execution_port::generated::WorkRunExecutionScope {
        attempt: 1,
        kind: winwincode_execution_port::generated::WorkRunExecutionScopeKind::WorkRun,
        product_session_id: ProductSessionId(canonical_id("psn", seed)),
        rework_authorization: None,
        work_contract_id: contract.id.clone(),
        work_contract_revision: Revision(1),
        work_item_id: item.id.clone(),
        work_item_revision: Revision(1),
        work_run_id: run_id,
    };
    ExecutionJob {
        attempt: 1,
        execution_profile: "executor".into(),
        goal: item.goal.clone(),
        job_id,
        limits: ExecutionLimits {
            deadline_at: Instant("2027-01-15T09:00:00.000Z".into()),
            max_artifact_bytes: 10_000_000,
            max_runtime_seconds: 3600,
        },
        payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        scope: ExecutionScope::WorkRunExecutionScope(wr_scope),
        work_input: Some(winwincode_execution_port::generated::WorkRunInput {
            delivery_spec_id: "spec-fixture".into(),
            delivery_spec_revision: Revision(2),
            candidate_ref: None,
            schema_version: SchemaVersion::WinwincodeV1,
            work_contract: contract,
            work_item: item,
        }),
        workspace: ExecutionWorkspace {
            checkout_revision: "fixture-checkout".into(),
            repository_id: scope.repository_id.clone(),
            write_mode: ExecutionWorkspaceWriteMode::Candidate,
        },
    }
}

fn seed_workrun_delivery(fixture: &mut Fixture, delivery: &Delivery, job: &ExecutionJob) {
    let mut snapshot = delivery.clone().into_snapshot();
    snapshot.work_run_aggregate.items.push(
        job.work_input
            .as_ref()
            .expect("WorkRun input")
            .work_item
            .clone(),
    );
    let seeded = Delivery::try_from_snapshot(snapshot).expect("Delivery with WorkItem");
    let journal = CapturingJournal::default();
    let mutation = DeliveryStore::borrowed(&journal)
        .execute(DeliveryCommand::SeedForTest(CreateDelivery {
            request_id: RequestId(canonical_id("req", 70_000)),
            request_digest: "b".repeat(64),
            snapshot: seeded.clone(),
        }))
        .expect("seed Delivery");
    assert!(mutation.snapshot == seeded);
    let AtomicPublication::Create {
        delivery_id,
        manifest,
        first_record,
    } = journal
        .publication
        .into_inner()
        .expect("journal lock")
        .expect("journal publication")
    else {
        panic!("Delivery seed publication");
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
    let actor = Actor::UserActor(UserActor {
        id: UserId(canonical_id("usr", 70_001)),
        kind: winwincode_domain::UserActorKind::User,
    });
    let identity = winwincode_control_plane::command_receipt_identity(
        &actor,
        &Scope::RepositoryScope(fixture.scope.clone()),
        RequestId(canonical_id("req", 70_001)),
    )
    .expect("receipt identity");
    fixture
        .storage
        .commit(
            &StateCommit::new(
                identity,
                Sha256Digest(format!("sha256:{}", "b".repeat(64))),
                format!("delivery:{}", seeded.id().0),
                0,
                seeded.encode_json().expect("Delivery JSON"),
                vec![NewOutboxEvent::internal(
                    format!("execution-job:{}", job.job_id.0),
                    "execution.job.dispatch",
                    serde_json::to_vec(job).expect("job JSON"),
                )],
            )
            .with_journal_publication(publication),
        )
        .expect("seed Delivery/job");
}

struct Fixture {
    root: PathBuf,
    control_plane: ControlPlane,
    storage: SqliteStorage,
    scope: RepositoryScope,
}

fn public_advance_command(seed: u64) -> CommandEnvelope {
    CommandEnvelope {
        actor: Actor::UserActor(UserActor {
            id: UserId(canonical_id("usr", seed)),
            kind: winwincode_domain::UserActorKind::User,
        }),
        command: CommandName::DeliveryAdvance,
        expected_revision: Revision(1),
        payload: serde_json::json!({"deliveryId": canonical_id("dlv", seed), "dispatchProfile": "executor"}),
        request_id: RequestId(canonical_id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::RepositoryScope(repository_scope(seed)),
    }
}

fn public_execution_fixture(
    seed: u64,
    name: &str,
) -> (PathBuf, PathBuf, ControlPlane, Delivery, ExecutionJob) {
    let root = temporary_directory(name);
    let repository = root.join("repositories/project-one");
    let base_revision = git_repository(&repository);
    let delivery = public_delivery_before_advance(seed, base_revision);
    let scope = repository_scope(seed);
    seed_public_delivery(&root, &delivery);
    seed_public_catalog(&root, &scope, &delivery);
    let mut control_plane = ControlPlane::start_local_with_delivery_adapters(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
        winwincode_control_plane::LocalDeliveryAdapterConfig::new(&repository, scope.clone()),
    )
    .expect("public Delivery adapters");
    let command: DeliveryAdvanceCommand =
        from_value(serde_json::to_value(public_advance_command(seed)).expect("advance envelope"))
            .expect("generated delivery.advance command");
    control_plane
        .delivery_advance(&command)
        .expect("public delivery.advance");
    let advanced = control_plane
        .load_state(&format!("delivery:{}", delivery.id().0))
        .expect("advanced Delivery state")
        .expect("advanced Delivery");
    let advanced_json: Value = serde_json::from_slice(&advanced.payload).expect("advanced JSON");
    assert!(
        advanced_json["workRunAggregate"]["runs"]
            .as_array()
            .expect("advanced runs")
            .is_empty()
    );
    assert!(
        advanced_json["sessionBindings"]
            .as_array()
            .expect("advanced bindings")
            .is_empty()
    );
    let job = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("queue database")
        .query_row(
            "SELECT dispatch_payload FROM scheduler_execution_jobs ORDER BY rowid DESC LIMIT 1",
            [],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .map(|payload| serde_json::from_slice(&payload).expect("public queued job"))
        .expect("public queued job row");
    (root, repository, control_plane, delivery, job)
}

fn public_scheduler_scope(seed: u64) -> RepositorySchedulerScope {
    let scope = repository_scope(seed);
    RepositorySchedulerScope {
        organization_id: scope.organization_id,
        workspace_id: scope.workspace_id,
        project_id: scope.project_id,
        repository_id: scope.repository_id,
    }
}

fn public_attempt_time(second: u64) -> Instant {
    Instant(format!("2027-01-15T08:00:{second:02}.000Z"))
}

fn public_admission(
    storage: &mut SqliteStorage,
    scope: &RepositoryScope,
    job: &ExecutionJob,
    delivery_id: &DeliveryId,
    seed: u64,
) {
    let ExecutionScope::WorkRunExecutionScope(run) = &job.scope else {
        panic!("public job WorkRun scope")
    };
    let queue_scope = ExecutionQueueScope {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
        product_session_id: run.product_session_id.clone(),
        delivery_id: Some(delivery_id.clone()),
    };
    let pool = WorkerPoolId(canonical_id("wpl", seed));
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 2,
        max_queued: 2,
        token_budget: 10_000,
        cost_budget_microunits: 100_000,
        max_runtime_millis: 60_000,
    };
    let boundaries = [
        ExecutionAdmissionBoundary::Organization {
            organization_id: scope.organization_id.clone(),
        },
        ExecutionAdmissionBoundary::Project {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
        },
        ExecutionAdmissionBoundary::Repository {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
        },
        ExecutionAdmissionBoundary::ProductSession {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            product_session_id: run.product_session_id.clone(),
        },
        ExecutionAdmissionBoundary::Delivery {
            organization_id: scope.organization_id.clone(),
            delivery_id: delivery_id.clone(),
        },
        ExecutionAdmissionBoundary::WorkerPool {
            organization_id: scope.organization_id.clone(),
            worker_pool_id: pool.clone(),
        },
    ];
    let mut admission = storage.execution_admission().expect("public admission");
    for boundary in boundaries {
        admission
            .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
            .expect("public admission policy");
    }
    admission
        .reserve(&ExecutionReservationRequest {
            scope: queue_scope.clone(),
            user_id: UserId(canonical_id("usr", seed)),
            worker_pool_id: pool.clone(),
            job_id: job.job_id.clone(),
            request_id: RequestId(canonical_id("req", seed + 200)),
            repository_access: ExecutionRepositoryAccess::IsolatedWrite {
                worktree_key: job.job_id.0.clone(),
            },
            reserved_tokens: 100,
            reserved_cost_microunits: 100,
            runtime_limit_millis: 30_000,
            submitted_at: public_attempt_time(10),
        })
        .expect("public admission reserve");
    admission
        .start(&ExecutionReservationStart {
            scope: queue_scope,
            worker_pool_id: pool,
            job_id: job.job_id.clone(),
            request_id: RequestId(canonical_id("req", seed + 201)),
            expected_revision: 1,
            started_at: public_attempt_time(11),
        })
        .expect("public admission start");
}

#[test]
#[allow(clippy::too_many_lines)]
fn public_git_generated_advance_dispatch_binding_terminal_and_restart_replay() {
    let seed = 390;
    let (root, repository, mut control_plane, delivery, job) =
        public_execution_fixture(seed, "public-git-ingress");
    let scope = repository_scope(seed);
    let mut storage = SqliteStorage::open(&root).expect("public ingress storage");

    let register = worker_register();
    DurableExecutionPortIngress::new(
        &mut control_plane,
        &mut storage,
        &scope,
        register.sent_at.clone(),
    )
    .expect("worker ingress")
    .handle(&ExecutionPortMessage::WorkerRegisterMessage(
        register.clone(),
    ))
    .expect("public worker registration");
    let dispatch = RepositoryExecutionScheduler::new(&mut storage)
        .claim_next(&RepositorySchedulerClaimRequest {
            scope: public_scheduler_scope(seed),
            request_id: RequestId(canonical_id("req", seed + 1)),
            scheduler_generation: "public".into(),
            worker_id: register.worker_id.clone(),
            worker_instance_id: register.worker_instance_id.clone(),
            issued_at: Instant("2027-01-15T08:00:00.200Z".into()),
            expires_at: Instant("2027-01-15T08:05:00.000Z".into()),
        })
        .expect("public scheduler claim")
        .expect("public dispatch");
    assert_eq!(dispatch.job.job_id, job.job_id);
    let accepted = JobDispatchResultMessage {
        error: None,
        job_id: dispatch.job.job_id.clone(),
        kind: JobDispatchResultMessageKind::JobDispatchResult,
        lease: dispatch.lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 2)),
        payload_digest: dispatch.job.payload_digest.clone(),
        request_id: RequestId(canonical_id("req", seed + 2)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: public_attempt_time(21),
        status: JobDispatchResultMessageStatus::Accepted,
        worker_session_id: Some(WorkerSessionId(canonical_id("wsn", seed))),
    };
    let accepted_output = DurableExecutionPortIngress::new(
        &mut control_plane,
        &mut storage,
        &scope,
        public_attempt_time(21),
    )
    .expect("accepted ingress")
    .handle(&ExecutionPortMessage::JobDispatchResultMessage(accepted))
    .expect("public accepted dispatch");
    assert!(
        accepted_output.is_empty(),
        "accepted dispatch is durably acknowledged by the ingress boundary"
    );
    let accepted_state = control_plane
        .load_state(&format!("delivery:{}", delivery.id().0))
        .expect("accepted Delivery state")
        .expect("accepted Delivery");
    let accepted_json: Value =
        serde_json::from_slice(&accepted_state.payload).expect("accepted JSON");
    assert_eq!(
        accepted_json["workRunAggregate"]["runs"]
            .as_array()
            .expect("accepted runs")
            .len(),
        1
    );
    assert_eq!(
        accepted_json["sessionBindings"]
            .as_array()
            .expect("accepted bindings")
            .len(),
        1
    );
    let accepted_worker_session = &accepted_json["sessionBindings"][0]["workerSessionId"];
    assert!(
        accepted_worker_session.is_null()
            || *accepted_worker_session == binding_worker_session_value(seed)
    );
    let binding = workrun_binding(&dispatch.job, &dispatch.lease, seed, seed);
    let binding_output = DurableExecutionPortIngress::new(
        &mut control_plane,
        &mut storage,
        &scope,
        public_attempt_time(22),
    )
    .expect("binding ingress")
    .handle(&ExecutionPortMessage::SessionBindingMessage(
        binding.clone(),
    ))
    .expect("public session binding");
    assert!(
        binding_output.is_empty(),
        "binding is acknowledged by the durable commit"
    );
    let bound_state = control_plane
        .load_state(&format!("delivery:{}", delivery.id().0))
        .expect("bound Delivery state")
        .expect("bound Delivery");
    let bound_json: Value = serde_json::from_slice(&bound_state.payload).expect("bound JSON");
    assert_eq!(
        bound_json["sessionBindings"][0]["workerSessionId"],
        binding.worker_session_id.0
    );
    assert_eq!(
        bound_json["sessionBindings"][0]["codexThreadId"],
        binding.codex_thread_id.0
    );

    let ExecutionPortMessage::WorkerHeartbeatMessage(mut heartbeat) =
        execution_message("worker.heartbeat")
    else {
        panic!("heartbeat variant")
    };
    heartbeat.worker_id = register.worker_id.clone();
    heartbeat.worker_instance_id = register.worker_instance_id.clone();
    heartbeat.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 3));
    heartbeat.sent_at = public_attempt_time(23);
    heartbeat.observed_at = heartbeat.sent_at.clone();
    heartbeat.active_leases.clear();
    heartbeat.capacity = WorkerCapacity {
        running_jobs: 0,
        available_slots: 4,
    };
    DurableExecutionPortIngress::new(
        &mut control_plane,
        &mut storage,
        &scope,
        public_attempt_time(23),
    )
    .expect("heartbeat ingress")
    .handle(&ExecutionPortMessage::WorkerHeartbeatMessage(heartbeat))
    .expect("public heartbeat");

    let delivery_id = delivery.id().clone();
    public_admission(&mut storage, &scope, &dispatch.job, &delivery_id, seed);
    let slot_authority = WorkerSlotAuthority {
        worker_id: dispatch.lease.worker_id.clone(),
        worker_instance_id: dispatch.lease.worker_instance_id.clone(),
        worker_session_id: binding.worker_session_id.clone(),
        codex_thread_id: binding.codex_thread_id.clone(),
        job_id: dispatch.job.job_id.clone(),
        lease_id: dispatch.lease.lease_id.clone(),
        attempt: u64::try_from(dispatch.lease.attempt).expect("attempt"),
        fencing_token: dispatch.lease.fencing_token.clone(),
    };
    let mut slots = storage.worker_session_slots().expect("public slots");
    slots
        .configure_resources(
            &slot_authority.worker_id,
            &slot_authority.worker_instance_id,
            WorkerSlotResourceLimits {
                max_memory_bytes: 100,
                max_disk_bytes: 100,
                max_processes: 1,
            },
        )
        .expect("slot resources");
    slots
        .open(&WorkerSlotOpenRequest {
            authority: slot_authority,
            resources: WorkerSlotResources {
                memory_bytes: 10,
                disk_bytes: 10,
                process_slots: 1,
            },
            request_id: RequestId(canonical_id("req", seed + 4)),
            opened_at: public_attempt_time(24),
        })
        .expect("public worker slot");
    let outcome = terminal_workrun_outcome(
        &dispatch.job,
        dispatch.lease.clone(),
        &binding.worker_session_id,
        seed,
    );
    let first = DurableExecutionPortIngress::new(
        &mut control_plane,
        &mut storage,
        &scope,
        public_attempt_time(25),
    )
    .expect("terminal ingress")
    .handle(&ExecutionPortMessage::JobOutcomeMessage(outcome.clone()))
    .expect("public terminal outcome");
    assert_eq!(
        outcome_ack_status(first),
        winwincode_execution_port::generated::JobOutcomeAckMessageStatus::Accepted
    );
    let queue_scope = match &dispatch.job.scope {
        ExecutionScope::WorkRunExecutionScope(run) => ExecutionQueueScope {
            organization_id: scope.organization_id.clone(),
            workspace_id: scope.workspace_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
            product_session_id: run.product_session_id.clone(),
            delivery_id: Some(delivery_id.clone()),
        },
        ExecutionScope::ProductSessionExecutionScope(_) => panic!("public job WorkRun scope"),
    };
    let failed_job = storage
        .execution_queue()
        .expect("public queue")
        .load_job(&queue_scope, &job.job_id)
        .expect("public queue load")
        .expect("public queue job");
    assert_eq!(failed_job.state, ExecutionJobState::Failed);
    let reservation = storage
        .execution_admission()
        .expect("public admission")
        .load_reservation_by_job(&job.job_id)
        .expect("public reservation")
        .expect("public reservation record");
    assert_eq!(
        reservation.state,
        winwincode_storage::ExecutionReservationState::Released
    );
    let slot_state: String = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("slot inspection")
        .query_row(
            "SELECT state FROM worker_session_slots WHERE worker_session_id = ?1",
            [&binding.worker_session_id.0],
            |row| row.get(0),
        )
        .expect("slot state");
    assert_eq!(slot_state, "failed");
    assert_eq!(lease_terminal_count(&root, &job.job_id), 1);
    Box::new(storage).close().expect("public storage close");
    control_plane.shutdown().expect("public CP shutdown");

    let mut restarted = ControlPlane::start_local_with_delivery_adapters(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
        winwincode_control_plane::LocalDeliveryAdapterConfig::new(&repository, scope.clone()),
    )
    .expect("public CP restart");
    let mut storage = SqliteStorage::open(&root).expect("public restart storage");
    let replay = DurableExecutionPortIngress::new(
        &mut restarted,
        &mut storage,
        &scope,
        public_attempt_time(26),
    )
    .expect("restart ingress")
    .handle(&ExecutionPortMessage::JobOutcomeMessage(outcome))
    .expect("public terminal replay");
    assert_eq!(
        outcome_ack_status(replay),
        winwincode_execution_port::generated::JobOutcomeAckMessageStatus::Duplicate
    );
    let replayed_job = storage
        .execution_queue()
        .expect("replay queue")
        .load_job(&queue_scope, &job.job_id)
        .expect("replay queue load")
        .expect("replay queue job");
    assert_eq!(replayed_job.state, ExecutionJobState::Failed);
    assert_eq!(lease_terminal_count(&root, &job.job_id), 1);
    let replayed_reservation = storage
        .execution_admission()
        .expect("replay admission")
        .load_reservation_by_job(&job.job_id)
        .expect("replay reservation")
        .expect("replay reservation record");
    assert_eq!(
        replayed_reservation.state,
        winwincode_storage::ExecutionReservationState::Released
    );
    Box::new(storage).close().expect("public restart close");
    restarted.shutdown().expect("public restart shutdown");
    fs::remove_dir_all(root).expect("public fixture cleanup");
}

fn binding_worker_session_value(seed: u64) -> Value {
    Value::String(canonical_id("wsn", seed))
}

impl Fixture {
    fn open(name: &str, seed: u64) -> Self {
        let root = temporary_directory(name);
        let control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane start");
        let storage = SqliteStorage::open(&root).expect("Registry storage");
        Self {
            root,
            control_plane,
            storage,
            scope: repository_scope(seed),
        }
    }

    fn close(self) {
        Box::new(self.storage).close().expect("Registry close");
        self.control_plane.shutdown().expect("Control Plane close");
        fs::remove_dir_all(self.root).expect("fixture release");
    }

    fn restart(self) -> Self {
        let Self {
            root,
            control_plane,
            storage,
            scope,
        } = self;
        Box::new(storage).close().expect("Registry restart close");
        control_plane
            .shutdown()
            .expect("Control Plane restart close");
        let control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane restart");
        let storage = SqliteStorage::open(&root).expect("Registry restart");
        Self {
            root,
            control_plane,
            storage,
            scope,
        }
    }

    fn accept(
        &mut self,
        message: &ExecutionPortMessage,
        now: Instant,
    ) -> Result<Vec<ExecutionPortMessage>, DurableExecutionPortError> {
        DurableExecutionPortIngress::new(
            &mut self.control_plane,
            &mut self.storage,
            &self.scope,
            now,
        )?
        .handle(message)
    }

    fn accept_with_delegate(
        &mut self,
        message: &ExecutionPortMessage,
        now: Instant,
        delegate: &mut dyn DurableExecutionPortDelegate,
    ) -> Result<Vec<ExecutionPortMessage>, DurableExecutionPortError> {
        DurableExecutionPortIngress::with_delegate(
            &mut self.control_plane,
            &mut self.storage,
            &self.scope,
            now,
            delegate,
        )?
        .handle(message)
    }
}

struct ProductDispatch {
    job: ExecutionJob,
    lease: ExecutionLeaseStamp,
    product_session_id: ProductSessionId,
    worker_session_id: WorkerSessionId,
}

#[allow(
    clippy::too_many_lines,
    reason = "the production ingress fixture keeps the durable Job, Registry claim, and accepted dispatch visibly exact"
)]
fn install_product_dispatch(fixture: &mut Fixture, seed: u64) -> ProductDispatch {
    let register = worker_register();
    fixture
        .accept(
            &ExecutionPortMessage::WorkerRegisterMessage(register.clone()),
            register.sent_at.clone(),
        )
        .expect("Worker registration");
    let product_session_id = ProductSessionId(canonical_id("psn", seed));
    let job = ExecutionJob {
        attempt: 1,
        execution_profile: "codex".to_owned(),
        goal: "Advance ProductSession chat".to_owned(),
        job_id: ExecutionJobId(canonical_id("job", seed)),
        limits: ExecutionLimits {
            deadline_at: Instant("2027-01-15T09:00:00.000Z".to_owned()),
            max_artifact_bytes: 10_000_000,
            max_runtime_seconds: 3_600,
        },
        payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        scope: ExecutionScope::ProductSessionExecutionScope(ProductSessionExecutionScope {
            kind: ProductSessionExecutionScopeKind::ProductSession,
            product_session_id: product_session_id.clone(),
        }),
        work_input: None,
        workspace: ExecutionWorkspace {
            checkout_revision: "fixture-checkout".to_owned(),
            repository_id: fixture.scope.repository_id.clone(),
            write_mode: ExecutionWorkspaceWriteMode::Candidate,
        },
    };
    fixture
        .control_plane
        .commit(
            &CommandEnvelope {
                actor: Actor::UserActor(UserActor {
                    id: UserId(canonical_id("usr", seed)),
                    kind: winwincode_domain::UserActorKind::User,
                }),
                command: CommandName::SessionCancel,
                expected_revision: Revision(0),
                payload: serde_json::json!({"productSessionId": product_session_id}),
                request_id: RequestId(canonical_id("req", seed)),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: Scope::RepositoryScope(fixture.scope.clone()),
            },
            StateChange::new(
                product_session_catalog_stream_id(&fixture.scope),
                b"product-session-state".to_vec(),
                vec![NewOutboxEvent::internal(
                    format!("execution-job:{}", job.job_id.0),
                    "execution.job.dispatch",
                    serde_json::to_vec(&job).expect("job JSON"),
                )],
            ),
        )
        .expect("durable ProductSession job");
    let claim = ExecutionLeaseClaim {
        expires_at: Instant("2027-01-15T08:05:00.000Z".to_owned()),
        fencing_token: FencingToken("7".to_owned()),
        issued_at: Instant("2027-01-15T08:00:00.200Z".to_owned()),
        job_id: job.job_id.clone(),
        lease_id: LeaseId(canonical_id("lse", seed)),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 1)),
        payload_digest: job.payload_digest.clone(),
        request_id: RequestId(canonical_id("req", seed + 1)),
        worker_id: register.worker_id,
        worker_instance_id: register.worker_instance_id,
        attempt: 1,
    };
    seed_queue_lease(
        &mut fixture.storage,
        &fixture.scope,
        &job,
        &claim,
        seed,
        None,
    );
    fixture
        .storage
        .execution_registry()
        .expect("registry")
        .claim_execution_job(&claim)
        .expect("lease claim");
    let worker_session_id = WorkerSessionId(canonical_id("wsn", seed));
    let lease = ExecutionLeaseStamp {
        attempt: 1,
        expires_at: claim.expires_at,
        fencing_token: claim.fencing_token,
        issued_at: claim.issued_at,
        job_id: claim.job_id,
        lease_id: claim.lease_id,
        worker_id: claim.worker_id,
        worker_instance_id: claim.worker_instance_id,
    };
    let result = JobDispatchResultMessage {
        error: None,
        job_id: job.job_id.clone(),
        kind: JobDispatchResultMessageKind::JobDispatchResult,
        lease: lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 2)),
        payload_digest: job.payload_digest.clone(),
        request_id: claim.request_id,
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:01.000Z".to_owned()),
        status: JobDispatchResultMessageStatus::Accepted,
        worker_session_id: Some(worker_session_id.clone()),
    };
    fixture
        .accept(
            &ExecutionPortMessage::JobDispatchResultMessage(result),
            Instant("2027-01-15T08:00:01.100Z".to_owned()),
        )
        .expect("accepted dispatch");
    ProductDispatch {
        job,
        lease,
        product_session_id,
        worker_session_id,
    }
}

fn registration_result(output: &[ExecutionPortMessage]) -> WorkerRegistrationResultMessageStatus {
    let [ExecutionPortMessage::WorkerRegistrationResultMessage(result)] = output else {
        panic!("ingress must return one worker.registration_result");
    };
    result.status.clone()
}

#[derive(Default)]
struct RecordingDelegate {
    seen: Vec<&'static str>,
}

impl DurableExecutionPortDelegate for RecordingDelegate {
    fn accept(
        &mut self,
        mut context: DurableExecutionPortContext<'_>,
        supplement: DurableExecutionPortSupplement<'_>,
    ) -> Result<Vec<ExecutionPortMessage>, DurableExecutionPortError> {
        let control_plane_path = context
            .control_plane()
            .local_database_path()
            .expect("local Control Plane path")
            .to_path_buf();
        let storage_path = context.storage().database_path().to_path_buf();
        assert_eq!(control_plane_path, storage_path);
        assert_eq!(
            context.repository_scope().kind,
            RepositoryScopeKind::Repository
        );
        assert_eq!(context.server_time().0, "2027-01-15T08:00:02.100Z");
        match supplement {
            DurableExecutionPortSupplement::ProductSessionBinding {
                job,
                dispatch,
                message,
            } => {
                assert_eq!(job.job_id, message.lease.job_id);
                assert_eq!(dispatch.lease().job_id, job.job_id);
                self.seen.push("product-binding");
            }
            DurableExecutionPortSupplement::ProductSessionOutcome {
                job,
                dispatch,
                message,
            } => {
                assert_eq!(job.job_id, message.lease.job_id);
                assert_eq!(dispatch.lease().job_id, job.job_id);
                self.seen.push("product-outcome");
            }
            DurableExecutionPortSupplement::JobScopedWorkerMessage { dispatch, message } => {
                assert_eq!(dispatch.lease().job_id, delegated_job_id(message));
                self.seen.push(delegated_kind(message));
            }
            DurableExecutionPortSupplement::WorkerMessage(_) => self.seen.push("worker"),
        }
        Ok(Vec::new())
    }
}

fn delegated_job_id(message: &ExecutionPortMessage) -> ExecutionJobId {
    match message {
        ExecutionPortMessage::ModelOpenMessage(message) => message.lease.job_id.clone(),
        ExecutionPortMessage::JobCancelAckMessage(message) => message.lease.job_id.clone(),
        ExecutionPortMessage::ActionEnforcementRequestMessage(message) => {
            message.lease.job_id.clone()
        }
        _ => panic!("unexpected delegated message"),
    }
}

fn delegated_kind(message: &ExecutionPortMessage) -> &'static str {
    match message {
        ExecutionPortMessage::ModelOpenMessage(_) => "model.open",
        ExecutionPortMessage::JobCancelAckMessage(_) => "job.cancel_ack",
        ExecutionPortMessage::ActionEnforcementRequestMessage(_) => "action.enforcement_request",
        _ => panic!("unexpected delegated message"),
    }
}

fn exact_session_identity(dispatch: &ProductDispatch) -> SessionIdentity {
    SessionIdentity {
        codex_thread_id: CodexThreadId(canonical_id("cdx", 88)),
        product_session_id: dispatch.product_session_id.clone(),
        work_run_id: None,
        worker_session_id: dispatch.worker_session_id.clone(),
    }
}

fn align_delegated_message(
    message: &mut ExecutionPortMessage,
    dispatch: &ProductDispatch,
    sequence: u64,
) {
    let sent_at = Instant("2027-01-15T08:00:02.100Z".to_owned());
    let message_id = ExecutionMessageId(canonical_id("xmsg", 100 + sequence));
    let identity = exact_session_identity(dispatch);
    match message {
        ExecutionPortMessage::ModelOpenMessage(message) => {
            message.lease.clone_from(&dispatch.lease);
            message
                .worker_session_id
                .clone_from(&dispatch.worker_session_id);
            message.session_identity.clone_from(&identity);
            message.sent_at = sent_at;
            message.message_id = message_id;
        }
        ExecutionPortMessage::JobCancelAckMessage(message) => {
            message.lease.clone_from(&dispatch.lease);
            message
                .worker_session_id
                .clone_from(&dispatch.worker_session_id);
            message.session_identity.clone_from(&identity);
            message.sent_at = sent_at;
            message.message_id = message_id;
        }
        ExecutionPortMessage::ActionEnforcementRequestMessage(message) => {
            message.job_id.clone_from(&dispatch.job.job_id);
            message.lease.clone_from(&dispatch.lease);
            message
                .worker_session_id
                .clone_from(&dispatch.worker_session_id);
            message.session_identity.clone_from(&identity);
            message.sent_at = sent_at;
            message.message_id = message_id;
        }
        ExecutionPortMessage::JobOutcomeMessage(message) => {
            message.lease.clone_from(&dispatch.lease);
            message
                .worker_session_id
                .clone_from(&dispatch.worker_session_id);
            message.session_identity = identity;
            message.sent_at = sent_at;
            message.message_id = message_id;
        }
        _ => panic!("unexpected aligned message"),
    }
}

#[test]
fn product_terminal_model_cancel_and_action_share_one_sealed_delegate_core() {
    let mut fixture = Fixture::open("closed-delegate", 88);
    let dispatch = install_product_dispatch(&mut fixture, 88);
    let mut delegate = RecordingDelegate::default();
    for (sequence, kind) in [
        "job.outcome",
        "model.open",
        "job.cancel_ack",
        "action.enforcement_request",
    ]
    .into_iter()
    .enumerate()
    {
        let mut message = execution_message(kind);
        align_delegated_message(
            &mut message,
            &dispatch,
            u64::try_from(sequence).expect("sequence"),
        );
        assert!(
            fixture
                .accept_with_delegate(
                    &message,
                    Instant("2027-01-15T08:00:02.100Z".to_owned()),
                    &mut delegate,
                )
                .expect("sealed delegate")
                .is_empty()
        );
    }
    assert_eq!(
        delegate.seen,
        [
            "product-outcome",
            "model.open",
            "job.cancel_ack",
            "action.enforcement_request"
        ]
    );

    let mut stale = execution_message("model.open");
    align_delegated_message(&mut stale, &dispatch, 9);
    let ExecutionPortMessage::ModelOpenMessage(stale) = &mut stale else {
        panic!("model.open fixture");
    };
    stale.lease.fencing_token = FencingToken("6".to_owned());
    let error = fixture
        .accept_with_delegate(
            &ExecutionPortMessage::ModelOpenMessage(stale.clone()),
            Instant("2027-01-15T08:00:02.100Z".to_owned()),
            &mut delegate,
        )
        .expect_err("stale delegate message must fail before its owner");
    assert!(matches!(error, DurableExecutionPortError::Storage(_)));
    assert_eq!(delegate.seen.len(), 4);
    fixture.close();
}

#[test]
fn local_and_remote_adapters_share_the_same_durable_ingress_core() {
    let message = worker_register();
    let frame = TypedFrame::new(
        FrameDirection::WorkerToControlPlane,
        ExecutionPortMessage::WorkerRegisterMessage(message.clone()),
    )
    .expect("Worker frame");
    let encoded = RemoteTransportAdapter::<DurableExecutionPortIngress<'_>>::encode(&frame)
        .expect("remote frame encoding");
    let mut local = Fixture::open("local", 1);
    let mut remote = Fixture::open("remote", 1);

    let local_output = {
        let mut ingress = DurableExecutionPortIngress::new(
            &mut local.control_plane,
            &mut local.storage,
            &local.scope,
            message.sent_at.clone(),
        )
        .expect("local ingress");
        LocalWorkerAdapter::new(&mut ingress, EndpointSide::ControlPlane)
            .accept(&frame)
            .expect("local ingress response")
    };
    let remote_output = {
        let mut ingress = DurableExecutionPortIngress::new(
            &mut remote.control_plane,
            &mut remote.storage,
            &remote.scope,
            message.sent_at.clone(),
        )
        .expect("remote ingress");
        RemoteTransportAdapter::new(&mut ingress, EndpointSide::ControlPlane)
            .accept(&encoded)
            .expect("remote ingress response")
    };

    assert_eq!(local_output, remote_output);
    assert_eq!(
        registration_result(&local_output),
        WorkerRegistrationResultMessageStatus::Accepted
    );
    assert!(
        local
            .storage
            .execution_registry()
            .expect("local registry")
            .load_worker(&message.worker_id)
            .expect("local Worker load")
            .is_some()
    );
    assert!(
        remote
            .storage
            .execution_registry()
            .expect("remote registry")
            .load_worker(&message.worker_id)
            .expect("remote Worker load")
            .is_some()
    );

    local.close();
    remote.close();
}

#[test]
fn ingress_rejects_a_second_database_before_processing_worker_input() {
    let mut canonical = Fixture::open("canonical", 2);
    let foreign_root = temporary_directory("foreign");
    let mut foreign = SqliteStorage::open(&foreign_root).expect("foreign storage");
    let Err(error) = DurableExecutionPortIngress::new(
        &mut canonical.control_plane,
        &mut foreign,
        &canonical.scope,
        worker_register().sent_at,
    ) else {
        panic!("foreign database must be rejected");
    };
    assert!(matches!(error, DurableExecutionPortError::Configuration));

    Box::new(foreign).close().expect("foreign storage close");
    fs::remove_dir_all(foreign_root).expect("foreign fixture release");
    canonical.close();
}

#[test]
fn production_execution_port_contract_has_no_worker_time_authority_fallback() {
    let control_plane = include_str!("../src/lib.rs");
    let service = include_str!("../src/execution_port_service.rs");
    let ingress = include_str!("../src/durable_execution_port.rs");

    assert!(!control_plane.contains("pub fn accept_runtime_event_at"));
    assert!(!control_plane.contains("pub fn commit_delivery_terminal_outcome_at"));
    assert!(!control_plane.contains("pub fn commit_delivery_session_binding_at"));
    assert!(
        !control_plane
            .contains("session_binding_transaction::execute(storage, message, authority)")
    );
    assert!(service.contains("server_time: Instant"));
    assert!(service.contains(
        ".accept_runtime_event(route.scope(), message, route.authority(), &self.server_time)"
    ));
    assert!(
        ingress
            .contains(".commit_delivery_session_binding(binding, &authority, &self.server_time)")
    );
    assert!(ingress.contains("validate_first_seen_dispatch"));
    assert!(ingress.contains("Exact receipt replay must"));
    assert!(ingress.contains("must not be copied into the owner request identity, digest"));
    assert!(ingress.contains("The trusted clock never enters the owner digest"));
    assert!(ingress.contains("Worker-controlled `sentAt` is an audited fact, never authorization"));
}

#[test]
#[allow(clippy::too_many_lines)]
fn accepted_dispatch_seals_product_session_runtime_and_replays_exactly() {
    let seed = 44;
    let mut fixture = Fixture::open("product-runtime", seed);
    let register = worker_register();
    fixture
        .accept(
            &ExecutionPortMessage::WorkerRegisterMessage(register.clone()),
            register.sent_at.clone(),
        )
        .expect("Worker registration");

    let product_session_id = ProductSessionId(canonical_id("psn", seed));
    let job = ExecutionJob {
        attempt: 1,
        execution_profile: "codex".to_owned(),
        goal: "Advance ProductSession chat".to_owned(),
        job_id: ExecutionJobId(canonical_id("job", seed)),
        limits: ExecutionLimits {
            deadline_at: Instant("2027-01-15T09:00:00.000Z".to_owned()),
            max_artifact_bytes: 10_000_000,
            max_runtime_seconds: 3_600,
        },
        payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        scope: ExecutionScope::ProductSessionExecutionScope(ProductSessionExecutionScope {
            kind: ProductSessionExecutionScopeKind::ProductSession,
            product_session_id: product_session_id.clone(),
        }),
        work_input: None,
        workspace: ExecutionWorkspace {
            checkout_revision: "fixture-checkout".to_owned(),
            repository_id: fixture.scope.repository_id.clone(),
            write_mode: ExecutionWorkspaceWriteMode::Candidate,
        },
    };
    fixture
        .control_plane
        .commit(
            &CommandEnvelope {
                actor: Actor::UserActor(UserActor {
                    id: UserId(canonical_id("usr", seed)),
                    kind: winwincode_domain::UserActorKind::User,
                }),
                command: CommandName::SessionCancel,
                expected_revision: Revision(0),
                payload: serde_json::json!({"productSessionId": product_session_id}),
                request_id: RequestId(canonical_id("req", seed)),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: Scope::RepositoryScope(fixture.scope.clone()),
            },
            StateChange::new(
                product_session_catalog_stream_id(&fixture.scope),
                b"product-session-state".to_vec(),
                vec![NewOutboxEvent::internal(
                    format!("execution-job:{}", job.job_id.0),
                    "execution.job.dispatch",
                    serde_json::to_vec(&job).expect("job JSON"),
                )],
            ),
        )
        .expect("durable ProductSession job");

    let lease = ExecutionLeaseClaim {
        expires_at: Instant("2027-01-15T08:05:00.000Z".to_owned()),
        fencing_token: FencingToken("7".to_owned()),
        issued_at: Instant("2027-01-15T08:00:00.200Z".to_owned()),
        job_id: job.job_id.clone(),
        lease_id: LeaseId(canonical_id("lse", seed)),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 1)),
        payload_digest: job.payload_digest.clone(),
        request_id: RequestId(canonical_id("req", seed + 1)),
        worker_id: register.worker_id.clone(),
        worker_instance_id: register.worker_instance_id.clone(),
        attempt: 1,
    };
    seed_queue_lease(
        &mut fixture.storage,
        &fixture.scope,
        &job,
        &lease,
        seed,
        None,
    );
    fixture
        .storage
        .execution_registry()
        .expect("registry")
        .claim_execution_job(&lease)
        .expect("lease claim");
    let worker_session_id = WorkerSessionId(canonical_id("wsn", seed));
    let lease_stamp = ExecutionLeaseStamp {
        attempt: 1,
        expires_at: lease.expires_at.clone(),
        fencing_token: lease.fencing_token.clone(),
        issued_at: lease.issued_at.clone(),
        job_id: lease.job_id.clone(),
        lease_id: lease.lease_id.clone(),
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
    };
    let dispatch_result = JobDispatchResultMessage {
        error: None,
        job_id: job.job_id.clone(),
        kind: JobDispatchResultMessageKind::JobDispatchResult,
        lease: lease_stamp.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 2)),
        payload_digest: job.payload_digest.clone(),
        request_id: lease.request_id.clone(),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:01.000Z".to_owned()),
        status: JobDispatchResultMessageStatus::Accepted,
        worker_session_id: Some(worker_session_id.clone()),
    };
    assert!(
        fixture
            .accept(
                &ExecutionPortMessage::JobDispatchResultMessage(dispatch_result),
                Instant("2027-01-15T08:00:01.100Z".to_owned()),
            )
            .expect("dispatch-result ingress")
            .is_empty()
    );

    let codex_thread_id = CodexThreadId(canonical_id("cdx", seed));
    let runtime = RuntimeEventMessage {
        codex_thread_id: codex_thread_id.clone(),
        event: ExecutionEventRecord {
            category: ExecutionEventCategory::Lifecycle,
            event_id: ExecutionEventId(canonical_id("xevt", seed)),
            occurred_at: Instant("2027-01-15T08:00:02.000Z".to_owned()),
            payload: None,
            sequence: ExecutionSequence(1),
            summary: "ProductSession Worker started".to_owned(),
        },
        kind: RuntimeEventMessageKind::RuntimeEvent,
        lease: lease_stamp,
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 3)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:02.100Z".to_owned()),
        session_identity: SessionIdentity {
            codex_thread_id: codex_thread_id.clone(),
            product_session_id,
            work_run_id: None,
            worker_session_id: worker_session_id.clone(),
        },
        worker_session_id,
    };
    let first = fixture
        .accept(
            &ExecutionPortMessage::RuntimeEventMessage(runtime.clone()),
            runtime.sent_at.clone(),
        )
        .expect("runtime ingress");
    let [ExecutionPortMessage::RuntimeAckMessage(first)] = first.as_slice() else {
        panic!("runtime.ack response");
    };
    assert_eq!(
        first.status,
        LeaseWriteStatus::Accepted,
        "{:?}",
        first.error
    );

    let replay = fixture
        .accept(
            &ExecutionPortMessage::RuntimeEventMessage(runtime.clone()),
            runtime.sent_at.clone(),
        )
        .expect("runtime replay");
    let [ExecutionPortMessage::RuntimeAckMessage(replay)] = replay.as_slice() else {
        panic!("runtime replay ack");
    };
    assert_eq!(replay.status, LeaseWriteStatus::Duplicate);

    let expired_time = Instant("2027-01-15T08:06:00.000Z".to_owned());
    let expired_replay = fixture
        .accept(
            &ExecutionPortMessage::RuntimeEventMessage(runtime.clone()),
            expired_time.clone(),
        )
        .expect("expired exact runtime replay");
    let [ExecutionPortMessage::RuntimeAckMessage(expired_replay)] = expired_replay.as_slice()
    else {
        panic!("expired runtime replay ack");
    };
    assert_eq!(expired_replay.status, LeaseWriteStatus::Duplicate);

    let mut changed = runtime.clone();
    changed.event.summary = "changed replay body".to_owned();
    let changed = fixture
        .accept(
            &ExecutionPortMessage::RuntimeEventMessage(changed),
            expired_time.clone(),
        )
        .expect("changed runtime replay rejection");
    let [ExecutionPortMessage::RuntimeAckMessage(changed)] = changed.as_slice() else {
        panic!("changed runtime replay ack");
    };
    assert_eq!(changed.status, LeaseWriteStatus::RejectedConflict);

    let mut forged = runtime.clone();
    forged.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 30));
    forged.event.event_id = ExecutionEventId(canonical_id("xevt", seed + 30));
    forged.event.sequence = ExecutionSequence(2);
    let forged = fixture
        .accept(
            &ExecutionPortMessage::RuntimeEventMessage(forged),
            expired_time,
        )
        .expect("fresh expired runtime rejection");
    let [ExecutionPortMessage::RuntimeAckMessage(forged)] = forged.as_slice() else {
        panic!("fresh expired runtime ack");
    };
    assert_eq!(forged.status, LeaseWriteStatus::RejectedExpiredLease);

    let mut premature = runtime.clone();
    premature.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 31));
    premature.event.event_id = ExecutionEventId(canonical_id("xevt", seed + 31));
    premature.event.sequence = ExecutionSequence(2);
    let premature = fixture
        .accept(
            &ExecutionPortMessage::RuntimeEventMessage(premature),
            Instant("2027-01-15T07:59:59.000Z".to_owned()),
        )
        .expect("premature runtime rejection");
    let [ExecutionPortMessage::RuntimeAckMessage(premature)] = premature.as_slice() else {
        panic!("premature runtime ack");
    };
    assert_eq!(premature.status, LeaseWriteStatus::RejectedConflict);

    let mut stale = runtime;
    stale.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 4));
    stale.event.event_id = ExecutionEventId(canonical_id("xevt", seed + 4));
    stale.event.sequence = ExecutionSequence(2);
    stale.lease.fencing_token = FencingToken("6".to_owned());
    let rejected = fixture
        .accept(
            &ExecutionPortMessage::RuntimeEventMessage(stale.clone()),
            stale.sent_at,
        )
        .expect("stale runtime rejection");
    let [ExecutionPortMessage::RuntimeAckMessage(rejected)] = rejected.as_slice() else {
        panic!("stale runtime ack");
    };
    assert_eq!(rejected.status, LeaseWriteStatus::RejectedStaleFencingToken);
    fixture.close();
}

fn workrun_binding(
    job: &ExecutionJob,
    lease: &ExecutionLeaseStamp,
    seed: u64,
    session: u64,
) -> SessionBindingMessage {
    let ExecutionScope::WorkRunExecutionScope(scope) = &job.scope else {
        panic!("WorkRun scope")
    };
    let worker_session_id = WorkerSessionId(canonical_id("wsn", session));
    let codex_thread_id = CodexThreadId(canonical_id("cdx", session));
    let binding_time = if lease.attempt == 1 {
        "2027-01-15T08:00:02.000Z"
    } else {
        "2027-01-15T08:00:06.000Z"
    };
    let identity = SessionIdentity {
        codex_thread_id: codex_thread_id.clone(),
        product_session_id: scope.product_session_id.clone(),
        worker_session_id: worker_session_id.clone(),
        work_run_id: Some(scope.work_run_id.clone()),
    };
    SessionBindingMessage {
        attempt: lease.attempt,
        bound_at: Instant(binding_time.into()),
        codex_thread_id: codex_thread_id.clone(),
        fencing_token: lease.fencing_token.clone(),
        kind: SessionBindingMessageKind::SessionBinding,
        lease: lease.clone(),
        lease_id: lease.lease_id.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 10)),
        product_session_id: scope.product_session_id.clone(),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant(binding_time.into()),
        session_identity: identity,
        source_identity: SessionBindingSourceIdentity {
            kind: SessionBindingSourceIdentityKind::ExecutionWorker,
            lease_id: lease.lease_id.clone(),
            worker_id: lease.worker_id.clone(),
            worker_instance_id: lease.worker_instance_id.clone(),
            worker_session_id: worker_session_id.clone(),
        },
        worker_id: lease.worker_id.clone(),
        worker_session_id,
        work_run_id: Some(scope.work_run_id.clone()),
    }
}

fn prepare_workrun_admission(
    fixture: &mut Fixture,
    job: &ExecutionJob,
    delivery_id: Option<DeliveryId>,
    seed: u64,
) -> WorkerPoolId {
    let pool = WorkerPoolId(canonical_id("wpl", seed));
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 4,
        max_queued: 4,
        token_budget: 100_000,
        cost_budget_microunits: 100_000,
        max_runtime_millis: 60_000,
    };
    let boundaries = [
        ExecutionAdmissionBoundary::Organization {
            organization_id: fixture.scope.organization_id.clone(),
        },
        ExecutionAdmissionBoundary::Project {
            organization_id: fixture.scope.organization_id.clone(),
            project_id: fixture.scope.project_id.clone(),
        },
        ExecutionAdmissionBoundary::Repository {
            organization_id: fixture.scope.organization_id.clone(),
            project_id: fixture.scope.project_id.clone(),
            repository_id: fixture.scope.repository_id.clone(),
        },
        ExecutionAdmissionBoundary::Delivery {
            organization_id: fixture.scope.organization_id.clone(),
            delivery_id: DeliveryId(canonical_id("dlv", seed)),
        },
        ExecutionAdmissionBoundary::ProductSession {
            organization_id: fixture.scope.organization_id.clone(),
            project_id: fixture.scope.project_id.clone(),
            product_session_id: ProductSessionId(canonical_id("psn", seed)),
        },
        ExecutionAdmissionBoundary::WorkerPool {
            organization_id: fixture.scope.organization_id.clone(),
            worker_pool_id: pool.clone(),
        },
    ];
    let mut admission = fixture.storage.execution_admission().expect("admission");
    for boundary in boundaries {
        admission
            .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
            .expect("admission policy");
    }
    admission
        .reserve(&ExecutionReservationRequest {
            scope: ExecutionQueueScope {
                organization_id: fixture.scope.organization_id.clone(),
                workspace_id: fixture.scope.workspace_id.clone(),
                project_id: fixture.scope.project_id.clone(),
                repository_id: fixture.scope.repository_id.clone(),
                product_session_id: ProductSessionId(canonical_id("psn", seed)),
                delivery_id: delivery_id.clone(),
            },
            user_id: UserId(canonical_id("usr", seed)),
            worker_pool_id: pool.clone(),
            job_id: job.job_id.clone(),
            request_id: RequestId(canonical_id("req", seed * 100 + 200)),
            repository_access: ExecutionRepositoryAccess::ReadOnly,
            reserved_tokens: 100,
            reserved_cost_microunits: 100,
            runtime_limit_millis: 30_000,
            submitted_at: Instant("2027-01-15T08:00:00.050Z".into()),
        })
        .expect("admission reserve");
    admission
        .start(&ExecutionReservationStart {
            scope: ExecutionQueueScope {
                organization_id: fixture.scope.organization_id.clone(),
                workspace_id: fixture.scope.workspace_id.clone(),
                project_id: fixture.scope.project_id.clone(),
                repository_id: fixture.scope.repository_id.clone(),
                product_session_id: ProductSessionId(canonical_id("psn", seed)),
                delivery_id,
            },
            worker_pool_id: pool.clone(),
            job_id: job.job_id.clone(),
            request_id: RequestId(canonical_id("req", seed * 100 + 201)),
            expected_revision: 1,
            started_at: Instant("2027-01-15T08:00:00.100Z".into()),
        })
        .expect("admission start");
    pool
}

fn install_workrun_dispatch_for_terminal(
    fixture: &mut Fixture,
    seed: u64,
) -> (Delivery, ExecutionJob, ExecutionLeaseStamp, WorkerSessionId) {
    let delivery = initial_workrun_delivery(seed);
    let job = workrun_job(seed, &delivery, &fixture.scope);
    seed_workrun_delivery(fixture, &delivery, &job);
    let register = worker_register();
    fixture
        .accept(
            &ExecutionPortMessage::WorkerRegisterMessage(register.clone()),
            register.sent_at.clone(),
        )
        .expect("register");
    let claim = ExecutionLeaseClaim {
        expires_at: Instant("2027-01-15T08:05:00.000Z".into()),
        fencing_token: FencingToken("1".into()),
        issued_at: Instant("2027-01-15T08:00:00.200Z".into()),
        job_id: job.job_id.clone(),
        lease_id: LeaseId(canonical_id("lse", seed)),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 1)),
        payload_digest: job.payload_digest.clone(),
        request_id: RequestId(canonical_id("req", seed + 1)),
        worker_id: register.worker_id.clone(),
        worker_instance_id: register.worker_instance_id.clone(),
        attempt: 1,
    };
    seed_queue_lease(
        &mut fixture.storage,
        &fixture.scope,
        &job,
        &claim,
        seed,
        Some(DeliveryId(canonical_id("dlv", seed))),
    );
    fixture
        .storage
        .execution_registry()
        .expect("registry")
        .claim_execution_job(&claim)
        .expect("claim");
    let worker_session_id = WorkerSessionId(canonical_id("wsn", seed));
    let lease = ExecutionLeaseStamp {
        attempt: 1,
        expires_at: claim.expires_at.clone(),
        fencing_token: claim.fencing_token.clone(),
        issued_at: claim.issued_at.clone(),
        job_id: claim.job_id.clone(),
        lease_id: claim.lease_id.clone(),
        worker_id: claim.worker_id.clone(),
        worker_instance_id: claim.worker_instance_id.clone(),
    };
    fixture
        .accept(
            &ExecutionPortMessage::JobDispatchResultMessage(JobDispatchResultMessage {
                error: None,
                job_id: job.job_id.clone(),
                kind: JobDispatchResultMessageKind::JobDispatchResult,
                lease: lease.clone(),
                message_id: ExecutionMessageId(canonical_id("xmsg", seed + 2)),
                payload_digest: job.payload_digest.clone(),
                request_id: claim.request_id,
                schema_version: SchemaVersion::WinwincodeV1,
                sent_at: Instant("2027-01-15T08:00:01.000Z".into()),
                status: JobDispatchResultMessageStatus::Accepted,
                worker_session_id: Some(worker_session_id.clone()),
            }),
            Instant("2027-01-15T08:00:01.100Z".into()),
        )
        .expect("accepted dispatch");
    (delivery, job, lease, worker_session_id)
}

fn terminal_workrun_outcome(
    job: &ExecutionJob,
    lease: ExecutionLeaseStamp,
    worker_session_id: &WorkerSessionId,
    seed: u64,
) -> winwincode_execution_port::generated::JobOutcomeMessage {
    let ExecutionPortMessage::JobOutcomeMessage(mut message) = execution_message("job.outcome")
    else {
        panic!("job.outcome variant");
    };
    let thread_id = CodexThreadId(canonical_id("cdx", seed));
    let product_session_id = match &job.scope {
        ExecutionScope::WorkRunExecutionScope(scope) => scope.product_session_id.clone(),
        ExecutionScope::ProductSessionExecutionScope(scope) => scope.product_session_id.clone(),
    };
    let work_run_id = match &job.scope {
        ExecutionScope::WorkRunExecutionScope(scope) => scope.work_run_id.clone(),
        ExecutionScope::ProductSessionExecutionScope(_) => {
            panic!("terminal fixture requires WorkRun")
        }
    };
    message.lease = lease;
    message.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 3));
    message.sent_at = Instant("2027-01-15T08:01:00.100Z".into());
    message.worker_session_id = worker_session_id.clone();
    message.outcome.status = ExecutionOutcomeStatus::Failed;
    message.outcome.summary = "Verifier failed".into();
    message.outcome.codex_thread_id = Some(thread_id.clone());
    message.outcome.finished_at = Instant("2027-01-15T08:01:00.000Z".into());
    message.session_identity = SessionIdentity {
        codex_thread_id: thread_id,
        product_session_id,
        worker_session_id: worker_session_id.clone(),
        work_run_id: Some(work_run_id),
    };
    message
}

fn outcome_ack_status(
    output: Vec<ExecutionPortMessage>,
) -> winwincode_execution_port::generated::JobOutcomeAckMessageStatus {
    let mut output = output.into_iter();
    let Some(ExecutionPortMessage::JobOutcomeAckMessage(ack)) = output.next() else {
        panic!("job.outcome_ack response");
    };
    assert!(output.next().is_none(), "single job.outcome_ack response");
    ack.status
}

fn lease_terminal_count(root: &Path, job_id: &ExecutionJobId) -> i64 {
    let connection =
        rusqlite::Connection::open(root.join("control-plane.sqlite3")).expect("Registry inspect");
    connection
        .query_row(
            "SELECT COUNT(*) FROM execution_lease_terminals WHERE job_id = ?1",
            [&job_id.0],
            |row| row.get(0),
        )
        .expect("Registry terminal count")
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the fault-injected restart test keeps Delivery, Registry, and replay assertions together"
)]
fn committed_delivery_outcome_replay_finishes_registry_once_after_restart_gap() {
    let seed = 397;
    let mut fixture = Fixture::open("terminal-registry-gap", seed);
    let (delivery, job, lease, worker_session_id) =
        install_workrun_dispatch_for_terminal(&mut fixture, seed);
    let binding = workrun_binding(&job, &lease, seed, seed);
    fixture
        .accept(
            &ExecutionPortMessage::SessionBindingMessage(binding),
            Instant("2027-01-15T08:00:02.100Z".into()),
        )
        .expect("session binding");
    let ExecutionPortMessage::WorkerHeartbeatMessage(mut heartbeat) =
        execution_message("worker.heartbeat")
    else {
        panic!("worker.heartbeat variant")
    };
    heartbeat.worker_id = lease.worker_id.clone();
    heartbeat.worker_instance_id = lease.worker_instance_id.clone();
    heartbeat.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 20));
    heartbeat.sent_at = Instant("2027-01-15T08:00:01.200Z".into());
    heartbeat.observed_at = heartbeat.sent_at.clone();
    heartbeat.capacity = WorkerCapacity {
        running_jobs: 0,
        available_slots: 4,
    };
    heartbeat.active_leases.clear();
    fixture
        .accept(
            &ExecutionPortMessage::WorkerHeartbeatMessage(heartbeat),
            Instant("2027-01-15T08:00:01.200Z".into()),
        )
        .expect("worker heartbeat");
    let pool = prepare_workrun_admission(
        &mut fixture,
        &job,
        Some(DeliveryId(canonical_id("dlv", seed))),
        seed,
    );
    let slot_authority = WorkerSlotAuthority {
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
        worker_session_id: worker_session_id.clone(),
        codex_thread_id: CodexThreadId(canonical_id("cdx", seed)),
        job_id: lease.job_id.clone(),
        lease_id: lease.lease_id.clone(),
        attempt: u64::try_from(lease.attempt).expect("attempt"),
        fencing_token: lease.fencing_token.clone(),
    };
    fixture
        .storage
        .worker_session_slots()
        .expect("slots")
        .configure_resources(
            &slot_authority.worker_id,
            &slot_authority.worker_instance_id,
            WorkerSlotResourceLimits {
                max_memory_bytes: 1_000_000,
                max_disk_bytes: 1_000_000,
                max_processes: 4,
            },
        )
        .expect("slot resources");
    fixture
        .storage
        .worker_session_slots()
        .expect("slots")
        .open(&WorkerSlotOpenRequest {
            authority: slot_authority,
            resources: WorkerSlotResources {
                memory_bytes: 100,
                disk_bytes: 100,
                process_slots: 1,
            },
            request_id: RequestId(canonical_id("req", seed + 21)),
            opened_at: Instant("2027-01-15T08:00:01.300Z".into()),
        })
        .expect("slot open");
    assert_eq!(pool, WorkerPoolId(canonical_id("wpl", seed)));
    let outcome = terminal_workrun_outcome(&job, lease, &worker_session_id, seed);

    let connection = rusqlite::Connection::open(fixture.root.join("control-plane.sqlite3"))
        .expect("Registry failure injector");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_registry_terminal_after_delivery_commit
             BEFORE INSERT ON execution_lease_terminals
             BEGIN SELECT RAISE(ABORT, 'injected Registry terminal failure'); END;",
        )
        .expect("install Registry failure");
    drop(connection);
    let first_error = fixture
        .accept(
            &ExecutionPortMessage::JobOutcomeMessage(outcome.clone()),
            outcome.sent_at.clone(),
        )
        .expect_err("Registry settlement fails after Delivery commit");
    assert!(matches!(first_error, DurableExecutionPortError::Storage(_)));
    let committed = fixture
        .control_plane
        .load_state(&format!("delivery:{}", delivery.id().0))
        .expect("committed Delivery read")
        .expect("committed Delivery state");
    assert_eq!(committed.revision, 5);
    assert_eq!(lease_terminal_count(&fixture.root, &job.job_id), 0);

    let connection = rusqlite::Connection::open(fixture.root.join("control-plane.sqlite3"))
        .expect("Registry failure removal");
    connection
        .execute_batch("DROP TRIGGER fail_registry_terminal_after_delivery_commit;")
        .expect("remove Registry failure");
    drop(connection);
    fixture = fixture.restart();
    assert_eq!(
        outcome_ack_status(
            fixture
                .accept(
                    &ExecutionPortMessage::JobOutcomeMessage(outcome.clone()),
                    Instant("2027-01-15T08:06:00.000Z".into()),
                )
                .expect("exact terminal recovery replay"),
        ),
        winwincode_execution_port::generated::JobOutcomeAckMessageStatus::Duplicate
    );
    assert_eq!(lease_terminal_count(&fixture.root, &job.job_id), 1);

    let mut changed = outcome.clone();
    changed.outcome.summary = "changed replay body".into();
    assert_eq!(
        outcome_ack_status(
            fixture
                .accept(
                    &ExecutionPortMessage::JobOutcomeMessage(changed),
                    Instant("2027-01-15T08:06:00.000Z".into()),
                )
                .expect("changed terminal replay rejection"),
        ),
        winwincode_execution_port::generated::JobOutcomeAckMessageStatus::RejectedConflict
    );
    assert_eq!(lease_terminal_count(&fixture.root, &job.job_id), 1);

    let mut forged = outcome.clone();
    forged.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 30));
    assert_eq!(
        outcome_ack_status(
            fixture
                .accept(
                    &ExecutionPortMessage::JobOutcomeMessage(forged),
                    Instant("2027-01-15T08:06:00.000Z".into()),
                )
                .expect("fresh terminal conflict"),
        ),
        winwincode_execution_port::generated::JobOutcomeAckMessageStatus::RejectedConflict
    );
    assert_eq!(lease_terminal_count(&fixture.root, &job.job_id), 1);
    let mut premature = outcome;
    premature.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 31));
    assert_eq!(
        outcome_ack_status(
            fixture
                .accept(
                    &ExecutionPortMessage::JobOutcomeMessage(premature),
                    Instant("2027-01-15T07:59:59.000Z".into()),
                )
                .expect("premature terminal conflict"),
        ),
        winwincode_execution_port::generated::JobOutcomeAckMessageStatus::RejectedConflict
    );
    assert_eq!(lease_terminal_count(&fixture.root, &job.job_id), 1);
    fixture.close();
}

#[test]
fn workrun_accepted_dispatch_appends_real_delivery_and_exact_duplicate_replays() {
    let seed = 301;
    let mut fixture = Fixture::open("workrun-append", seed);
    let (delivery, job, lease, worker_session_id) =
        install_workrun_dispatch_for_terminal(&mut fixture, seed);
    let result = JobDispatchResultMessage {
        error: None,
        job_id: job.job_id.clone(),
        kind: JobDispatchResultMessageKind::JobDispatchResult,
        lease: lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 2)),
        payload_digest: job.payload_digest.clone(),
        request_id: RequestId(canonical_id("req", seed + 1)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:01.000Z".into()),
        status: JobDispatchResultMessageStatus::Accepted,
        worker_session_id: Some(worker_session_id),
    };
    let state = fixture
        .control_plane
        .load_state(&format!("delivery:{}", delivery.id().0))
        .expect("state")
        .expect("delivery");
    let value: Value = serde_json::from_slice(&state.payload).expect("delivery JSON");
    assert_eq!(value["revision"], 2);
    assert_eq!(
        value["workRunAggregate"]["runs"]
            .as_array()
            .expect("runs")
            .len(),
        1
    );
    fixture = fixture.restart();
    fixture
        .accept(
            &ExecutionPortMessage::JobDispatchResultMessage(result),
            Instant("2027-01-15T08:00:01.200Z".into()),
        )
        .expect("restart duplicate dispatch");
    let state = fixture
        .control_plane
        .load_state(&format!("delivery:{}", delivery.id().0))
        .expect("state")
        .expect("delivery");
    let value: Value = serde_json::from_slice(&state.payload).expect("delivery JSON");
    assert_eq!(value["revision"], 2);
    assert_eq!(
        value["workRunAggregate"]["runs"]
            .as_array()
            .expect("runs")
            .len(),
        1
    );
    fixture.close();
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the replacement path keeps scheduler, lease, binding, and replay assertions together"
)]
fn replacement_seal_skips_append_and_binds_successor_worker_session() {
    let seed = 302;
    let mut fixture = Fixture::open("workrun-replacement", seed);
    let delivery = initial_workrun_delivery(seed);
    let job = workrun_job(seed, &delivery, &fixture.scope);
    seed_workrun_delivery(&mut fixture, &delivery, &job);
    let register = worker_register();
    fixture
        .accept(
            &ExecutionPortMessage::WorkerRegisterMessage(register.clone()),
            register.sent_at.clone(),
        )
        .expect("register");
    let claim = ExecutionLeaseClaim {
        expires_at: Instant("2027-01-15T08:05:00.000Z".into()),
        fencing_token: FencingToken("1".into()),
        issued_at: Instant("2027-01-15T08:00:00.200Z".into()),
        job_id: job.job_id.clone(),
        lease_id: LeaseId(canonical_id("lse", seed)),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 1)),
        payload_digest: job.payload_digest.clone(),
        request_id: RequestId(canonical_id("req", seed + 1)),
        worker_id: register.worker_id.clone(),
        worker_instance_id: register.worker_instance_id.clone(),
        attempt: 1,
    };
    seed_queue_lease(
        &mut fixture.storage,
        &fixture.scope,
        &job,
        &claim,
        seed,
        Some(DeliveryId(canonical_id("dlv", seed))),
    );
    fixture
        .storage
        .execution_registry()
        .expect("registry")
        .claim_execution_job(&claim)
        .expect("claim");
    let lease = ExecutionLeaseStamp {
        attempt: 1,
        expires_at: claim.expires_at.clone(),
        fencing_token: claim.fencing_token.clone(),
        issued_at: claim.issued_at.clone(),
        job_id: claim.job_id.clone(),
        lease_id: claim.lease_id.clone(),
        worker_id: claim.worker_id.clone(),
        worker_instance_id: claim.worker_instance_id.clone(),
    };
    let first = JobDispatchResultMessage {
        error: None,
        job_id: job.job_id.clone(),
        kind: JobDispatchResultMessageKind::JobDispatchResult,
        lease: lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 2)),
        payload_digest: job.payload_digest.clone(),
        request_id: claim.request_id.clone(),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:01.000Z".into()),
        status: JobDispatchResultMessageStatus::Accepted,
        worker_session_id: Some(WorkerSessionId(canonical_id("wsn", seed))),
    };
    fixture
        .accept(
            &ExecutionPortMessage::JobDispatchResultMessage(first.clone()),
            Instant("2027-01-15T08:00:01.100Z".into()),
        )
        .expect("initial accepted");
    let binding = workrun_binding(&job, &lease, seed, seed);
    fixture
        .accept(
            &ExecutionPortMessage::SessionBindingMessage(binding.clone()),
            Instant("2027-01-15T08:00:02.100Z".into()),
        )
        .expect("predecessor binding");
    let ExecutionPortMessage::WorkerHeartbeatMessage(mut heartbeat) =
        execution_message("worker.heartbeat")
    else {
        panic!("worker.heartbeat variant")
    };
    heartbeat.worker_id = register.worker_id.clone();
    heartbeat.worker_instance_id = register.worker_instance_id.clone();
    heartbeat.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 30));
    heartbeat.sent_at = Instant("2027-01-15T08:00:02.150Z".into());
    heartbeat.observed_at = heartbeat.sent_at.clone();
    heartbeat.capacity = WorkerCapacity {
        running_jobs: 0,
        available_slots: 4,
    };
    heartbeat.active_leases.clear();
    fixture
        .accept(
            &ExecutionPortMessage::WorkerHeartbeatMessage(heartbeat),
            Instant("2027-01-15T08:00:02.150Z".into()),
        )
        .expect("predecessor heartbeat");
    let _pool = prepare_workrun_admission(
        &mut fixture,
        &job,
        Some(DeliveryId(canonical_id("dlv", seed))),
        seed,
    );
    let old_slot = WorkerSlotAuthority {
        worker_id: claim.worker_id.clone(),
        worker_instance_id: claim.worker_instance_id.clone(),
        worker_session_id: WorkerSessionId(canonical_id("wsn", seed)),
        codex_thread_id: CodexThreadId(canonical_id("cdx", seed)),
        job_id: claim.job_id.clone(),
        lease_id: claim.lease_id.clone(),
        attempt: claim.attempt,
        fencing_token: claim.fencing_token.clone(),
    };
    fixture
        .storage
        .worker_session_slots()
        .expect("slots")
        .configure_resources(
            &old_slot.worker_id,
            &old_slot.worker_instance_id,
            WorkerSlotResourceLimits {
                max_memory_bytes: 100,
                max_disk_bytes: 100,
                max_processes: 4,
            },
        )
        .expect("slot resources");
    fixture
        .storage
        .worker_session_slots()
        .expect("slots")
        .open(&WorkerSlotOpenRequest {
            authority: old_slot,
            resources: WorkerSlotResources {
                memory_bytes: 10,
                disk_bytes: 10,
                process_slots: 1,
            },
            request_id: RequestId(canonical_id("req", seed + 8)),
            opened_at: Instant("2027-01-15T08:00:02.200Z".into()),
        })
        .expect("predecessor slot");
    let queue_scope = ExecutionQueueScope {
        organization_id: fixture.scope.organization_id.clone(),
        workspace_id: fixture.scope.workspace_id.clone(),
        project_id: fixture.scope.project_id.clone(),
        repository_id: fixture.scope.repository_id.clone(),
        product_session_id: ProductSessionId(canonical_id("psn", seed)),
        delivery_id: Some(DeliveryId(canonical_id("dlv", seed))),
    };
    fixture
        .storage
        .execution_queue()
        .expect("queue")
        .transition(&ExecutionJobTransitionRequest {
            scope: queue_scope,
            job_id: job.job_id.clone(),
            request_id: RequestId(canonical_id("req", seed + 3)),
            expected_revision: 3,
            from: ExecutionJobState::Running,
            to: ExecutionJobState::Failed,
            occurred_at: Instant("2027-01-15T08:00:03.100Z".into()),
        })
        .expect("failed predecessor");
    fixture
        .storage
        .execution_registry()
        .expect("registry")
        .finish_execution_lease(&ExecutionLeaseTerminalRequest {
            job_id: claim.job_id.clone(),
            lease_id: claim.lease_id.clone(),
            worker_id: claim.worker_id.clone(),
            worker_instance_id: claim.worker_instance_id.clone(),
            attempt: 1,
            fencing_token: claim.fencing_token.clone(),
            outcome: ExecutionLeaseTerminalOutcome::Failed,
            terminal_at: Instant("2027-01-15T08:00:03.100Z".into()),
            request_id: RequestId(canonical_id("req", seed + 7)),
        })
        .expect("terminal predecessor lease");
    let mut successor_register = worker_register();
    successor_register.worker_id = register.worker_id.clone();
    successor_register.worker_instance_id = WorkerInstanceId(canonical_id("wki", seed + 1));
    successor_register.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 6));
    successor_register.request_id = RequestId(canonical_id("req", seed + 6));
    successor_register.sent_at = Instant("2027-01-15T08:00:03.500Z".into());
    successor_register.started_at = successor_register.sent_at.clone();
    fixture
        .accept(
            &ExecutionPortMessage::WorkerRegisterMessage(successor_register.clone()),
            successor_register.sent_at.clone(),
        )
        .expect("successor register");
    let ExecutionPortMessage::WorkerHeartbeatMessage(mut heartbeat) =
        execution_message("worker.heartbeat")
    else {
        panic!("worker.heartbeat variant")
    };
    heartbeat.worker_id = register.worker_id.clone();
    heartbeat.worker_instance_id = successor_register.worker_instance_id.clone();
    heartbeat.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 30));
    heartbeat.sent_at = Instant("2027-01-15T08:00:03.600Z".into());
    heartbeat.observed_at = heartbeat.sent_at.clone();
    heartbeat.capacity = WorkerCapacity {
        running_jobs: 0,
        available_slots: 4,
    };
    heartbeat.active_leases.clear();
    fixture
        .accept(
            &ExecutionPortMessage::WorkerHeartbeatMessage(heartbeat),
            Instant("2027-01-15T08:00:03.600Z".into()),
        )
        .expect("successor heartbeat");
    let retry = RepositoryExecutionScheduler::new(&mut fixture.storage)
        .retry_failed(&RepositorySchedulerRetryRequest {
            scope: RepositorySchedulerScope {
                organization_id: fixture.scope.organization_id.clone(),
                workspace_id: fixture.scope.workspace_id.clone(),
                project_id: fixture.scope.project_id.clone(),
                repository_id: fixture.scope.repository_id.clone(),
            },
            job_id: job.job_id.clone(),
            request_id: RequestId(canonical_id("req", seed + 4)),
            scheduler_generation: "fixture-retry".into(),
            worker_id: successor_register.worker_id.clone(),
            worker_instance_id: successor_register.worker_instance_id.clone(),
            retryable_failure: true,
            failed_at_tick: 1,
            now_tick: 10,
            policy: SchedulerRetryPolicy {
                max_attempts: 3,
                initial_backoff_ticks: 1,
                max_backoff_ticks: 2,
            },
            issued_at: Instant("2027-01-15T08:00:04.000Z".into()),
            expires_at: Instant("2027-01-15T08:10:00.000Z".into()),
        })
        .expect("retry")
        .expect("replacement dispatch");
    assert_eq!(retry.job.attempt, 2);
    let successor_lease = retry.lease.clone();
    let successor = JobDispatchResultMessage {
        error: None,
        job_id: retry.job.job_id.clone(),
        kind: JobDispatchResultMessageKind::JobDispatchResult,
        lease: ExecutionLeaseStamp {
            attempt: 2,
            expires_at: successor_lease.expires_at.clone(),
            fencing_token: successor_lease.fencing_token.clone(),
            issued_at: successor_lease.issued_at.clone(),
            job_id: successor_lease.job_id.clone(),
            lease_id: successor_lease.lease_id.clone(),
            worker_id: successor_lease.worker_id.clone(),
            worker_instance_id: successor_lease.worker_instance_id.clone(),
        },
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 5)),
        payload_digest: retry.job.payload_digest.clone(),
        request_id: retry.request_id.clone(),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:05.000Z".into()),
        status: JobDispatchResultMessageStatus::Accepted,
        worker_session_id: Some(WorkerSessionId(canonical_id("wsn", seed + 1))),
    };
    fixture
        .accept(
            &ExecutionPortMessage::JobDispatchResultMessage(successor.clone()),
            Instant("2027-01-15T08:00:05.100Z".into()),
        )
        .expect("successor accepted");
    fixture
        .accept(
            &ExecutionPortMessage::JobDispatchResultMessage(successor.clone()),
            Instant("2027-01-15T08:00:05.200Z".into()),
        )
        .expect("duplicate successor accepted");
    let state = fixture
        .control_plane
        .load_state(&format!("delivery:{}", delivery.id().0))
        .expect("state")
        .expect("delivery");
    let value: Value = serde_json::from_slice(&state.payload).expect("Delivery JSON");
    assert_eq!(
        value["workRunAggregate"]["runs"]
            .as_array()
            .expect("runs")
            .len(),
        1,
        "replacement must not ordinary-append"
    );
    fixture
        .accept(
            &ExecutionPortMessage::SessionBindingMessage(binding),
            Instant("2027-01-15T08:00:05.300Z".into()),
        )
        .expect_err("stale predecessor seal");
    let successor_binding = workrun_binding(&retry.job, &successor.lease, seed + 20, seed + 1);
    fixture
        .accept(
            &ExecutionPortMessage::SessionBindingMessage(successor_binding),
            Instant("2027-01-15T08:00:06.100Z".into()),
        )
        .expect("successor binding transaction");
    let state = fixture
        .control_plane
        .load_state(&format!("delivery:{}", delivery.id().0))
        .expect("state")
        .expect("delivery");
    let value: Value = serde_json::from_slice(&state.payload).expect("Delivery JSON");
    assert_eq!(value["revision"], 6);
    assert_eq!(
        value["workRunAggregate"]["runs"]
            .as_array()
            .expect("runs")
            .len(),
        2
    );
    fixture.close();
}
