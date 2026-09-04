// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::large_futures, clippy::too_many_lines)]

use std::{
    fs,
    future::Future,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use sha2::{Digest as _, Sha256};
use winwincode_api::generated::{
    Actor, CommandEnvelope, CommandName, CredentialReferenceCreateCommand,
    CredentialReferenceCreateCommandCommand, CredentialReferenceCreatePayload, ModelRoute,
    OrganizationScope, OrganizationScopeKind, Scope,
};
#[cfg(feature = "test-support")]
use winwincode_codex::CodexCoreAdapter as _;
use winwincode_control_plane::delivery_execution::{
    DeliveryExecutionConfig, DeliveryExecutionPortError, ExecutionJobDispatcher,
    PendingDeliveryExecution, prepare_delivery_advance,
};
use winwincode_control_plane::{
    ControlPlane, ControlPlaneConfig, CreateProductSessionCommand, CredentialReferenceService,
    DurableExecutionPortIngress, EventPublishError, EventPublisher, LocalModelPolicyAuthority,
    LocalModelPolicyAuthorityConfig, LocalSecretStoreAdapter, ModelAdmissionClock,
    ModelAdmissionClockError, ModelAdmissionLimits, ModelAdmissionPolicyLayer, ModelCapability,
    ModelExecutionOpenReceipt, ModelExecutionPortReceipt, ModelRequestPoolConfig,
    ModelRoutePolicyDecision, ModelSettingsRequest, ModelSettingsService, ModelSettingsTarget,
    ModelSettingsValues, ModelToolSupport, OutboxEvent, ProductSessionExecutionConfig,
    ProductSessionService, ProviderAdmissionReservationConfig, ProviderCatalogRequest,
    ProviderCatalogService, ProviderDescriptor, ProviderFinishReason, ProviderGatewayOpenReceipt,
    ProviderStreamConverter, ProviderStreamEvent, ProviderStreamFailure, ProviderStreamFailureKind,
    ProviderTokenUsage, ProviderToolIdentity, ProviderToolKind, ResolvedSecret,
    StandaloneModelExecutionApplication, StandaloneModelExecutionConfig, StandaloneProviderConfig,
    StructuredOutputSupport, SubmitChatMessageCommand, local_loopback_retry_policy,
    product_session_command_context,
};
use winwincode_delivery::{
    application::stage::{AdvanceStageInput, NewStageIdentities, advance},
    domain::{
        DELIVERY_SCHEMA_VERSION, Delivery, DeliveryStage, DeliveryStatus, DeliveryTask,
        DeliveryTaskStatus, SessionBindingId, StageRun, StageRunActorType, StageRunStatus,
    },
    store::{
        AtomicPublication, CreateDelivery, DeliveryCommand, DeliveryCommandPort,
        DeliveryJournalPort, DeliveryStore, JournalBackendError, LoadedDeliveryJournal,
    },
};
use winwincode_domain::{
    ControlPlaneEventId, CredentialReferenceId, DeliveryId, DeliveryTaskId, ExecutionJobId,
    ExecutionMessageId, ExecutionSequence, FencingToken, Instant, InteractiveInputMode,
    InteractiveInputValue, LeaseId, OrganizationId, ProductSessionId, ProjectId, RepositoryId,
    RequestId, Revision, SchemaVersion, Sha256Digest, StageRunId, UserId, WorkerId,
    WorkerInstanceId, WorkspaceId,
};
use winwincode_domain::{RepositoryScope, RepositoryScopeKind, UserActor, UserActorKind};
use winwincode_execution_port::{
    action_enforcement::{ActionEnforcementIssuer, ActionEnforcementSigningKey},
    generated::{
        ActionEnforcementDecision, ActionEnforcementReceiptMessage,
        ActionEnforcementReceiptMessageKind, ApprovalDecisionMessage,
        ApprovalDecisionMessageDecision, ApprovalDecisionMessageKind, ApprovalDecisionMessageScope,
        ArtifactAckMessage, ArtifactAckMessageKind, ArtifactKind, ArtifactReference,
        ChangeBatchProgressState, ChangeBatchReceiptStatus, ExecutionEventCategory, ExecutionJob,
        ExecutionLeaseStamp, ExecutionLimits, ExecutionOutcomeStatus, ExecutionPortErrorCode,
        ExecutionPortMessage, ExecutionScope, ExecutionWorkspace, ExecutionWorkspaceWriteMode,
        InputRequestMessage, InputResponseMessage, InputResponseMessageKind,
        InputResponseMessageStatus, JobCancelMessage, JobCancelMessageKind, JobCancelMessageReason,
        JobDispatchMessage, JobDispatchMessageKind, JobDispatchResultMessageStatus,
        LeaseWriteStatus, ModelChunkMessage, ModelChunkMessageKind, ModelGatewayRoute,
        ModelOpenMessage, RuntimeEventMessage, WorkerCapabilityFeature, WorkerCapabilitySet,
        WorkerCapabilitySetPlatform, WorkerRegistrationResultMessage,
        WorkerRegistrationResultMessageKind, WorkerRegistrationResultMessageLeaseRecovery,
        WorkerRegistrationResultMessageStatus,
    },
    transport::{FrameDirection, TypedFrame},
};
use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord,
    EXECUTION_PROTOCOL_VERSION, ExecutionAdmissionBoundary, ExecutionAdmissionLimits,
    ExecutionAdmissionPolicy, ExecutionJobState, ExecutionJobSubmission,
    ExecutionJobTransitionRequest, ExecutionLeaseClaim, ExecutionQueueScope,
    ExecutionRepositoryAccess, ExecutionReservationRequest, ExecutionReservationStart,
    NewOutboxEvent, ProductStateStorage, StateCommit, WorkerAuthenticationIdentity,
    WorkerHeartbeatRequest, WorkerPlatform, WorkerPoolId, WorkerRegistrationRequest,
    WorkerSlotAuthority, WorkerSlotOpenRequest, WorkerSlotResourceLimits, WorkerSlotResources,
};
use winwincode_storage::{LeaseWriteStatus as StorageLeaseWriteStatus, SqliteStorage};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
const PROVIDER_ID: &str = "winwincode-loopback";
const MODEL_ID: &str = "loopback-model";
const PROVIDER_SECRET: &[u8] = b"native-cutover-provider-secret";
const FIXTURE_CANDIDATE_REF: &str =
    "git-candidate:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn run_on_large_stack(future: impl Future<Output = ()> + Send + 'static) {
    std::thread::Builder::new()
        .name("production-codex-vertical".to_owned())
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build production vertical runtime")
                .block_on(future);
        })
        .expect("spawn production vertical thread")
        .join()
        .expect("production vertical thread");
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "winwincode-native-model-port-{label}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create native ModelPort test directory");
        Self(path)
    }

    fn data(&self) -> PathBuf {
        self.0.join("control-plane")
    }

    fn secrets(&self) -> PathBuf {
        self.0.join("provider-secrets")
    }

    fn worker(&self) -> PathBuf {
        self.0.join("worker")
    }

    fn sources(&self) -> PathBuf {
        self.0.join("sources")
    }

    fn workspaces(&self) -> PathBuf {
        self.0.join("job-workspaces")
    }

    fn source_revision(&self) -> String {
        let repository = self.sources().join(id("rep", 1));
        if !repository.join(".git").is_dir() {
            fs::create_dir_all(&repository).expect("create production source repository");
            git(&repository, &["init", "-q"]);
            git(&repository, &["config", "user.name", "WinWinCode Fixture"]);
            git(
                &repository,
                &["config", "user.email", "fixture@example.invalid"],
            );
            fs::write(repository.join("fixture.txt"), b"source\n")
                .expect("write production source fixture");
            fs::write(repository.join(".gitignore"), b"/target/\n/Cargo.lock\n")
                .expect("write production source ignore rules");
            fs::create_dir_all(repository.join("src")).expect("create source fixture crate");
            fs::write(
                repository.join("Cargo.toml"),
                b"[package]\nname = \"production-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            )
            .expect("write source fixture manifest");
            fs::write(
                repository.join("src/lib.rs"),
                b"pub fn fixture_value() -> u64 { 1 }\n",
            )
            .expect("write source fixture crate");
            git(
                &repository,
                &[
                    "add",
                    ".gitignore",
                    "fixture.txt",
                    "Cargo.toml",
                    "src/lib.rs",
                ],
            );
            git(&repository, &["commit", "-qm", "source"]);
        }
        git_output(&repository, &["rev-parse", "HEAD"])
    }

    fn workspace_runtime(&self) -> winwincode_worker::workspace_runtime::JobWorkspaceRuntime {
        let _ = self.source_revision();
        winwincode_worker::workspace_runtime::JobWorkspaceRuntime::open(
            self.workspaces(),
            self.sources(),
        )
        .expect("open production Job workspace runtime")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn detached_checkout(root: &TestDirectory) -> PathBuf {
    let mut checkouts = fs::read_dir(root.workspaces())
        .expect("read Worker workspace roots")
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("checkout"))
        .filter(|checkout| checkout.is_dir())
        .collect::<Vec<_>>();
    checkouts.sort();
    assert_eq!(checkouts.len(), 1, "one detached checkout per Job");
    checkouts.pop().expect("detached checkout")
}

struct DirectorySnapshot {
    directories: Vec<PathBuf>,
    files: Vec<(PathBuf, Vec<u8>, fs::Permissions)>,
}

impl DirectorySnapshot {
    fn capture(root: &Path) -> Self {
        fn visit(
            root: &Path,
            current: &Path,
            directories: &mut Vec<PathBuf>,
            files: &mut Vec<(PathBuf, Vec<u8>, fs::Permissions)>,
        ) {
            let mut entries = fs::read_dir(current)
                .expect("read crash snapshot directory")
                .collect::<Result<Vec<_>, _>>()
                .expect("collect crash snapshot directory");
            entries.sort_by_key(fs::DirEntry::file_name);
            for entry in entries {
                let path = entry.path();
                let relative = path
                    .strip_prefix(root)
                    .expect("snapshot entry below root")
                    .to_path_buf();
                let file_type = entry.file_type().expect("read snapshot entry type");
                if file_type.is_dir() {
                    directories.push(relative);
                    visit(root, &path, directories, files);
                } else if file_type.is_file() {
                    let permissions = entry
                        .metadata()
                        .expect("read durable crash snapshot permissions")
                        .permissions();
                    files.push((
                        relative,
                        fs::read(&path).expect("read durable crash snapshot file"),
                        permissions,
                    ));
                } else {
                    panic!("durable crash snapshot contains a non-file entry");
                }
            }
        }

        let mut directories = Vec::new();
        let mut files = Vec::new();
        visit(root, root, &mut directories, &mut files);
        Self { directories, files }
    }

    fn restore(&self, root: &Path) {
        let _ = fs::remove_dir_all(root);
        fs::create_dir_all(root).expect("recreate crash snapshot root");
        for directory in &self.directories {
            fs::create_dir_all(root.join(directory)).expect("restore snapshot directory");
        }
        for (path, bytes, permissions) in &self.files {
            let path = root.join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("restore snapshot file parent");
            }
            fs::write(&path, bytes).expect("restore durable crash snapshot file");
            // `fs::write` creates a fresh file with the process umask.  Keep
            // executable sealed helpers executable across a crash snapshot;
            // ProductionCodexAdapter intentionally validates that bit before
            // accepting the helper on restart.
            fs::set_permissions(path, permissions.clone())
                .expect("restore durable crash snapshot permissions");
        }
    }
}

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn at(value: &str) -> Instant {
    Instant(value.to_owned())
}

fn digest(byte: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", byte.to_string().repeat(64)))
}

fn git(repository: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .status()
        .expect("run production fixture Git");
    assert!(status.success(), "production fixture Git command failed");
}

fn git_output(repository: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .output()
        .expect("run production fixture Git output");
    assert!(
        output.status.success(),
        "production fixture Git output command failed"
    );
    String::from_utf8(output.stdout)
        .expect("production fixture Git output is UTF-8")
        .trim()
        .to_owned()
}

fn actor() -> Actor {
    Actor::UserActor(UserActor {
        id: UserId(id("usr", 1)),
        kind: UserActorKind::User,
    })
}

fn organization_scope() -> OrganizationScope {
    OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: OrganizationId(id("org", 1)),
    }
}

fn repository_scope() -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 1)),
        project_id: ProjectId(id("prj", 1)),
        repository_id: RepositoryId(id("rep", 1)),
    }
}

fn configure_provider(storage: &mut SqliteStorage, message: &ModelOpenMessage) {
    ProviderCatalogService::new(storage)
        .upsert(
            &ProviderCatalogRequest {
                actor: actor(),
                scope: Scope::OrganizationScope(organization_scope()),
                request_id: RequestId(id("req", 1)),
                expected_catalog_version: 0,
            },
            &ProviderDescriptor {
                provider_id: PROVIDER_ID.to_owned(),
                display_name: "WinWinCode loopback Provider".to_owned(),
                adapter_kind: "deterministic-loopback".to_owned(),
                credential_reference_id: CredentialReferenceId(id("crd", 1)),
                models: vec![ModelCapability {
                    model_id: MODEL_ID.to_owned(),
                    display_name: "WinWinCode loopback model".to_owned(),
                    context_window_tokens: 128_000,
                    max_output_tokens: 16_000,
                    tool_support: ModelToolSupport::Parallel,
                    structured_output_support: StructuredOutputSupport::JsonSchemaStrict,
                    reasoning_efforts: vec!["high".to_owned()],
                }],
            },
        )
        .expect("register loopback Provider");
    CredentialReferenceService::new(storage)
        .create(
            &CredentialReferenceCreateCommand {
                actor: actor(),
                command: CredentialReferenceCreateCommandCommand::CredentialReferenceCreate,
                expected_revision: Revision(0),
                payload: CredentialReferenceCreatePayload {
                    credential_reference_id: CredentialReferenceId(id("crd", 1)),
                    display_name: "Loopback credential".to_owned(),
                    provider_id: PROVIDER_ID.to_owned(),
                    vault_locator: "local-production://loopback".to_owned(),
                },
                request_id: RequestId(id("req", 2)),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: Scope::OrganizationScope(organization_scope()),
            },
            1_893_456_000_000,
        )
        .expect("create credential reference");
    ModelSettingsService::new(storage)
        .update(
            &ModelSettingsRequest {
                actor: actor(),
                target: ModelSettingsTarget::ProductSession {
                    repository_scope: repository_scope(),
                    product_session_id: message.session_identity.product_session_id.clone(),
                },
                request_id: RequestId(id("req", 3)),
                expected_revision: 0,
            },
            ModelSettingsValues {
                default_model_route: Some(ModelRoute {
                    credential_reference_id: CredentialReferenceId(id("crd", 1)),
                    provider_id: PROVIDER_ID.to_owned(),
                    model_id: MODEL_ID.to_owned(),
                }),
                worker_concurrency_limit: 2,
            },
        )
        .expect("configure ProductSession model route");
}

fn commit_execution_queue(storage: &mut SqliteStorage, job: &ExecutionJob) {
    let queue_scope = execution_scope(job);
    let dispatch_payload = serde_json::to_vec(job).expect("ExecutionJob JSON");
    storage
        .execution_queue()
        .expect("execution queue")
        .submit(&ExecutionJobSubmission {
            scope: queue_scope.clone(),
            job_id: job.job_id.clone(),
            request_id: RequestId(id("req", 11)),
            payload_digest: job.payload_digest.clone(),
            dispatch_payload: dispatch_payload.clone(),
            attempt: u64::try_from(job.attempt).expect("positive Job attempt"),
            dependencies: Vec::new(),
            stage_run_id: match &job.scope {
                ExecutionScope::DeliveryStageExecutionScope(scope) => {
                    Some(scope.stage_run_id.clone())
                }
                ExecutionScope::ProductSessionExecutionScope(_) => None,
            },
            submitted_at: at("2029-12-31T23:59:56.000Z"),
        })
        .expect("submit canonical execution queue Job");
    storage
        .execution_queue()
        .expect("execution queue")
        .transition(&ExecutionJobTransitionRequest {
            scope: queue_scope,
            job_id: job.job_id.clone(),
            request_id: RequestId(id("req", 12)),
            expected_revision: 1,
            from: ExecutionJobState::Queued,
            to: ExecutionJobState::Leased,
            occurred_at: at("2030-01-01T00:00:00.000Z"),
        })
        .expect("lease canonical execution queue Job");
    // The production scheduler commits ProductSession and Delivery dispatch
    // intents atomically with their queue row.  This fixture writes the queue
    // directly, so retain the same immutable internal intent before the
    // embedded Provider runtime authorizes its first ModelOpen.
    let event_id = format!("execution-job:{}", job.job_id.0);
    if storage
        .load_outbox_event(&event_id)
        .expect("load execution intent fixture")
        .is_none()
    {
        storage
            .commit(&StateCommit::new(
                winwincode_control_plane::command_receipt_identity(
                    &actor(),
                    &Scope::RepositoryScope(repository_scope()),
                    RequestId(id("req", 13)),
                )
                .expect("execution intent receipt identity"),
                digest('e'),
                format!("execution-intent:{}", job.job_id.0),
                0,
                b"{}".to_vec(),
                vec![NewOutboxEvent::internal(
                    event_id,
                    "execution.job.dispatch",
                    serde_json::to_vec(job).expect("execution intent payload"),
                )],
            ))
            .expect("commit immutable execution intent fixture");
    }
}

fn lease_product_session_execution_queue(storage: &mut SqliteStorage, job: &ExecutionJob) {
    let queue_scope = execution_scope(job);
    storage
        .execution_queue()
        .expect("execution queue")
        .transition(&ExecutionJobTransitionRequest {
            scope: queue_scope,
            job_id: job.job_id.clone(),
            request_id: RequestId(id("req", 15)),
            expected_revision: 1,
            from: ExecutionJobState::Queued,
            to: ExecutionJobState::Leased,
            occurred_at: at("2030-01-01T00:00:00.000Z"),
        })
        .expect("lease ProductSession execution queue Job");
}

fn seed_delivery_job(root: &TestDirectory, job: &ExecutionJob) {
    let (pending, source) = if job.execution_profile == "planner" {
        (
            pending_planning_execution(&job.workspace.checkout_revision),
            delivery_before_planning(),
        )
    } else {
        (
            pending_delivery_execution(&job.workspace.checkout_revision),
            delivery_before_execution(),
        )
    };
    assert_eq!(
        pending.job(),
        job,
        "fixture Job must come from its Delivery"
    );
    seed_pending_delivery_job(root, &source, &pending);
}

fn seed_pending_delivery_job(
    root: &TestDirectory,
    source: &Delivery,
    pending: &PendingDeliveryExecution,
) {
    let journal = CapturingJournal::default();
    DeliveryStore::borrowed(&journal)
        .execute(DeliveryCommand::SeedForTest(CreateDelivery {
            request_id: RequestId(id("req", 14)),
            request_digest: "c".repeat(64),
            snapshot: source.clone(),
        }))
        .expect("seed Delivery journal");
    let AtomicPublication::Create {
        delivery_id,
        manifest,
        first_record,
    } = journal
        .publication
        .into_inner()
        .expect("Delivery journal lock")
        .expect("Delivery journal publication")
    else {
        panic!("Delivery fixture must create its journal");
    };
    let publication = AggregateJournalPublication::Create {
        key: AggregateJournalKey::new("delivery", delivery_id.0).expect("Delivery journal key"),
        manifest,
        first_record: AggregateJournalRecord::new(
            first_record.sequence,
            first_record.digest,
            first_record.bytes,
        ),
    };
    let identity = winwincode_control_plane::command_receipt_identity(
        &actor(),
        &Scope::RepositoryScope(repository_scope()),
        RequestId(id("req", 14)),
    )
    .expect("source Delivery receipt identity");
    let mut storage = SqliteStorage::open(root.data()).expect("open source Delivery storage");
    storage
        .commit(
            &StateCommit::new(
                identity,
                digest('c'),
                format!("delivery:{}", source.id().0),
                0,
                source.encode_json().expect("source Delivery JSON"),
                vec![NewOutboxEvent::internal(
                    format!("delivery-seeded:{}", source.id().0),
                    "delivery.seeded",
                    source.encode_json().expect("source Delivery event JSON"),
                )],
            )
            .with_journal_publication(publication),
        )
        .expect("commit source Delivery");
    Box::new(storage)
        .close()
        .expect("close source Delivery storage");

    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(root.data()),
        Box::new(DiscardingPublisher),
    )
    .expect("start Delivery execution Control Plane");
    control_plane
        .commit_delivery_execution(
            &CommandEnvelope {
                actor: actor(),
                command: CommandName::DeliveryAdvance,
                expected_revision: Revision(
                    i64::try_from(source.revision()).expect("Delivery revision fits API"),
                ),
                payload: serde_json::json!({"deliveryId": source.id().0}),
                request_id: pending.request_id().clone(),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: Scope::RepositoryScope(repository_scope()),
            },
            pending,
            &mut RecordingDispatcher,
        )
        .expect("commit canonical Delivery and ExecutionJob");
    control_plane
        .shutdown()
        .expect("shutdown Delivery execution Control Plane");
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
        *self.publication.lock().expect("Delivery journal lock") = Some(publication);
        Ok(())
    }
}

struct RecordingDispatcher;

impl ExecutionJobDispatcher for RecordingDispatcher {
    fn dispatch(&mut self, _job: &ExecutionJob) -> Result<(), DeliveryExecutionPortError> {
        Ok(())
    }
}

fn delivery_before_execution() -> Delivery {
    let mut snapshot = Delivery::decode_json(include_bytes!(
        "../../winwincode-delivery/tests/fixtures/delivery-main.json"
    ))
    .expect("canonical Delivery fixture")
    .into_snapshot();
    let delivery_id = DeliveryId(id("dlv", 1));
    snapshot.id = delivery_id.clone();
    snapshot.spec.delivery_id = delivery_id.clone();
    snapshot.revision = 1;
    snapshot.status = DeliveryStatus::Executing;
    snapshot.tasks = vec![DeliveryTask {
        schema_version: DELIVERY_SCHEMA_VERSION,
        id: DeliveryTaskId(id("dtk", 1)),
        delivery_id,
        title: "Run embedded production Codex".to_owned(),
        goal: "Reply with the deterministic loopback result.".to_owned(),
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
    Delivery::try_from_snapshot(snapshot).expect("Delivery before execution")
}

fn delivery_before_verification(role: &str) -> Delivery {
    assert!(matches!(role, "reviewer" | "verifier"));
    let mut snapshot = delivery_before_execution().into_snapshot();
    snapshot.status = DeliveryStatus::Verifying;
    snapshot.tasks[0].status = DeliveryTaskStatus::Verifying;
    let delivery_id = snapshot.id.clone();
    let task_id = snapshot.tasks[0].id.clone();
    snapshot.stage_runs = vec![StageRun {
        schema_version: DELIVERY_SCHEMA_VERSION,
        id: StageRunId("run-production-executor".to_owned()),
        delivery_id: delivery_id.clone(),
        delivery_task_id: Some(task_id.clone()),
        stage: DeliveryStage::Executing,
        actor_type: StageRunActorType::Codex,
        role: "executor".to_owned(),
        status: StageRunStatus::Succeeded,
        attempt: 1,
        started_at_millis: 1_893_455_999_000,
        finished_at_millis: Some(1_893_455_999_100),
    }];
    if role == "verifier" {
        snapshot.stage_runs.push(StageRun {
            schema_version: DELIVERY_SCHEMA_VERSION,
            id: StageRunId("run-production-reviewer".to_owned()),
            delivery_id,
            delivery_task_id: Some(task_id),
            stage: DeliveryStage::Verifying,
            actor_type: StageRunActorType::Codex,
            role: "reviewer".to_owned(),
            status: StageRunStatus::Succeeded,
            attempt: 1,
            started_at_millis: 1_893_455_999_200,
            finished_at_millis: Some(1_893_455_999_300),
        });
    }
    Delivery::try_from_snapshot(snapshot).expect("Delivery before verification")
}

fn delivery_before_planning() -> Delivery {
    let mut snapshot = Delivery::decode_json(include_bytes!(
        "../../winwincode-delivery/tests/fixtures/delivery-main.json"
    ))
    .expect("canonical planning Delivery fixture")
    .into_snapshot();
    let delivery_id = DeliveryId(id("dlv", 1));
    snapshot.id = delivery_id.clone();
    snapshot.spec.delivery_id = delivery_id;
    snapshot.revision = 1;
    snapshot.status = DeliveryStatus::Ready;
    snapshot.tasks.clear();
    snapshot.stage_runs.clear();
    snapshot.session_bindings.clear();
    snapshot.attention_items.clear();
    snapshot.evidence.clear();
    snapshot.verdict = None;
    snapshot.updated_at_millis = snapshot.created_at_millis;
    Delivery::try_from_snapshot(snapshot).expect("Delivery before planning")
}

fn pending_delivery_execution(checkout_revision: &str) -> PendingDeliveryExecution {
    let delivery = delivery_before_execution();
    let advanced = advance(
        &delivery,
        AdvanceStageInput {
            current_lease: None,
            rework_authorization: None,
            expected_revision: 1,
            product_session_id: ProductSessionId(id("psn", 1)),
            identities: NewStageIdentities {
                stage_run_id: StageRunId(id("run", 1)),
                execution_job_id: ExecutionJobId(id("job", 1)),
                session_binding_id: SessionBindingId::new("binding-production-vertical".to_owned())
                    .expect("SessionBinding id"),
                attention_item_id: winwincode_domain::AttentionItemId(id("att", 1)),
            },
            review: None,
            previous_outcome: None,
            now_millis: 1_893_455_999_000,
        },
    )
    .expect("advance production Delivery");
    prepare_delivery_advance(
        RequestId(id("req", 13)),
        advanced,
        DeliveryExecutionConfig {
            payload_digest: digest('b'),
            candidate_ref: None,
            workspace: ExecutionWorkspace {
                checkout_revision: checkout_revision.to_owned(),
                repository_id: RepositoryId(id("rep", 1)),
                write_mode: ExecutionWorkspaceWriteMode::Candidate,
            },
            limits: ExecutionLimits {
                deadline_at: at("2030-01-01T00:04:30.000Z"),
                max_artifact_bytes: 1_000_000,
                max_runtime_seconds: 240,
            },
        },
    )
    .expect("prepare canonical Delivery execution")
}

fn pending_verification_execution(
    checkout_revision: &str,
    role: &str,
) -> (Delivery, PendingDeliveryExecution) {
    let delivery = delivery_before_verification(role);
    let suffix = role;
    let identity_seed = if role == "reviewer" { 21 } else { 22 };
    let advanced = advance(
        &delivery,
        AdvanceStageInput {
            current_lease: None,
            rework_authorization: None,
            expected_revision: delivery.revision(),
            product_session_id: ProductSessionId(id("psn", identity_seed)),
            identities: NewStageIdentities {
                stage_run_id: StageRunId(id("run", identity_seed)),
                execution_job_id: ExecutionJobId(id("job", identity_seed)),
                session_binding_id: SessionBindingId::new(format!("binding-production-{suffix}"))
                    .expect("verification SessionBinding id"),
                attention_item_id: winwincode_domain::AttentionItemId(id("att", identity_seed)),
            },
            review: None,
            previous_outcome: None,
            now_millis: 1_893_455_999_400,
        },
    )
    .expect("advance production verification Delivery");
    let pending = prepare_delivery_advance(
        RequestId(id("req", if role == "reviewer" { 21 } else { 22 })),
        advanced,
        DeliveryExecutionConfig {
            payload_digest: digest('f'),
            candidate_ref: Some(FIXTURE_CANDIDATE_REF.to_owned()),
            workspace: ExecutionWorkspace {
                checkout_revision: checkout_revision.to_owned(),
                repository_id: RepositoryId(id("rep", 1)),
                write_mode: ExecutionWorkspaceWriteMode::ReadOnly,
            },
            limits: ExecutionLimits {
                deadline_at: at("2030-01-01T00:04:30.000Z"),
                max_artifact_bytes: 1_000_000,
                max_runtime_seconds: 240,
            },
        },
    )
    .expect("prepare canonical verification execution");
    (delivery, pending)
}

fn pending_planning_execution(checkout_revision: &str) -> PendingDeliveryExecution {
    let delivery = delivery_before_planning();
    let advanced = advance(
        &delivery,
        AdvanceStageInput {
            current_lease: None,
            rework_authorization: None,
            expected_revision: 1,
            product_session_id: ProductSessionId(id("psn", 1)),
            identities: NewStageIdentities {
                stage_run_id: StageRunId(id("run", 1)),
                execution_job_id: ExecutionJobId(id("job", 1)),
                session_binding_id: SessionBindingId::new(
                    "binding-production-planning-vertical".to_owned(),
                )
                .expect("planning SessionBinding id"),
                attention_item_id: winwincode_domain::AttentionItemId(id("att", 1)),
            },
            review: None,
            previous_outcome: None,
            now_millis: 1_893_455_999_000,
        },
    )
    .expect("advance production planning Delivery");
    prepare_delivery_advance(
        RequestId(id("req", 13)),
        advanced,
        DeliveryExecutionConfig {
            payload_digest: digest('d'),
            candidate_ref: None,
            workspace: ExecutionWorkspace {
                checkout_revision: checkout_revision.to_owned(),
                repository_id: RepositoryId(id("rep", 1)),
                write_mode: ExecutionWorkspaceWriteMode::ReadOnly,
            },
            limits: ExecutionLimits {
                deadline_at: at("2030-01-01T00:04:30.000Z"),
                max_artifact_bytes: 1_000_000,
                max_runtime_seconds: 240,
            },
        },
    )
    .expect("prepare canonical planning execution")
}

fn execution_scope(job: &ExecutionJob) -> ExecutionQueueScope {
    let scope = repository_scope();
    let (product_session_id, delivery_id) = match &job.scope {
        ExecutionScope::DeliveryStageExecutionScope(stage) => (
            stage.product_session_id.clone(),
            Some(stage.delivery_id.clone()),
        ),
        ExecutionScope::ProductSessionExecutionScope(session) => {
            (session.product_session_id.clone(), None)
        }
    };
    ExecutionQueueScope {
        organization_id: scope.organization_id,
        workspace_id: scope.workspace_id,
        project_id: scope.project_id,
        repository_id: scope.repository_id,
        product_session_id,
        delivery_id,
    }
}

fn admission_boundaries(scope: &ExecutionQueueScope) -> Vec<ExecutionAdmissionBoundary> {
    let mut boundaries = vec![
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
            product_session_id: scope.product_session_id.clone(),
        },
        ExecutionAdmissionBoundary::WorkerPool {
            organization_id: scope.organization_id.clone(),
            worker_pool_id: WorkerPoolId(id("wpl", 1)),
        },
    ];
    if let Some(delivery_id) = scope.delivery_id.clone() {
        boundaries.push(ExecutionAdmissionBoundary::Delivery {
            organization_id: scope.organization_id.clone(),
            delivery_id,
        });
    }
    boundaries
}

fn register_worker(storage: &mut SqliteStorage, message: &ModelOpenMessage) {
    let registration = WorkerRegistrationRequest {
        authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
            control_plane_principal: "embedded-native-kernel".to_owned(),
        },
        protocol_version: EXECUTION_PROTOCOL_VERSION.to_owned(),
        platform: WorkerPlatform::Aarch64AppleDarwin,
        capabilities: vec!["model".to_owned()],
        capability_digest: digest('a'),
        security_zone: "local".to_owned(),
        max_slots: 1,
        message_id: ExecutionMessageId(id("xmsg", 5)),
        request_id: RequestId(id("req", 5)),
        sent_at: at("2029-12-31T23:59:56.000Z"),
        started_at: at("2029-12-31T23:59:55.000Z"),
        worker_id: message.lease.worker_id.clone(),
        worker_instance_id: message.lease.worker_instance_id.clone(),
    };
    let heartbeat = WorkerHeartbeatRequest {
        active_leases: Vec::new(),
        available_slots: 1,
        heartbeat_sequence: ExecutionSequence(1),
        max_slots: 1,
        running_slots: 0,
        message_id: ExecutionMessageId(id("xmsg", 6)),
        observed_at: at("2029-12-31T23:59:58.000Z"),
        sent_at: at("2029-12-31T23:59:58.000Z"),
        worker_id: message.lease.worker_id.clone(),
        worker_instance_id: message.lease.worker_instance_id.clone(),
    };
    let mut registry = storage.execution_registry().expect("execution registry");
    registry
        .register_worker(&registration)
        .expect("register embedded Worker");
    assert_eq!(
        registry
            .record_heartbeat(&heartbeat)
            .expect("record embedded Worker heartbeat")
            .status,
        StorageLeaseWriteStatus::Accepted
    );
}

fn start_execution_admission(
    storage: &mut SqliteStorage,
    message: &ModelOpenMessage,
    job: &ExecutionJob,
) {
    let scope = execution_scope(job);
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 1,
        max_queued: 1,
        token_budget: 10_000,
        cost_budget_microunits: 10_000,
        max_runtime_millis: 300_000,
    };
    let mut admission = storage.execution_admission().expect("execution admission");
    for boundary in admission_boundaries(&scope) {
        admission
            .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
            .expect("configure execution admission policy");
    }
    admission
        .reserve(&ExecutionReservationRequest {
            scope: scope.clone(),
            user_id: UserId(id("usr", 1)),
            worker_pool_id: WorkerPoolId(id("wpl", 1)),
            job_id: message.lease.job_id.clone(),
            request_id: RequestId(id("req", 7)),
            repository_access: ExecutionRepositoryAccess::ReadOnly,
            reserved_tokens: 100,
            reserved_cost_microunits: 100,
            runtime_limit_millis: 300_000,
            submitted_at: at("2029-12-31T23:59:56.000Z"),
        })
        .expect("reserve execution");
    admission
        .start(&ExecutionReservationStart {
            scope,
            worker_pool_id: WorkerPoolId(id("wpl", 1)),
            job_id: message.lease.job_id.clone(),
            request_id: RequestId(id("req", 8)),
            expected_revision: 1,
            started_at: at("2029-12-31T23:59:57.000Z"),
        })
        .expect("start execution");
}

fn claim_lease_and_open_slot(
    storage: &mut SqliteStorage,
    message: &ModelOpenMessage,
    job: &ExecutionJob,
) {
    let claim = ExecutionLeaseClaim {
        expires_at: message.lease.expires_at.clone(),
        fencing_token: message.lease.fencing_token.clone(),
        issued_at: message.lease.issued_at.clone(),
        job_id: message.lease.job_id.clone(),
        lease_id: message.lease.lease_id.clone(),
        message_id: ExecutionMessageId(id("xmsg", 9)),
        payload_digest: job.payload_digest.clone(),
        request_id: RequestId(id("req", 9)),
        worker_id: message.lease.worker_id.clone(),
        worker_instance_id: message.lease.worker_instance_id.clone(),
        attempt: u64::try_from(message.lease.attempt).expect("positive attempt"),
    };
    assert_eq!(
        storage
            .execution_registry()
            .expect("execution registry")
            .claim_execution_job(&claim)
            .expect("claim execution lease")
            .status,
        StorageLeaseWriteStatus::Accepted
    );
    let authority = slot_authority(message);
    let mut slots = storage.worker_session_slots().expect("Worker slots");
    slots
        .configure_resources(
            &authority.worker_id,
            &authority.worker_instance_id,
            WorkerSlotResourceLimits {
                max_memory_bytes: 1_000,
                max_disk_bytes: 1_000,
                max_processes: 4,
            },
        )
        .expect("configure slot resources");
    slots
        .open(&WorkerSlotOpenRequest {
            authority,
            resources: WorkerSlotResources {
                memory_bytes: 10,
                disk_bytes: 10,
                process_slots: 1,
            },
            request_id: RequestId(id("req", 10)),
            opened_at: message.lease.issued_at.clone(),
        })
        .expect("open WorkerSession slot");
}

fn slot_authority(message: &ModelOpenMessage) -> WorkerSlotAuthority {
    WorkerSlotAuthority {
        worker_id: message.lease.worker_id.clone(),
        worker_instance_id: message.lease.worker_instance_id.clone(),
        worker_session_id: message.worker_session_id.clone(),
        codex_thread_id: message.session_identity.codex_thread_id.clone(),
        job_id: message.lease.job_id.clone(),
        lease_id: message.lease.lease_id.clone(),
        attempt: u64::try_from(message.lease.attempt).expect("positive attempt"),
        fencing_token: message.lease.fencing_token.clone(),
    }
}

fn setup(root: &TestDirectory, message: &ModelOpenMessage, job: &ExecutionJob) {
    if matches!(&job.scope, ExecutionScope::DeliveryStageExecutionScope(_)) {
        seed_delivery_job(root, job);
    }
    setup_model(root, message, job);
}

fn setup_model(root: &TestDirectory, message: &ModelOpenMessage, job: &ExecutionJob) {
    let resolution = {
        let mut storage = SqliteStorage::open(root.data()).expect("open setup storage");
        configure_provider(&mut storage, message);
        if matches!(&job.scope, ExecutionScope::ProductSessionExecutionScope(_)) {
            lease_product_session_execution_queue(&mut storage, job);
        } else {
            commit_execution_queue(&mut storage, job);
        }
        register_worker(&mut storage, message);
        start_execution_admission(&mut storage, message, job);
        claim_lease_and_open_slot(&mut storage, message, job);
        CredentialReferenceService::new(&mut storage)
            .resolve(
                &Scope::OrganizationScope(organization_scope()),
                &CredentialReferenceId(id("crd", 1)),
            )
            .expect("resolve loopback credential")
    };
    LocalSecretStoreAdapter::open(root.secrets())
        .expect("open local SecretStore")
        .store(
            &resolution,
            ResolvedSecret::from_bytes(PROVIDER_SECRET.to_vec()).expect("resolved secret"),
        )
        .expect("store loopback credential");
}

struct FixedClock;

impl ModelAdmissionClock for FixedClock {
    fn unix_minute(&self) -> Result<u64, ModelAdmissionClockError> {
        Ok(31_556_300)
    }
}

fn policy() -> LocalModelPolicyAuthority {
    let base = ModelAdmissionPolicyLayer::try_new(
        "native-model-port-loopback-policy".to_owned(),
        1,
        "budget-2030-01".to_owned(),
        ModelRoutePolicyDecision::Allow,
        ModelAdmissionLimits {
            requests_per_minute: 100,
            tokens_per_minute: 100_000,
            concurrent_requests: 100,
            token_budget: 1_000_000,
            cost_budget_micros: 1_000_000,
        },
    )
    .expect("model admission policy");
    LocalModelPolicyAuthority::try_new(LocalModelPolicyAuthorityConfig {
        base,
        enterprise_ceilings: Vec::new(),
    })
    .expect("local model policy authority")
}

fn application(root: &TestDirectory) -> StandaloneModelExecutionApplication {
    StandaloneModelExecutionApplication::open_with_clock(
        StandaloneModelExecutionConfig {
            data_directory: root.data(),
            secret_directory: root.secrets(),
            providers: vec![StandaloneProviderConfig::Loopback {
                provider_id: PROVIDER_ID.to_owned(),
            }],
            admission: ProviderAdmissionReservationConfig::try_new(100, 10)
                .expect("Provider reservation config"),
            pool: ModelRequestPoolConfig {
                max_routes: 4,
                max_active_per_route: 2,
                max_waiting_per_route: 4,
                max_exchange_records_per_route: 8,
                max_buffered_frames_per_stream: 32,
                max_buffered_bytes_per_stream: 64 * 1024,
                resume_buffered_frames_per_stream: 8,
                resume_buffered_bytes_per_stream: 16 * 1024,
            },
            policy: Box::new(policy()),
            retry_policy: Box::new(
                local_loopback_retry_policy().expect("loopback retry authority"),
            ),
        },
        Box::new(FixedClock),
    )
    .expect("open standalone model application")
}

fn typed(message: ExecutionPortMessage) -> TypedFrame {
    TypedFrame::new(FrameDirection::WorkerToControlPlane, message)
        .expect("typed Worker ExecutionPort frame")
}

fn opened(receipt: ModelExecutionPortReceipt) -> ProviderGatewayOpenReceipt {
    opened_with_replay(receipt).0
}

fn opened_with_replay(receipt: ModelExecutionPortReceipt) -> (ProviderGatewayOpenReceipt, bool) {
    let ModelExecutionPortReceipt::Opened(ModelExecutionOpenReceipt::Opened {
        gateway,
        idempotent_replay,
        ..
    }) = receipt
    else {
        panic!("standalone ModelOpen must enter the Provider Gateway");
    };
    (gateway, idempotent_replay)
}

fn provider_chunks(
    open: &ModelOpenMessage,
    gateway: &ProviderGatewayOpenReceipt,
    events: impl IntoIterator<Item = ProviderStreamEvent>,
    message_seed: u64,
) -> Vec<ModelChunkMessage> {
    let mut converter = ProviderStreamConverter::from_gateway_receipt(gateway);
    events
        .into_iter()
        .flat_map(|event| converter.ingest(event).expect("convert Provider event"))
        .map(|frame| ModelChunkMessage {
            error: None,
            is_final: frame.is_terminal(),
            kind: ModelChunkMessageKind::ModelChunk,
            lease: open.lease.clone(),
            message_id: ExecutionMessageId(id("xmsg", message_seed + frame.sequence())),
            model_exchange_id: open.model_exchange_id.clone(),
            payload: Some(frame.encoded_payload()),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: at("2030-01-01T00:00:02.000Z"),
            sequence: ExecutionSequence(
                i64::try_from(frame.sequence()).expect("Provider sequence fits ExecutionPort"),
            ),
            session_identity: open.session_identity.clone(),
            worker_session_id: open.worker_session_id.clone(),
        })
        .collect()
}

fn input_response(request: &InputRequestMessage) -> InputResponseMessage {
    let responded_at = at("2030-01-01T00:00:02.000Z");
    let value = request
        .choices
        .as_ref()
        .and_then(|choices| choices.first())
        .map_or_else(|| "accepted".to_owned(), |choice| choice.value.clone());
    InputResponseMessage {
        input_request_id: request.input_request_id.clone(),
        kind: InputResponseMessageKind::InputResponse,
        lease: request.lease.clone(),
        message_id: ExecutionMessageId(id("xmsg", 950)),
        responded_at: responded_at.clone(),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: responded_at,
        session_identity: request.session_identity.clone(),
        status: InputResponseMessageStatus::Provided,
        value: Some(InteractiveInputValue {
            mode: request.mode.clone(),
            value,
        }),
        worker_session_id: request.worker_session_id.clone(),
    }
}

#[derive(Clone, Default)]
struct RecordedPort {
    messages: std::sync::Arc<Mutex<Vec<ExecutionPortMessage>>>,
}

impl RecordedPort {
    fn messages(&self) -> Vec<ExecutionPortMessage> {
        self.messages.lock().expect("lock Worker messages").clone()
    }
}

impl winwincode_codex::WorkerExecutionPort for RecordedPort {
    type Error = ();

    fn send(
        &mut self,
        message: ExecutionPortMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        self.messages
            .lock()
            .expect("lock Worker messages")
            .push(message);
        std::future::ready(Ok(()))
    }
}

fn helper_executable() -> PathBuf {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf();
    let target = std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || workspace.join("target"),
        |value| {
            let value = PathBuf::from(value);
            if value.is_absolute() {
                value
            } else {
                workspace.join(value)
            }
        },
    );
    target.join("debug/winwincode-kernel-helper")
}

fn helper_release_manifest() -> winwincode_codex::HelperReleaseManifest {
    winwincode_codex::HelperReleaseManifest::from_test_helper(&helper_executable())
        .expect("build test helper release manifest")
}

fn action_signing_key() -> ActionEnforcementSigningKey {
    ActionEnforcementSigningKey::from_bytes([31_u8; 32]).expect("action signing key")
}

fn worker_config() -> winwincode_worker::WorkerConfig {
    winwincode_worker::WorkerConfig {
        worker_id: WorkerId(id("wrk", 1)),
        worker_instance_id: WorkerInstanceId(id("wki", 1)),
        started_at: at("2029-12-31T23:59:55.000Z"),
        capabilities: WorkerCapabilitySet {
            capability_digest: digest('a'),
            features: vec![WorkerCapabilityFeature::Sandbox],
            max_concurrent_jobs: 1,
            platform: WorkerCapabilitySetPlatform::Aarch64AppleDarwin,
        },
    }
}

fn dispatch(root: &TestDirectory) -> JobDispatchMessage {
    let revision = root.source_revision();
    let job = pending_delivery_execution(&revision).job().clone();
    dispatch_for_job(job.clone(), lease_for_job(&job, "delivery"))
}

fn input_dispatch(root: &TestDirectory) -> JobDispatchMessage {
    let scope = repository_scope();
    let product_session_id = ProductSessionId(id("psn", 901));
    let mut storage = SqliteStorage::open(root.data()).expect("open input fixture storage");
    let create_context = product_session_command_context(
        &actor(),
        &scope,
        RequestId(id("req", 901)),
        &Revision(0),
        ControlPlaneEventId(id("evt", 901)),
        at("2029-12-31T23:59:55.000Z"),
    )
    .expect("build ProductSession create context");
    let mut service = ProductSessionService::new(&mut storage);
    service
        .create(&CreateProductSessionCommand {
            context: create_context,
            product_session_id: product_session_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
            title: "Production input response fixture".to_owned(),
            model_route: ModelRoute {
                credential_reference_id: CredentialReferenceId(id("crd", 1)),
                provider_id: PROVIDER_ID.to_owned(),
                model_id: MODEL_ID.to_owned(),
            },
        })
        .expect("create ProductSession for the production input flow");
    let execution_config = ProductSessionExecutionConfig::try_new(
        scope.clone(),
        root.source_revision(),
        "chat",
        240,
        1_000_000,
    )
    .expect("build ProductSession execution config");
    let submit_context = product_session_command_context(
        &actor(),
        &scope,
        RequestId(id("req", 902)),
        &Revision(1),
        ControlPlaneEventId(id("evt", 902)),
        at("2030-01-01T00:00:00.000Z"),
    )
    .expect("build ProductSession submit context");
    let receipt = service
        .submit_chat(&SubmitChatMessageCommand {
            context: submit_context,
            product_session_id: product_session_id.clone(),
            message: "Reply with the selected input.".to_owned(),
            execution_config,
        })
        .expect("submit ProductSession Chat turn");
    drop(service);
    let queue_scope = ExecutionQueueScope {
        organization_id: scope.organization_id,
        workspace_id: scope.workspace_id,
        project_id: scope.project_id,
        repository_id: scope.repository_id,
        product_session_id,
        delivery_id: None,
    };
    let record = storage
        .execution_queue()
        .expect("execution queue")
        .load_job(&queue_scope, &receipt.turn_intent.execution_job_id)
        .expect("load ProductSession execution Job")
        .expect("ProductSession execution Job");
    let job: ExecutionJob =
        serde_json::from_slice(&record.dispatch_payload).expect("decode ProductSession Job");
    dispatch_for_job(job.clone(), lease_for_job(&job, "input"))
}

fn lease_for_job(job: &ExecutionJob, suffix: &str) -> ExecutionLeaseStamp {
    let lease_seed = match suffix {
        "reviewer" => 21,
        "verifier" => 22,
        _ => 1,
    };
    ExecutionLeaseStamp {
        attempt: 1,
        expires_at: at("2030-01-01T00:05:00.000Z"),
        fencing_token: FencingToken("1".to_owned()),
        issued_at: at("2030-01-01T00:00:00.000Z"),
        job_id: job.job_id.clone(),
        lease_id: LeaseId(id("lse", lease_seed)),
        worker_id: WorkerId(id("wrk", 1)),
        worker_instance_id: WorkerInstanceId(id("wki", 1)),
    }
}

fn verification_dispatch(root: &TestDirectory, role: &str) -> JobDispatchMessage {
    assert!(matches!(role, "reviewer" | "verifier"));
    let revision = root.source_revision();
    let (_, pending) = pending_verification_execution(&revision, role);
    let job = pending.job().clone();
    dispatch_for_job(job.clone(), lease_for_job(&job, role))
}

fn planning_dispatch(root: &TestDirectory) -> JobDispatchMessage {
    let lease = ExecutionLeaseStamp {
        attempt: 1,
        expires_at: at("2030-01-01T00:05:00.000Z"),
        fencing_token: FencingToken("1".to_owned()),
        issued_at: at("2030-01-01T00:00:00.000Z"),
        job_id: ExecutionJobId(id("job", 1)),
        lease_id: LeaseId(id("lse", 1)),
        worker_id: WorkerId(id("wrk", 1)),
        worker_instance_id: WorkerInstanceId(id("wki", 1)),
    };
    let revision = root.source_revision();
    let job = pending_planning_execution(&revision).job().clone();
    dispatch_for_job(job, lease)
}

fn dispatch_for_job(job: ExecutionJob, lease: ExecutionLeaseStamp) -> JobDispatchMessage {
    JobDispatchMessage {
        job,
        kind: JobDispatchMessageKind::JobDispatch,
        lease,
        message_id: ExecutionMessageId(id("xmsg", 80)),
        replacement_authority: None,
        request_id: RequestId(id("req", 80)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: at("2030-01-01T00:00:00.000Z"),
    }
}

fn adapter_config(root: &TestDirectory) -> winwincode_codex::ProductionCodexConfig {
    adapter_config_with_mode(root, winwincode_codex::ExecutionMode::React)
}

fn adapter_config_with_mode(
    root: &TestDirectory,
    execution_mode: winwincode_codex::ExecutionMode,
) -> winwincode_codex::ProductionCodexConfig {
    winwincode_codex::ProductionCodexConfig::try_new(winwincode_codex::ProductionCodexOptions {
        data_directory: root.worker(),
        helper_executable: helper_executable(),
        helper_release_manifest: helper_release_manifest(),
        provider: PROVIDER_ID.to_owned(),
        model: MODEL_ID.to_owned(),
        gateway_route: ModelGatewayRoute {
            capability: "reasoning".to_owned(),
            route: "embedded-canonical-loopback".to_owned(),
        },
        registered_capabilities: worker_config().capabilities,
        discovered_capabilities: Vec::new(),
        action_signing_key: action_signing_key(),
        execution_envelope: winwincode_execution_port::action_gateway::ExecutionEnvelopeToken {
            version: 1,
            digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        },
        execution_mode,
        observer_mode: winwincode_codex::ObserverMode::Off,
    })
    .expect("validated production Codex configuration")
}

#[test]
fn delegated_executor_reads_the_candidate_then_retains_one_strict_batch_intent() {
    run_on_large_stack(async {
        let root = TestDirectory::new("production-delegated-batch");
        let mut dispatch = dispatch(&root);
        dispatch.job.workspace.write_mode = ExecutionWorkspaceWriteMode::ReadOnly;
        let port = RecordedPort::default();
        let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config_with_mode(
            &root,
            winwincode_codex::ExecutionMode::DelegatedPatch,
        ))
        .expect("open delegated production adapter");
        let mut worker = winwincode_worker::WorkerMain::new(
            worker_config(),
            port.clone(),
            adapter,
            root.workspace_runtime(),
        );
        register(&mut worker, &port).await;
        worker
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
                at("2030-01-01T00:00:00.000Z"),
            )
            .await
            .expect("accept delegated dispatch");
        let open = poll_until_message(
            &mut worker,
            &port,
            &at("2030-01-01T00:00:01.000Z"),
            |messages| {
                messages.iter().find_map(|message| match message {
                    ExecutionPortMessage::ModelOpenMessage(open) => Some(open.clone()),
                    _ => None,
                })
            },
            "delegated model request was not delivered",
        )
        .await;
        let request_bytes = STANDARD
            .decode(&open.request.data_base64)
            .expect("decode delegated request");
        let request: serde_json::Value =
            serde_json::from_slice(&request_bytes).expect("decode delegated request JSON");
        let format = &request["request"]["text"]["format"];
        assert_eq!(format["type"], "json_schema", "{request:#}");
        assert_eq!(format["strict"], true);
        assert_eq!(format["schema"]["additionalProperties"], false);

        setup_model(&root, &open, &dispatch.job);
        let mut app = application(&root);
        let gateway = opened(
            app.accept_local(&typed(ExecutionPortMessage::ModelOpenMessage(open.clone())))
                .expect("accept delegated ModelOpen"),
        );
        let checkout = detached_checkout(&root);
        let read_call_id = "provider-delegated-read-call".to_owned();
        let read_identity = ProviderToolIdentity::try_new(
            ProviderToolKind::Function,
            "shell_command".to_owned(),
            Some("functions".to_owned()),
        )
        .expect("canonical delegated read tool");
        for chunk in provider_chunks(
            &open,
            &gateway,
            [
                ProviderStreamEvent::ResponseStarted {
                    provider_response_id: "provider-delegated-read-response".to_owned(),
                },
                ProviderStreamEvent::ToolCallStarted {
                    index: 0,
                    provider_call_id: read_call_id.clone(),
                    identity: read_identity,
                },
                ProviderStreamEvent::ToolCallArgumentsDelta {
                    index: 0,
                    provider_call_id: read_call_id.clone(),
                    delta: serde_json::json!({
                        "command": "cat src/lib.rs",
                        "workdir": checkout.to_string_lossy(),
                    })
                    .to_string(),
                },
                ProviderStreamEvent::ToolCallEnded {
                    index: 0,
                    provider_call_id: read_call_id,
                },
                ProviderStreamEvent::Usage(ProviderTokenUsage {
                    input_tokens: 10,
                    cached_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    output_tokens: 5,
                    reasoning_output_tokens: 0,
                }),
                ProviderStreamEvent::Finished(ProviderFinishReason::ToolCalls),
            ],
            970,
        ) {
            worker
                .accept_control(
                    &ExecutionPortMessage::ModelChunkMessage(chunk),
                    at("2030-01-01T00:00:02.000Z"),
                )
                .await
                .expect("deliver delegated read call");
        }
        let followup_open = poll_until_message(
            &mut worker,
            &port,
            &at("2030-01-01T00:00:02.000Z"),
            |messages| {
                messages.iter().find_map(|message| match message {
                    ExecutionPortMessage::ModelOpenMessage(candidate)
                        if candidate.message_id != open.message_id =>
                    {
                        Some(candidate.clone())
                    }
                    _ => None,
                })
            },
            "delegated Core read did not produce a follow-up model request",
        )
        .await;
        let followup_request = STANDARD
            .decode(&followup_open.request.data_base64)
            .expect("decode delegated follow-up request");
        let followup_request =
            String::from_utf8(followup_request).expect("delegated follow-up request is UTF-8 JSON");
        assert!(
            followup_request.contains("fixture_value() -> u64 { 1 }"),
            "the exact candidate bytes read by Core must reach the proposal turn"
        );
        assert!(
            !port.messages().iter().any(|message| matches!(
                message,
                ExecutionPortMessage::ActionEnforcementRequestMessage(_)
            )),
            "delegated read-only tools do not enter the writable action queue"
        );
        assert_eq!(
            fs::read_to_string(checkout.join("src/lib.rs"))
                .expect("read unchanged delegated candidate"),
            "pub fn fixture_value() -> u64 { 1 }\n",
            "the real delegated read must leave the candidate unchanged"
        );
        let followup_gateway = opened(
            app.accept_local(&typed(ExecutionPortMessage::ModelOpenMessage(
                followup_open.clone(),
            )))
            .expect("accept delegated follow-up ModelOpen"),
        );
        let final_message = serde_json::json!({
            "acceptanceCriteriaIds": dispatch.job.stage_input
                .as_ref()
                .and_then(|input| input.task.as_ref())
                .expect("delegated task")
                .acceptance_criterion_ids,
            "disposition": "final",
            "patch": "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-pub fn fixture_value() -> u64 { 1 }\n+pub fn fixture_value() -> u64 { 2 }\n*** End Patch\n",
            "schemaVersion": 1,
            "validationProfile": "changed"
        })
        .to_string();
        let usage = ProviderTokenUsage {
            input_tokens: 10,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 5,
            reasoning_output_tokens: 0,
        };
        for chunk in provider_chunks(
            &followup_open,
            &followup_gateway,
            [
                ProviderStreamEvent::ResponseStarted {
                    provider_response_id: "provider-delegated-response".to_owned(),
                },
                ProviderStreamEvent::TextStarted { index: 0 },
                ProviderStreamEvent::TextDelta {
                    index: 0,
                    delta: final_message,
                },
                ProviderStreamEvent::TextEnded { index: 0 },
                ProviderStreamEvent::Usage(usage),
                ProviderStreamEvent::Finished(ProviderFinishReason::Stop),
            ],
            980,
        ) {
            worker
                .accept_control(
                    &ExecutionPortMessage::ModelChunkMessage(chunk),
                    at("2030-01-01T00:00:02.000Z"),
                )
                .await
                .expect("deliver delegated structured output");
        }
        let mut delegated = Vec::new();
        for _ in 0..400 {
            worker
                .poll_codex(at("2030-01-01T00:00:02.000Z"))
                .await
                .expect("poll delegated result");
            delegated = worker.take_delegated_poll_outcomes();
            if delegated.iter().any(|outcome| {
                matches!(
                    outcome,
                    winwincode_worker::DelegatedPollOutcome::ChangeBatchProposed(_)
                )
            }) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let expected_intent = delegated
            .iter()
            .find_map(|outcome| match outcome {
                winwincode_worker::DelegatedPollOutcome::ChangeBatchProposed(event) => {
                    Some((**event).clone())
                }
                _ => None,
            })
            .expect("delegated proposal was not retained");
        assert!(delegated.iter().any(|outcome| matches!(
            outcome,
            winwincode_worker::DelegatedPollOutcome::ChangeBatchProgress(event)
                if event.state == ChangeBatchProgressState::Applied
        )));
        assert!(delegated.iter().any(|outcome| matches!(
            outcome,
            winwincode_worker::DelegatedPollOutcome::ChangeBatchReceipt(receipt)
                if receipt.status == ChangeBatchReceiptStatus::Applied
                    && receipt.result_revision.is_some()
                    && receipt.delta_exact
                    && receipt.delta_digest.is_some()
        )));
        let expected_replay = delegated.clone();
        assert_eq!(worker.active_jobs().len(), 1);
        assert_eq!(
            port.messages()
                .iter()
                .filter(|message| matches!(message, ExecutionPortMessage::ModelOpenMessage(_)))
                .count(),
            2
        );
        assert!(
            !port
                .messages()
                .iter()
                .any(|message| matches!(message, ExecutionPortMessage::JobOutcomeMessage(_)))
        );

        drop(worker);
        let replay_port = RecordedPort::default();
        let replay_adapter = winwincode_codex::ProductionCodexAdapter::open(
            adapter_config_with_mode(&root, winwincode_codex::ExecutionMode::DelegatedPatch),
        )
        .expect("reopen delegated production adapter");
        let mut replay_worker = winwincode_worker::WorkerMain::new(
            worker_config(),
            replay_port.clone(),
            replay_adapter,
            root.workspace_runtime(),
        );
        register(&mut replay_worker, &replay_port).await;
        replay_worker
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
                at("2030-01-01T00:00:03.000Z"),
            )
            .await
            .expect("recover delegated dispatch");
        let mut replayed = Vec::new();
        for _ in 0..400 {
            replay_worker
                .poll_codex(at("2030-01-01T00:00:03.000Z"))
                .await
                .expect("poll replayed delegated intent");
            replayed = replay_worker.take_delegated_poll_outcomes();
            if replayed.iter().any(|outcome| {
                matches!(
                    outcome,
                    winwincode_worker::DelegatedPollOutcome::ChangeBatchProposed(_)
                )
            }) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let replayed_intent = replayed
            .iter()
            .find_map(|outcome| match outcome {
                winwincode_worker::DelegatedPollOutcome::ChangeBatchProposed(event) => {
                    Some((**event).clone())
                }
                _ => None,
            })
            .expect("replayed delegated intent");
        assert_eq!(replayed_intent, expected_intent);
        assert_eq!(replayed, expected_replay);
        replay_worker
            .poll_codex(at("2030-01-01T00:00:04.000Z"))
            .await
            .expect("poll consumed delegated intent");
        assert!(replay_worker.take_delegated_poll_outcomes().is_empty());
        assert!(replay_worker.active_jobs().len() == 1);
        assert!(!replay_port.messages().iter().any(|message| matches!(
            message,
            ExecutionPortMessage::ModelOpenMessage(_) | ExecutionPortMessage::JobOutcomeMessage(_)
        )));
    });
}

#[test]
fn delegated_invalid_output_gets_one_format_repair_then_becomes_inconclusive() {
    run_on_large_stack(async {
        let root = TestDirectory::new("production-delegated-repair");
        let mut dispatch = dispatch(&root);
        dispatch.job.workspace.write_mode = ExecutionWorkspaceWriteMode::ReadOnly;
        let port = RecordedPort::default();
        let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config_with_mode(
            &root,
            winwincode_codex::ExecutionMode::DelegatedPatch,
        ))
        .expect("open delegated repair adapter");
        let mut worker = winwincode_worker::WorkerMain::new(
            worker_config(),
            port.clone(),
            adapter,
            root.workspace_runtime(),
        );
        register(&mut worker, &port).await;
        worker
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
                at("2030-01-01T00:00:00.000Z"),
            )
            .await
            .expect("accept delegated repair dispatch");
        let first_open = poll_until_message(
            &mut worker,
            &port,
            &at("2030-01-01T00:00:01.000Z"),
            |messages| {
                messages.iter().find_map(|message| match message {
                    ExecutionPortMessage::ModelOpenMessage(open) => Some(open.clone()),
                    _ => None,
                })
            },
            "delegated repair request was not delivered",
        )
        .await;
        setup_model(&root, &first_open, &dispatch.job);
        let mut app = application(&root);
        let first_gateway = opened(
            app.accept_local(&typed(ExecutionPortMessage::ModelOpenMessage(
                first_open.clone(),
            )))
            .expect("accept first delegated repair ModelOpen"),
        );
        let usage = ProviderTokenUsage {
            input_tokens: 10,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 5,
            reasoning_output_tokens: 0,
        };
        for chunk in provider_chunks(
            &first_open,
            &first_gateway,
            [
                ProviderStreamEvent::ResponseStarted {
                    provider_response_id: "provider-delegated-invalid-1".to_owned(),
                },
                ProviderStreamEvent::TextStarted { index: 0 },
                ProviderStreamEvent::TextDelta {
                    index: 0,
                    delta: "not a ChangeBatch proposal".to_owned(),
                },
                ProviderStreamEvent::TextEnded { index: 0 },
                ProviderStreamEvent::Usage(usage),
                ProviderStreamEvent::Finished(ProviderFinishReason::Stop),
            ],
            990,
        ) {
            worker
                .accept_control(
                    &ExecutionPortMessage::ModelChunkMessage(chunk),
                    at("2030-01-01T00:00:02.000Z"),
                )
                .await
                .expect("deliver first invalid delegated output");
        }
        let repair_open = poll_until_message(
            &mut worker,
            &port,
            &at("2030-01-01T00:00:02.000Z"),
            |messages| {
                messages
                    .iter()
                    .filter_map(|message| match message {
                        ExecutionPortMessage::ModelOpenMessage(open) => Some(open.clone()),
                        _ => None,
                    })
                    .nth(1)
            },
            "one delegated format-repair request was not delivered",
        )
        .await;
        let first_request: serde_json::Value = serde_json::from_slice(
            &STANDARD
                .decode(&first_open.request.data_base64)
                .expect("decode first delegated repair request"),
        )
        .expect("parse first delegated repair request");
        let repair_request: serde_json::Value = serde_json::from_slice(
            &STANDARD
                .decode(&repair_open.request.data_base64)
                .expect("decode delegated format-repair request"),
        )
        .expect("parse delegated format-repair request");
        assert_eq!(
            repair_request["request"]["text"]["format"],
            first_request["request"]["text"]["format"]
        );
        let repair_gateway = opened(
            app.accept_local(&typed(ExecutionPortMessage::ModelOpenMessage(
                repair_open.clone(),
            )))
            .expect("accept delegated format-repair ModelOpen"),
        );
        for chunk in provider_chunks(
            &repair_open,
            &repair_gateway,
            [
                ProviderStreamEvent::ResponseStarted {
                    provider_response_id: "provider-delegated-invalid-2".to_owned(),
                },
                ProviderStreamEvent::TextStarted { index: 0 },
                ProviderStreamEvent::TextDelta {
                    index: 0,
                    delta: "still invalid".to_owned(),
                },
                ProviderStreamEvent::TextEnded { index: 0 },
                ProviderStreamEvent::Usage(ProviderTokenUsage {
                    input_tokens: 10,
                    cached_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    output_tokens: 5,
                    reasoning_output_tokens: 0,
                }),
                ProviderStreamEvent::Finished(ProviderFinishReason::Stop),
            ],
            1_000,
        ) {
            worker
                .accept_control(
                    &ExecutionPortMessage::ModelChunkMessage(chunk),
                    at("2030-01-01T00:00:02.000Z"),
                )
                .await
                .expect("deliver second invalid delegated output");
        }
        let outcome = poll_until_message(
            &mut worker,
            &port,
            &at("2030-01-01T00:00:02.000Z"),
            |messages| {
                messages.iter().find_map(|message| match message {
                    ExecutionPortMessage::JobOutcomeMessage(outcome) => Some(outcome.clone()),
                    _ => None,
                })
            },
            "delegated invalid repair did not become inconclusive",
        )
        .await;
        assert_eq!(outcome.outcome.status, ExecutionOutcomeStatus::Failed);
        let error = outcome
            .outcome
            .error
            .as_ref()
            .expect("content failure code");
        assert_eq!(error.code, ExecutionPortErrorCode::ExecutionFailed);
        assert!(!error.retryable);
        assert_eq!(
            stored_run_json(&root)["terminal"]["kind"],
            "delegated_inconclusive"
        );
        assert_eq!(
            port.messages()
                .iter()
                .filter(|message| matches!(message, ExecutionPortMessage::ModelOpenMessage(_)))
                .count(),
            2
        );
        assert!(worker.take_delegated_poll_outcomes().is_empty());
        assert!(worker.active_jobs().is_empty());
        assert_eq!(
            fs::read_to_string(root.sources().join(id("rep", 1)).join("src/lib.rs"),)
                .expect("read unchanged delegated source"),
            "pub fn fixture_value() -> u64 { 1 }\n"
        );
        assert!(!port.messages().iter().any(|message| matches!(
            message,
            ExecutionPortMessage::ActionEnforcementRequestMessage(_)
        )));

        let first_terminal_facts = unique_terminal_facts(&port.messages());
        worker
            .shutdown(at("2030-01-01T00:00:03.000Z"))
            .await
            .expect("shutdown delegated content-failure Worker");
        drop(worker);

        let replay_port = RecordedPort::default();
        let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config_with_mode(
            &root,
            winwincode_codex::ExecutionMode::DelegatedPatch,
        ))
        .expect("reopen delegated content-failure adapter");
        let mut replay = winwincode_worker::WorkerMain::new(
            worker_config(),
            replay_port.clone(),
            adapter,
            root.workspace_runtime(),
        );
        register(&mut replay, &replay_port).await;
        replay
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch),
                at("2030-01-01T00:00:00.000Z"),
            )
            .await
            .expect("accept delegated content-failure replay dispatch");
        run_until_outcome_without_gateway(&mut replay, &replay_port).await;
        assert!(!replay_port.messages().iter().any(|message| matches!(
            message,
            ExecutionPortMessage::ModelOpenMessage(_)
                | ExecutionPortMessage::ActionEnforcementRequestMessage(_)
        )));
        assert_eq!(
            unique_terminal_facts(&replay_port.messages()),
            first_terminal_facts
        );
    });
}

async fn assert_delegated_repair_provider_failure(
    label: &str,
    failure_kind: ProviderStreamFailureKind,
) {
    let root = TestDirectory::new(&format!("production-delegated-repair-{label}"));
    let mut dispatch = dispatch(&root);
    dispatch.job.workspace.write_mode = ExecutionWorkspaceWriteMode::ReadOnly;
    let port = RecordedPort::default();
    let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config_with_mode(
        &root,
        winwincode_codex::ExecutionMode::DelegatedPatch,
    ))
    .expect("open delegated Provider-failure adapter");
    let mut worker = winwincode_worker::WorkerMain::new(
        worker_config(),
        port.clone(),
        adapter,
        root.workspace_runtime(),
    );
    register(&mut worker, &port).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
            at("2030-01-01T00:00:00.000Z"),
        )
        .await
        .expect("accept delegated Provider-failure dispatch");
    let first_open = poll_until_message(
        &mut worker,
        &port,
        &at("2030-01-01T00:00:01.000Z"),
        |messages| {
            messages.iter().find_map(|message| match message {
                ExecutionPortMessage::ModelOpenMessage(open) => Some(open.clone()),
                _ => None,
            })
        },
        "delegated initial request was not delivered",
    )
    .await;
    setup_model(&root, &first_open, &dispatch.job);
    let mut app = application(&root);
    let first_gateway = opened(
        app.accept_local(&typed(ExecutionPortMessage::ModelOpenMessage(
            first_open.clone(),
        )))
        .expect("accept delegated initial ModelOpen"),
    );
    for chunk in provider_chunks(
        &first_open,
        &first_gateway,
        [
            ProviderStreamEvent::ResponseStarted {
                provider_response_id: format!("provider-delegated-invalid-{label}"),
            },
            ProviderStreamEvent::TextStarted { index: 0 },
            ProviderStreamEvent::TextDelta {
                index: 0,
                delta: "not a ChangeBatch proposal".to_owned(),
            },
            ProviderStreamEvent::TextEnded { index: 0 },
            ProviderStreamEvent::Finished(ProviderFinishReason::Stop),
        ],
        1_010,
    ) {
        worker
            .accept_control(
                &ExecutionPortMessage::ModelChunkMessage(chunk),
                at("2030-01-01T00:00:02.000Z"),
            )
            .await
            .expect("deliver initial malformed delegated output");
    }
    let repair_open = poll_until_message(
        &mut worker,
        &port,
        &at("2030-01-01T00:00:02.000Z"),
        |messages| {
            messages
                .iter()
                .filter_map(|message| match message {
                    ExecutionPortMessage::ModelOpenMessage(open) => Some(open.clone()),
                    _ => None,
                })
                .nth(1)
        },
        "delegated repair request was not delivered",
    )
    .await;
    let repair_gateway = opened(
        app.accept_local(&typed(ExecutionPortMessage::ModelOpenMessage(
            repair_open.clone(),
        )))
        .expect("accept delegated repair ModelOpen"),
    );
    for chunk in provider_chunks(
        &repair_open,
        &repair_gateway,
        [ProviderStreamEvent::Failed(ProviderStreamFailure::new(
            failure_kind,
        ))],
        1_020,
    ) {
        worker
            .accept_control(
                &ExecutionPortMessage::ModelChunkMessage(chunk),
                at("2030-01-01T00:00:02.000Z"),
            )
            .await
            .expect("deliver delegated repair Provider failure");
    }
    let outcome = poll_until_message(
        &mut worker,
        &port,
        &at("2030-01-01T00:00:02.000Z"),
        |messages| {
            messages.iter().find_map(|message| match message {
                ExecutionPortMessage::JobOutcomeMessage(outcome) => Some(outcome.clone()),
                _ => None,
            })
        },
        "delegated repair Provider failure did not become infrastructure failure",
    )
    .await;
    assert_eq!(
        outcome.outcome.status,
        ExecutionOutcomeStatus::InfrastructureError
    );
    assert_eq!(
        outcome.outcome.summary,
        "delegated format-repair infrastructure failure"
    );
    let error = outcome
        .outcome
        .error
        .as_ref()
        .expect("repair Provider failure code");
    assert_eq!(error.code, ExecutionPortErrorCode::InfrastructureError);
    assert!(error.retryable);
    assert_eq!(
        stored_run_json(&root)["terminal"]["kind"],
        "delegated_repair_infrastructure_failed"
    );
    assert_eq!(
        port.messages()
            .iter()
            .filter(|message| matches!(message, ExecutionPortMessage::ModelOpenMessage(_)))
            .count(),
        2
    );
    assert!(!port.messages().iter().any(|message| matches!(
        message,
        ExecutionPortMessage::ActionEnforcementRequestMessage(_)
    )));

    let first_terminal_facts = unique_terminal_facts(&port.messages());
    worker
        .shutdown(at("2030-01-01T00:00:03.000Z"))
        .await
        .expect("shutdown delegated Provider-failure Worker");
    drop(worker);

    let replay_port = RecordedPort::default();
    let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config_with_mode(
        &root,
        winwincode_codex::ExecutionMode::DelegatedPatch,
    ))
    .expect("reopen delegated Provider-failure adapter");
    let mut replay = winwincode_worker::WorkerMain::new(
        worker_config(),
        replay_port.clone(),
        adapter,
        root.workspace_runtime(),
    );
    register(&mut replay, &replay_port).await;
    replay
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch),
            at("2030-01-01T00:00:00.000Z"),
        )
        .await
        .expect("accept delegated Provider-failure replay dispatch");
    run_until_outcome_without_gateway(&mut replay, &replay_port).await;
    assert!(!replay_port.messages().iter().any(|message| matches!(
        message,
        ExecutionPortMessage::ModelOpenMessage(_)
            | ExecutionPortMessage::ActionEnforcementRequestMessage(_)
    )));
    assert_eq!(
        unique_terminal_facts(&replay_port.messages()),
        first_terminal_facts
    );
}

#[test]
fn delegated_repair_provider_failures_are_retryable_infrastructure_and_replay_exactly() {
    run_on_large_stack(async {
        for (label, failure_kind) in [
            ("capability", ProviderStreamFailureKind::InvalidRequest),
            ("rate-limit", ProviderStreamFailureKind::RateLimit),
            ("timeout", ProviderStreamFailureKind::Timeout),
            ("transport", ProviderStreamFailureKind::Transport),
        ] {
            assert_delegated_repair_provider_failure(label, failure_kind).await;
        }
    });
}

#[cfg(feature = "test-support")]
#[test]
fn delegated_repair_kernel_recovery_failure_is_retryable_without_an_extra_provider_call() {
    use winwincode_codex::ProductionFormatRepairFault;

    run_on_large_stack(async {
        let root = TestDirectory::new("production-delegated-repair-kernel-recovery");
        let mut dispatch = dispatch(&root);
        dispatch.job.workspace.write_mode = ExecutionWorkspaceWriteMode::ReadOnly;
        let port = RecordedPort::default();
        let adapter = winwincode_codex::ProductionCodexAdapter::open(
            adapter_config_with_mode(&root, winwincode_codex::ExecutionMode::DelegatedPatch)
                .with_test_format_repair_fault(ProductionFormatRepairFault::KernelRecoveryFailed),
        )
        .expect("open delegated Kernel-recovery-failure adapter");
        let mut worker = winwincode_worker::WorkerMain::new(
            worker_config(),
            port.clone(),
            adapter,
            root.workspace_runtime(),
        );
        register(&mut worker, &port).await;
        worker
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
                at("2030-01-01T00:00:00.000Z"),
            )
            .await
            .expect("accept delegated Kernel-recovery-failure dispatch");
        let first_open = poll_until_message(
            &mut worker,
            &port,
            &at("2030-01-01T00:00:01.000Z"),
            |messages| {
                messages.iter().find_map(|message| match message {
                    ExecutionPortMessage::ModelOpenMessage(open) => Some(open.clone()),
                    _ => None,
                })
            },
            "delegated initial request was not delivered",
        )
        .await;
        setup_model(&root, &first_open, &dispatch.job);
        let mut app = application(&root);
        let gateway = opened(
            app.accept_local(&typed(ExecutionPortMessage::ModelOpenMessage(
                first_open.clone(),
            )))
            .expect("accept delegated initial ModelOpen"),
        );
        for chunk in provider_chunks(
            &first_open,
            &gateway,
            [
                ProviderStreamEvent::ResponseStarted {
                    provider_response_id: "provider-delegated-kernel-recovery".to_owned(),
                },
                ProviderStreamEvent::TextStarted { index: 0 },
                ProviderStreamEvent::TextDelta {
                    index: 0,
                    delta: "not a ChangeBatch proposal".to_owned(),
                },
                ProviderStreamEvent::TextEnded { index: 0 },
                ProviderStreamEvent::Finished(ProviderFinishReason::Stop),
            ],
            1_030,
        ) {
            worker
                .accept_control(
                    &ExecutionPortMessage::ModelChunkMessage(chunk),
                    at("2030-01-01T00:00:02.000Z"),
                )
                .await
                .expect("deliver malformed delegated output before Kernel recovery failure");
        }
        let outcome = poll_until_message(
            &mut worker,
            &port,
            &at("2030-01-01T00:00:02.000Z"),
            |messages| {
                messages.iter().find_map(|message| match message {
                    ExecutionPortMessage::JobOutcomeMessage(outcome) => Some(outcome.clone()),
                    _ => None,
                })
            },
            "delegated repair Kernel recovery failure did not terminalize",
        )
        .await;
        assert_eq!(
            outcome.outcome.status,
            ExecutionOutcomeStatus::InfrastructureError
        );
        let error = outcome
            .outcome
            .error
            .as_ref()
            .expect("Kernel recovery failure code");
        assert_eq!(error.code, ExecutionPortErrorCode::InfrastructureError);
        assert!(error.retryable);
        assert_eq!(
            stored_run_json(&root)["terminal"]["kind"],
            "delegated_repair_infrastructure_failed"
        );
        assert_eq!(
            port.messages()
                .iter()
                .filter(|message| matches!(message, ExecutionPortMessage::ModelOpenMessage(_)))
                .count(),
            1,
            "failed repair reconciliation must not open another Provider exchange"
        );

        let first_terminal_facts = unique_terminal_facts(&port.messages());
        worker
            .shutdown(at("2030-01-01T00:00:03.000Z"))
            .await
            .expect("shutdown delegated Kernel-recovery-failure Worker");
        drop(worker);

        let replay_port = RecordedPort::default();
        let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config_with_mode(
            &root,
            winwincode_codex::ExecutionMode::DelegatedPatch,
        ))
        .expect("reopen delegated Kernel-recovery-failure adapter");
        let mut replay = winwincode_worker::WorkerMain::new(
            worker_config(),
            replay_port.clone(),
            adapter,
            root.workspace_runtime(),
        );
        register(&mut replay, &replay_port).await;
        replay
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch),
                at("2030-01-01T00:00:00.000Z"),
            )
            .await
            .expect("accept delegated Kernel-recovery-failure replay dispatch");
        run_until_outcome_without_gateway(&mut replay, &replay_port).await;
        assert!(
            !replay_port
                .messages()
                .iter()
                .any(|message| matches!(message, ExecutionPortMessage::ModelOpenMessage(_)))
        );
        assert_eq!(
            unique_terminal_facts(&replay_port.messages()),
            first_terminal_facts
        );
    });
}

async fn register(
    worker: &mut winwincode_worker::WorkerMain<
        RecordedPort,
        winwincode_codex::ProductionCodexAdapter,
    >,
    port: &RecordedPort,
) {
    worker
        .start(at("2030-01-01T00:00:00.000Z"))
        .await
        .expect("send Worker registration");
    let request_id = port
        .messages()
        .into_iter()
        .rev()
        .find_map(|message| match message {
            ExecutionPortMessage::WorkerRegisterMessage(register) => Some(register.request_id),
            _ => None,
        })
        .expect("Worker registration request");
    let registration = WorkerRegistrationResultMessage {
        error: None,
        heartbeat_interval_ms: 2_000,
        kind: WorkerRegistrationResultMessageKind::WorkerRegistrationResult,
        lease_recovery: WorkerRegistrationResultMessageLeaseRecovery::NoActiveLeases,
        message_id: ExecutionMessageId(id("xmsg", 81)),
        request_id,
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: at("2030-01-01T00:00:00.000Z"),
        server_time: at("2030-01-01T00:00:00.000Z"),
        status: WorkerRegistrationResultMessageStatus::Accepted,
        worker_id: WorkerId(id("wrk", 1)),
        worker_instance_id: WorkerInstanceId(id("wki", 1)),
    };
    worker
        .accept_control(
            &ExecutionPortMessage::WorkerRegistrationResultMessage(registration),
            at("2030-01-01T00:00:00.000Z"),
        )
        .await
        .expect("accept Worker registration");
}

struct GatewayDriver<'root> {
    root: &'root TestDirectory,
    job: ExecutionJob,
    app: Option<StandaloneModelExecutionApplication>,
    control_plane: Option<ControlPlane>,
    runtime_storage: Option<SqliteStorage>,
    pending_worker_facts: Vec<ExecutionPortMessage>,
    cursor: usize,
    open_count: usize,
    idempotent_replays: usize,
    complete: bool,
}

impl<'root> GatewayDriver<'root> {
    fn new(root: &'root TestDirectory, job: ExecutionJob, complete: bool) -> Self {
        Self {
            root,
            job,
            app: None,
            control_plane: None,
            runtime_storage: None,
            pending_worker_facts: Vec::new(),
            cursor: 0,
            open_count: 0,
            idempotent_replays: 0,
            complete,
        }
    }

    async fn drive(
        &mut self,
        port: &RecordedPort,
        worker: &mut winwincode_worker::WorkerMain<
            RecordedPort,
            winwincode_codex::ProductionCodexAdapter,
        >,
    ) {
        let messages = port.messages();
        let mut chunks = Vec::new();
        let mut responses = Vec::new();
        let mut action_responses = Vec::new();
        let mut artifact_acks = Vec::new();
        if self.app.is_none()
            && let Some(open) = messages[self.cursor..]
                .iter()
                .find_map(|message| match message {
                    ExecutionPortMessage::ModelOpenMessage(open) => Some(open),
                    _ => None,
                })
        {
            setup(self.root, open, &self.job);
            self.app = Some(application(self.root));
            self.control_plane = Some(
                ControlPlane::start_local(
                    ControlPlaneConfig::local(self.root.data()),
                    Box::new(DiscardingPublisher),
                )
                .expect("start runtime Control Plane"),
            );
            self.runtime_storage =
                Some(SqliteStorage::open(self.root.data()).expect("open runtime storage"));
        }
        for message in &messages[self.cursor..] {
            match message {
                ExecutionPortMessage::ModelOpenMessage(_) => {
                    self.open_count += 1;
                    let (receipt, idempotent_replay) = opened_with_replay(
                        self.app
                            .as_mut()
                            .expect("model application")
                            .accept_local(&typed(message.clone()))
                            .unwrap_or_else(|error| {
                                panic!(
                                    "accept production ModelOpen attempt={}: {error:?}",
                                    self.open_count
                                )
                            }),
                    );
                    self.idempotent_replays += usize::from(idempotent_replay);
                    if self.complete {
                        chunks.extend(
                            self.app
                                .as_mut()
                                .expect("model application")
                                .complete_loopback(&receipt, &at("2030-01-01T00:00:02.000Z"))
                                .expect("complete deterministic Provider loopback")
                                .chunks,
                        );
                    }
                }
                ExecutionPortMessage::ModelAckMessage(_) => {
                    self.app
                        .as_mut()
                        .expect("model application before acknowledgement")
                        .accept_local(&typed(message.clone()))
                        .expect("accept production ModelAck");
                }
                ExecutionPortMessage::JobDispatchResultMessage(_)
                | ExecutionPortMessage::SessionBindingMessage(_)
                | ExecutionPortMessage::RuntimeEventMessage(_)
                | ExecutionPortMessage::JobOutcomeMessage(_) => {
                    self.pending_worker_facts.push(message.clone());
                }
                ExecutionPortMessage::ApprovalRequestMessage(request) => {
                    let decided_at = at("2030-01-01T00:00:02.000Z");
                    action_responses.push(ExecutionPortMessage::ApprovalDecisionMessage(
                        ApprovalDecisionMessage {
                            approval_id: request.approval_id.clone(),
                            decided_at: decided_at.clone(),
                            decision: ApprovalDecisionMessageDecision::Approved,
                            kind: ApprovalDecisionMessageKind::ApprovalDecision,
                            lease: request.lease.clone(),
                            message_id: ExecutionMessageId(id("xmsg", 930)),
                            reason: None,
                            schema_version: SchemaVersion::WinwincodeV1,
                            scope: ApprovalDecisionMessageScope::Once,
                            sent_at: decided_at,
                            session_identity: request.session_identity.clone(),
                            worker_session_id: request.worker_session_id.clone(),
                        },
                    ));
                }
                ExecutionPortMessage::ActionEnforcementRequestMessage(request) => {
                    let decided_at = at("2030-01-01T00:00:02.000Z");
                    let mut receipt = ActionEnforcementReceiptMessage {
                        actor: UserActor {
                            id: UserId(id("usr", 9)),
                            kind: UserActorKind::User,
                        },
                        decision: ActionEnforcementDecision::Permit,
                        evaluated_at: decided_at.clone(),
                        evaluation_sha256: digest('e'),
                        job_id: request.job_id.clone(),
                        kind: ActionEnforcementReceiptMessageKind::ActionEnforcementReceipt,
                        lease: request.lease.clone(),
                        matched_condition_sha256: request.matched_condition_sha256.clone(),
                        message_id: ExecutionMessageId(id("xmsg", 931)),
                        policy_kind: request.policy_kind.clone(),
                        policy_mode: None,
                        policy_version: None,
                        receipt_signature: digest('0'),
                        request_id: request.request_id.clone(),
                        resource: request.resource.clone(),
                        schema_version: SchemaVersion::WinwincodeV1,
                        scope: repository_scope(),
                        sent_at: decided_at.clone(),
                        session_identity: request.session_identity.clone(),
                        subject_sha256: request.subject_sha256.clone(),
                        worker_session_id: request.worker_session_id.clone(),
                    };
                    ActionEnforcementIssuer::new(action_signing_key())
                        .sign(&mut receipt)
                        .expect("sign loopback action permit");
                    action_responses.push(ExecutionPortMessage::ActionEnforcementReceiptMessage(
                        receipt,
                    ));
                }
                ExecutionPortMessage::ArtifactOpenMessage(open)
                    if open.artifact.kind == ArtifactKind::Candidate =>
                {
                    let active = worker
                        .active_jobs()
                        .into_iter()
                        .find(|active| active.job.job_id == open.lease.job_id)
                        .cloned()
                        .expect("candidate artifact has an active candidate-producing Job");
                    let artifact = ArtifactReference {
                        artifact_id: open.artifact.artifact_id.clone(),
                        digest: open.artifact.digest.clone(),
                    };
                    artifact_acks.push(candidate_ack(&active, &artifact, 0));
                }
                ExecutionPortMessage::ArtifactChunkMessage(chunk) => {
                    let artifact = messages.iter().find_map(|message| match message {
                        ExecutionPortMessage::ArtifactOpenMessage(open)
                            if open.artifact.artifact_id == chunk.artifact_id
                                && open.artifact.kind == ArtifactKind::Candidate =>
                        {
                            Some(ArtifactReference {
                                artifact_id: open.artifact.artifact_id.clone(),
                                digest: open.artifact.digest.clone(),
                            })
                        }
                        _ => None,
                    });
                    if let Some(artifact) = artifact {
                        let active = worker
                            .active_jobs()
                            .into_iter()
                            .find(|active| active.job.job_id == chunk.lease.job_id)
                            .cloned()
                            .expect(
                                "candidate artifact chunk has an active candidate-producing Job",
                            );
                        artifact_acks.push(candidate_ack(&active, &artifact, chunk.sequence.0));
                    }
                }
                _ => {}
            }
        }
        self.cursor = messages.len();
        if let (Some(control_plane), Some(runtime_storage)) =
            (&mut self.control_plane, &mut self.runtime_storage)
        {
            for message in self.pending_worker_facts.drain(..) {
                let fact_kind = match &message {
                    ExecutionPortMessage::JobDispatchResultMessage(_) => "job.dispatch_result",
                    ExecutionPortMessage::SessionBindingMessage(_) => "session.binding",
                    ExecutionPortMessage::RuntimeEventMessage(_) => "runtime.event",
                    ExecutionPortMessage::JobOutcomeMessage(_) => "job.outcome",
                    _ => unreachable!("pending Worker fact is a closed set"),
                };
                responses.extend(
                    DurableExecutionPortIngress::new(
                        control_plane,
                        runtime_storage,
                        &repository_scope(),
                        at("2030-01-01T00:00:02.000Z"),
                    )
                    .expect("compose durable ExecutionPort ingress")
                    .handle(&message)
                    .unwrap_or_else(|error| {
                        panic!("accept durable Worker fact kind={fact_kind}: {error:?}")
                    }),
                );
            }
        }
        for response in responses {
            worker
                .accept_control(&response, at("2030-01-01T00:00:02.000Z"))
                .await
                .expect("accept durable Control Plane response");
        }
        for chunk in chunks {
            worker
                .accept_control(
                    &ExecutionPortMessage::ModelChunkMessage(chunk),
                    at("2030-01-01T00:00:02.000Z"),
                )
                .await
                .expect("deliver canonical Provider chunk to Worker");
        }
        for response in action_responses {
            worker
                .accept_control(&response, at("2030-01-01T00:00:02.000Z"))
                .await
                .expect("accept loopback approval/action response");
        }
        for acknowledgement in artifact_acks {
            worker
                .accept_control(
                    &ExecutionPortMessage::ArtifactAckMessage(acknowledgement),
                    at("2030-01-01T00:00:02.000Z"),
                )
                .await
                .expect("accept loopback candidate artifact acknowledgement");
        }
    }
}

impl Drop for GatewayDriver<'_> {
    fn drop(&mut self) {
        self.runtime_storage.take();
        if let Some(control_plane) = self.control_plane.take() {
            control_plane
                .shutdown()
                .expect("shutdown runtime Control Plane");
        }
    }
}

struct DiscardingPublisher;

impl EventPublisher for DiscardingPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Ok(())
    }
}

async fn run_until_outcome(
    worker: &mut winwincode_worker::WorkerMain<
        RecordedPort,
        winwincode_codex::ProductionCodexAdapter,
    >,
    port: &RecordedPort,
    driver: &mut GatewayDriver<'_>,
) {
    for _ in 0..400 {
        worker
            .poll_codex(at("2030-01-01T00:00:02.000Z"))
            .await
            .expect("poll embedded production Codex");
        driver.drive(port, worker).await;
        if port
            .messages()
            .iter()
            .any(|message| matches!(message, ExecutionPortMessage::JobOutcomeMessage(_)))
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("embedded production Codex turn did not reach a terminal outcome");
}

async fn run_until_outcome_without_gateway(
    worker: &mut winwincode_worker::WorkerMain<
        RecordedPort,
        winwincode_codex::ProductionCodexAdapter,
    >,
    port: &RecordedPort,
) {
    for _ in 0..40 {
        worker
            .poll_codex(at("2030-01-01T00:00:02.000Z"))
            .await
            .expect("poll injected production infrastructure failure");
        if port
            .messages()
            .iter()
            .any(|message| matches!(message, ExecutionPortMessage::JobOutcomeMessage(_)))
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("injected production infrastructure failure did not retain an outcome");
}

async fn poll_until_message<T>(
    worker: &mut winwincode_worker::WorkerMain<
        RecordedPort,
        winwincode_codex::ProductionCodexAdapter,
    >,
    port: &RecordedPort,
    now: &Instant,
    mut select: impl FnMut(&[ExecutionPortMessage]) -> Option<T>,
    context: &'static str,
) -> T {
    for _ in 0..400 {
        worker
            .poll_codex(now.clone())
            .await
            .expect("poll embedded production Codex");
        let messages = port.messages();
        if let Some(value) = select(&messages) {
            return value;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("{context}");
}

async fn poll_until_candidate_outcome(
    worker: &mut winwincode_worker::WorkerMain<
        RecordedPort,
        winwincode_codex::ProductionCodexAdapter,
    >,
    port: &RecordedPort,
    now: &Instant,
) -> winwincode_execution_port::generated::JobOutcomeMessage {
    let mut acknowledged_frames = Vec::<String>::new();
    for _ in 0..400 {
        worker
            .poll_codex(now.clone())
            .await
            .expect("poll embedded production Codex");
        let messages = port.messages();
        let mut acknowledgements = Vec::new();
        for message in &messages {
            let (frame_id, artifact, job_id, sequence) = match message {
                ExecutionPortMessage::ArtifactOpenMessage(open)
                    if open.artifact.kind == ArtifactKind::Candidate =>
                {
                    (
                        open.message_id.0.clone(),
                        ArtifactReference {
                            artifact_id: open.artifact.artifact_id.clone(),
                            digest: open.artifact.digest.clone(),
                        },
                        open.lease.job_id.clone(),
                        0,
                    )
                }
                ExecutionPortMessage::ArtifactChunkMessage(chunk) => {
                    let Some(artifact) = messages.iter().find_map(|candidate| match candidate {
                        ExecutionPortMessage::ArtifactOpenMessage(open)
                            if open.artifact.artifact_id == chunk.artifact_id
                                && open.artifact.kind == ArtifactKind::Candidate =>
                        {
                            Some(ArtifactReference {
                                artifact_id: open.artifact.artifact_id.clone(),
                                digest: open.artifact.digest.clone(),
                            })
                        }
                        _ => None,
                    }) else {
                        continue;
                    };
                    (
                        chunk.message_id.0.clone(),
                        artifact,
                        chunk.lease.job_id.clone(),
                        chunk.sequence.0,
                    )
                }
                _ => continue,
            };
            if acknowledged_frames.iter().any(|id| id == &frame_id) {
                continue;
            }
            let active = worker
                .active_jobs()
                .into_iter()
                .find(|active| active.job.job_id == job_id)
                .cloned()
                .expect("candidate artifact has an active candidate-producing Job");
            acknowledgements.push((frame_id, candidate_ack(&active, &artifact, sequence)));
        }
        for (frame_id, acknowledgement) in acknowledgements {
            worker
                .accept_control(
                    &ExecutionPortMessage::ArtifactAckMessage(acknowledgement),
                    now.clone(),
                )
                .await
                .expect("accept loopback candidate artifact acknowledgement");
            acknowledged_frames.push(frame_id);
        }
        if let Some(outcome) = port
            .messages()
            .into_iter()
            .find_map(|message| match message {
                ExecutionPortMessage::JobOutcomeMessage(outcome) => Some(outcome),
                _ => None,
            })
        {
            return outcome;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("verification stage did not produce a terminal outcome");
}

fn candidate_ack(
    active: &winwincode_worker::ActiveJob,
    artifact: &ArtifactReference,
    sequence: i64,
) -> ArtifactAckMessage {
    ArtifactAckMessage {
        ack_sequence: winwincode_domain::ExecutionAckSequence(sequence),
        artifact_id: artifact.artifact_id.clone(),
        error: None,
        kind: ArtifactAckMessageKind::ArtifactAck,
        lease: active.lease.clone(),
        message_id: ExecutionMessageId(id(
            "xmsg",
            910_u64 + u64::try_from(sequence).expect("non-negative candidate ACK sequence"),
        )),
        replay_from_sequence: None,
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: at("2030-01-01T00:00:02.000Z"),
        session_identity: active.session_identity.clone(),
        status: LeaseWriteStatus::Accepted,
        worker_session_id: active.worker_session_id.clone(),
    }
}

fn runtime_payload(event: &RuntimeEventMessage) -> Option<serde_json::Value> {
    let payload = event.event.payload.as_ref()?;
    let bytes = STANDARD.decode(&payload.data_base64).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn assert_verification_products(messages: &[ExecutionPortMessage], role: &str, call_id: &str) {
    let runtime_events = messages
        .iter()
        .filter_map(|message| match message {
            ExecutionPortMessage::RuntimeEventMessage(event) => Some(event),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        runtime_events.iter().any(|event| {
            event.event.category == ExecutionEventCategory::Lifecycle
                && runtime_payload(event).is_some_and(|payload| {
                    payload["protocol"] == "winwincode.verification-session-policy.v1"
                })
        }),
        "{role} must retain its read-only policy before command/test evidence"
    );
    let expected_category = if role == "reviewer" {
        ExecutionEventCategory::Command
    } else {
        ExecutionEventCategory::Test
    };
    assert!(
        runtime_events
            .iter()
            .any(|event| event.event.category == expected_category),
        "{role} must retain direct command/test evidence"
    );
    let result = runtime_events.iter().find_map(|event| {
        (event.event.category == ExecutionEventCategory::Activity).then(|| {
            runtime_payload(event).filter(|payload| {
                payload["protocol"] == "winwincode.independent-verification-result.v1"
            })
        })
    });
    let result = result.flatten().expect("verification result Activity");
    let event_id = result["findings"][0]["evidence_sources"][0]["event_id"]
        .as_str()
        .expect("verification result binds the evidence event identity");
    assert!(!event_id.is_empty());
    assert_ne!(
        event_id, call_id,
        "result must cite the durable event, not call prose"
    );
}

async fn run_verification_stage(root: &TestDirectory, role: &str, command: &str) {
    let dispatch = verification_dispatch(root, role);
    let port = RecordedPort::default();
    let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config(root))
        .expect("open verification production adapter");
    let mut worker = winwincode_worker::WorkerMain::new(
        worker_config(),
        port.clone(),
        adapter,
        root.workspace_runtime(),
    );
    register(&mut worker, &port).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
            at("2030-01-01T00:00:00.000Z"),
        )
        .await
        .expect("accept verification dispatch");
    let checkout = detached_checkout(root);
    let first_open = poll_until_message(
        &mut worker,
        &port,
        &at("2030-01-01T00:00:01.000Z"),
        |messages| {
            messages.iter().find_map(|message| match message {
                ExecutionPortMessage::ModelOpenMessage(open) => Some(open.clone()),
                _ => None,
            })
        },
        "verification model request was not delivered",
    )
    .await;
    let (source, pending) = pending_verification_execution(&root.source_revision(), role);
    assert_eq!(
        pending.job(),
        &dispatch.job,
        "verification dispatch must be canonical"
    );
    seed_pending_delivery_job(root, &source, &pending);
    setup_model(root, &first_open, &dispatch.job);
    let mut app = application(root);
    let first_gateway = opened(
        app.accept_local(&typed(ExecutionPortMessage::ModelOpenMessage(
            first_open.clone(),
        )))
        .expect("accept verification tool-call ModelOpen"),
    );
    let identity = ProviderToolIdentity::try_new(
        ProviderToolKind::Function,
        "shell_command".to_owned(),
        Some("functions".to_owned()),
    )
    .expect("canonical verification shell tool");
    let usage = ProviderTokenUsage {
        input_tokens: 10,
        cached_input_tokens: 0,
        cache_write_input_tokens: 0,
        output_tokens: 5,
        reasoning_output_tokens: 0,
    };
    let call_id = format!("provider-{role}-evidence-call");
    let tool_chunks = provider_chunks(
        &first_open,
        &first_gateway,
        [
            ProviderStreamEvent::ResponseStarted {
                provider_response_id: format!("provider-{role}-response-1"),
            },
            ProviderStreamEvent::ToolCallStarted {
                index: 0,
                provider_call_id: call_id.clone(),
                identity,
            },
            ProviderStreamEvent::ToolCallArgumentsDelta {
                index: 0,
                provider_call_id: call_id.clone(),
                delta: serde_json::json!({
                    "command": command,
                    "workdir": checkout.to_string_lossy(),
                    "justification": "collect direct verification evidence",
                    "sandbox_permissions": "require_escalated"
                })
                .to_string(),
            },
            ProviderStreamEvent::ToolCallEnded {
                index: 0,
                provider_call_id: call_id.clone(),
            },
            ProviderStreamEvent::Usage(usage),
            ProviderStreamEvent::Finished(ProviderFinishReason::ToolCalls),
        ],
        400,
    );
    for chunk in tool_chunks {
        worker
            .accept_control(
                &ExecutionPortMessage::ModelChunkMessage(chunk),
                at("2030-01-01T00:00:02.000Z"),
            )
            .await
            .expect("deliver verification tool-call chunk");
    }
    let approval = poll_until_message(
        &mut worker,
        &port,
        &at("2030-01-01T00:00:02.000Z"),
        |messages| {
            messages.iter().find_map(|message| match message {
                ExecutionPortMessage::ApprovalRequestMessage(request) => Some(request.clone()),
                _ => None,
            })
        },
        "verification command approval was not delivered",
    )
    .await;
    let decided_at = at("2030-01-01T00:00:02.000Z");
    worker
        .accept_control(
            &ExecutionPortMessage::ApprovalDecisionMessage(ApprovalDecisionMessage {
                approval_id: approval.approval_id.clone(),
                decided_at: decided_at.clone(),
                decision: ApprovalDecisionMessageDecision::Approved,
                kind: ApprovalDecisionMessageKind::ApprovalDecision,
                lease: approval.lease.clone(),
                message_id: ExecutionMessageId(id("xmsg", 920)),
                reason: None,
                schema_version: SchemaVersion::WinwincodeV1,
                scope: ApprovalDecisionMessageScope::Once,
                sent_at: decided_at.clone(),
                session_identity: approval.session_identity.clone(),
                worker_session_id: approval.worker_session_id.clone(),
            }),
            decided_at.clone(),
        )
        .await
        .expect("approve exact verification command");
    let action = poll_until_message(
        &mut worker,
        &port,
        &decided_at,
        |messages| {
            messages.iter().find_map(|message| match message {
                ExecutionPortMessage::ActionEnforcementRequestMessage(request) => {
                    Some(request.clone())
                }
                _ => None,
            })
        },
        "verification action request was not delivered after approval",
    )
    .await;
    let mut receipt = ActionEnforcementReceiptMessage {
        actor: UserActor {
            id: UserId(id("usr", 9)),
            kind: UserActorKind::User,
        },
        decision: ActionEnforcementDecision::Permit,
        evaluated_at: decided_at.clone(),
        evaluation_sha256: digest('e'),
        job_id: action.job_id,
        kind: ActionEnforcementReceiptMessageKind::ActionEnforcementReceipt,
        lease: action.lease,
        matched_condition_sha256: action.matched_condition_sha256,
        message_id: ExecutionMessageId(id("xmsg", 921)),
        policy_kind: action.policy_kind,
        policy_mode: None,
        policy_version: None,
        receipt_signature: digest('0'),
        request_id: action.request_id,
        resource: action.resource,
        schema_version: SchemaVersion::WinwincodeV1,
        scope: repository_scope(),
        sent_at: decided_at.clone(),
        session_identity: action.session_identity,
        subject_sha256: action.subject_sha256,
        worker_session_id: action.worker_session_id,
    };
    ActionEnforcementIssuer::new(action_signing_key())
        .sign(&mut receipt)
        .expect("sign verification action permit");
    worker
        .accept_control(
            &ExecutionPortMessage::ActionEnforcementReceiptMessage(receipt),
            decided_at.clone(),
        )
        .await
        .expect("accept verification action permit");
    let second_open = poll_until_message(
        &mut worker,
        &port,
        &decided_at,
        |messages| {
            messages
                .iter()
                .filter_map(|message| match message {
                    ExecutionPortMessage::ModelOpenMessage(open) => Some(open.clone()),
                    _ => None,
                })
                .nth(1)
        },
        "verification action did not enter its final model turn",
    )
    .await;
    let input = dispatch
        .job
        .stage_input
        .as_ref()
        .expect("verification stage input");
    let task = input.task.as_ref().expect("verification task input");
    let evidence_type = if role == "reviewer" {
        "command"
    } else {
        "test"
    };
    // The adapter accepts only the exact field order emitted by the
    // canonical verification-result serializer.  serde_json::json! stores
    // object keys in sorted order in this build, so build the fixture with
    // escaped scalar values while retaining the protocol's field order.
    let final_message = format!(
        "{{\"protocol\":\"winwincode.independent-verification-result.v1\",\"delivery_spec_id\":{},\"delivery_spec_revision\":{},\"candidate_ref\":{},\"findings\":[{{\"finding_id\":{},\"criterion_id\":{},\"verdict\":\"pass\",\"explanation\":{},\"evidence_sources\":[{{\"type\":\"{}\",\"source_id\":{}}}]}}]}}",
        serde_json::to_string(&input.delivery_spec_id).expect("verification spec JSON"),
        input.delivery_spec_revision,
        serde_json::to_string(&input.candidate_ref).expect("verification candidate JSON"),
        serde_json::to_string(&format!("finding-{role}-fixture"))
            .expect("verification finding JSON"),
        serde_json::to_string(&task.acceptance_criterion_ids[0])
            .expect("verification criterion JSON"),
        serde_json::to_string(&format!("{role} observed the exact direct evidence."))
            .expect("verification explanation JSON"),
        evidence_type,
        serde_json::to_string(&call_id).expect("verification source JSON"),
    );
    let second_gateway = opened(
        app.accept_local(&typed(ExecutionPortMessage::ModelOpenMessage(
            second_open.clone(),
        )))
        .expect("accept verification result ModelOpen"),
    );
    for chunk in provider_chunks(
        &second_open,
        &second_gateway,
        [
            ProviderStreamEvent::ResponseStarted {
                provider_response_id: format!("provider-{role}-response-2"),
            },
            ProviderStreamEvent::TextStarted { index: 0 },
            ProviderStreamEvent::TextDelta {
                index: 0,
                delta: final_message,
            },
            ProviderStreamEvent::TextEnded { index: 0 },
            ProviderStreamEvent::Usage(usage),
            ProviderStreamEvent::Finished(ProviderFinishReason::Stop),
        ],
        500,
    ) {
        worker
            .accept_control(
                &ExecutionPortMessage::ModelChunkMessage(chunk),
                decided_at.clone(),
            )
            .await
            .expect("deliver canonical verification result");
    }
    let outcome = poll_until_candidate_outcome(&mut worker, &port, &decided_at).await;
    assert_eq!(outcome.outcome.status, ExecutionOutcomeStatus::Succeeded);
    assert_eq!(outcome.outcome.artifacts.len(), 1);
    assert_verification_products(&port.messages(), role, &call_id);
    worker
        .shutdown(at("2030-01-01T00:00:03.000Z"))
        .await
        .expect("shutdown verification Worker");
}

#[test]
fn production_worker_reviewer_and_verifier_emit_bound_stage_products() {
    run_on_large_stack(async {
        let reviewer_root = TestDirectory::new("production-reviewer-stage");
        run_verification_stage(&reviewer_root, "reviewer", "git diff --check").await;
        let verifier_root = TestDirectory::new("production-verifier-stage");
        run_verification_stage(&verifier_root, "verifier", "cargo test --quiet").await;
    });
}

fn replay_facts(messages: &[ExecutionPortMessage]) -> Vec<ExecutionPortMessage> {
    messages
        .iter()
        .filter(|message| {
            matches!(
                message,
                ExecutionPortMessage::RuntimeEventMessage(_)
                    | ExecutionPortMessage::JobOutcomeMessage(_)
            )
        })
        .cloned()
        .collect()
}

fn unique_terminal_facts(messages: &[ExecutionPortMessage]) -> Vec<ExecutionPortMessage> {
    messages
        .iter()
        .filter(|message| {
            matches!(
                message,
                ExecutionPortMessage::RuntimeEventMessage(_)
                    | ExecutionPortMessage::JobOutcomeMessage(_)
            )
        })
        .fold(Vec::new(), |mut facts, message| {
            if !facts.contains(message) {
                facts.push(message.clone());
            }
            facts
        })
}

fn simulate_terminal_persisted_before_stopped_trace(root: &TestDirectory) {
    let database = root.worker().join("worker-codex.sqlite3");
    let connection = rusqlite::Connection::open(database).expect("open adapter crash fixture DB");
    let (run_key, run_json): (String, Vec<u8>) = connection
        .query_row(
            "SELECT run_key, record_json FROM codex_run LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("load durable terminal run");
    let mut run: serde_json::Value =
        serde_json::from_slice(&run_json).expect("decode durable terminal run");
    let terminal_event_id = run["terminalTrace"]["eventId"]
        .as_str()
        .expect("durable terminal trace event identity")
        .to_owned();
    run["phase"] = serde_json::Value::String("terminal_trace_pending".to_owned());
    run["terminalTrace"]["retained"] = serde_json::Value::Bool(false);
    connection
        .execute(
            "UPDATE codex_run SET record_json = ?2 WHERE run_key = ?1",
            rusqlite::params![
                run_key,
                serde_json::to_vec(&run).expect("encode crash phase")
            ],
        )
        .expect("persist terminal-before-trace crash phase");

    let (stream_key, replay_json): (String, Vec<u8>) = connection
        .query_row(
            "SELECT stream_key, snapshot_json FROM runtime_replay LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("load runtime replay snapshot");
    let mut replay: serde_json::Value =
        serde_json::from_slice(&replay_json).expect("decode runtime replay snapshot");
    let events = replay["events"]
        .as_array_mut()
        .expect("runtime replay events");
    let before = events.len();
    events.retain(|event| event["eventId"].as_str() != Some(terminal_event_id.as_str()));
    assert_eq!(events.len(), before.saturating_sub(1));
    let highest_sequence = events
        .iter()
        .filter_map(|event| event["sequence"].as_u64())
        .max()
        .unwrap_or(0);
    replay["highestSequence"] = serde_json::Value::from(highest_sequence);
    replay["ackSequence"] = serde_json::Value::from(1_u64);
    connection
        .execute(
            "UPDATE runtime_replay SET snapshot_json = ?2 WHERE stream_key = ?1",
            rusqlite::params![
                stream_key,
                serde_json::to_vec(&replay).expect("encode crash replay")
            ],
        )
        .expect("persist terminal-before-trace replay boundary");
}

fn stored_run_json(root: &TestDirectory) -> serde_json::Value {
    let connection = rusqlite::Connection::open(root.worker().join("worker-codex.sqlite3"))
        .expect("open durable adapter run database");
    let run_json: Vec<u8> = connection
        .query_row("SELECT record_json FROM codex_run LIMIT 1", [], |row| {
            row.get(0)
        })
        .expect("load durable adapter run");
    serde_json::from_slice(&run_json).expect("decode durable adapter run")
}

fn stored_input_operation_json(root: &TestDirectory, input_request_id: &str) -> serde_json::Value {
    let connection = rusqlite::Connection::open(root.worker().join("worker-codex.sqlite3"))
        .expect("open durable adapter input database");
    let (
        run_key,
        kernel_session_id,
        question_id,
        turn_id,
        request_digest,
        resolution_digest,
        state,
    ): (
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        String,
    ) = connection
        .query_row(
            "SELECT run_key, kernel_session_id, question_id, turn_id,
                    request_digest, resolution_digest, state
             FROM input_operation WHERE input_request_id = ?1",
            rusqlite::params![input_request_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .expect("load durable input operation");
    serde_json::json!({
        "runKey": run_key,
        "kernelSessionId": kernel_session_id,
        "questionId": question_id,
        "turnId": turn_id,
        "requestDigest": request_digest,
        "resolutionDigest": resolution_digest,
        "state": state,
    })
}

fn exact_submission_digest(job: &ExecutionJob) -> Sha256Digest {
    let prompt = winwincode_codex::stage_product::stage_product_prompt(job)
        .expect("build canonical stage prompt");
    let mut digest = Sha256::new();
    digest.update(b"winwincode.codex-submission.v2\0");
    digest.update((prompt.len() as u64).to_be_bytes());
    digest.update(prompt.as_bytes());
    let no_schema = serde_json::to_vec(&Option::<serde_json::Value>::None)
        .expect("serialize absent output schema");
    digest.update((no_schema.len() as u64).to_be_bytes());
    digest.update(no_schema);
    Sha256Digest(format!("sha256:{:x}", digest.finalize()))
}

fn exact_rollout_terminal(root: &TestDirectory) -> Option<serde_json::Value> {
    let run = stored_run_json(root);
    let path = run["rolloutPath"].as_str()?;
    let submission_id = run["submissionId"].as_str()?;
    let contents = fs::read_to_string(path).ok()?;
    contents.lines().find_map(|line| {
        let item = serde_json::from_str::<serde_json::Value>(line).ok()?;
        (item["type"] == "event_msg"
            && item["payload"]["type"] == "task_complete"
            && item["payload"]["turn_id"] == submission_id)
            .then(|| item["payload"].clone())
    })
}

fn assert_planner_final_message(message: &serde_json::Value) {
    let text = message
        .as_str()
        .expect("Planner final message is retained as UTF-8 text");
    let product: serde_json::Value =
        serde_json::from_str(text).expect("Planner final message is canonical JSON");
    assert_eq!(
        product["protocol"], "winwincode.planner-solution.v1",
        "deterministic loopback must honor the strict Planner protocol"
    );
    assert_eq!(product["schemaVersion"], 1);
    assert!(
        product["taskProposals"]
            .as_array()
            .is_some_and(|proposals| !proposals.is_empty())
    );
}

#[cfg(feature = "test-support")]
fn rollout_has_exact_turn_start(root: &TestDirectory) -> bool {
    let run = stored_run_json(root);
    let path = run["rolloutPath"]
        .as_str()
        .expect("durable rollout path before submission");
    let submission_id = run["submissionId"]
        .as_str()
        .expect("durable submission identity");
    fs::read_to_string(path).is_ok_and(|contents| {
        contents
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .any(|item| {
                item["type"] == "event_msg"
                    && item["payload"]["type"] == "task_started"
                    && item["payload"]["turn_id"] == submission_id
            })
    })
}

#[cfg(feature = "test-support")]
async fn capture_submission_boundary(
    root: &TestDirectory,
    dispatch: &JobDispatchMessage,
    fault: winwincode_worker::WorkerSubmissionFault,
) {
    use winwincode_codex::ProductionSubmissionFault;

    let config = match fault {
        winwincode_worker::WorkerSubmissionFault::BeforeIntent => adapter_config(root),
        winwincode_worker::WorkerSubmissionFault::AfterIntent => adapter_config(root)
            .with_test_submission_fault(ProductionSubmissionFault::AfterIntentBeforeKernel),
    };
    let port = RecordedPort::default();
    let adapter = winwincode_codex::ProductionCodexAdapter::open(config)
        .expect("open submission-boundary adapter");
    let mut worker = winwincode_worker::WorkerMain::new(
        worker_config(),
        port.clone(),
        adapter,
        root.workspace_runtime(),
    );
    worker.inject_submission_fault(fault);
    register(&mut worker, &port).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
            at("2030-01-01T00:00:00.000Z"),
        )
        .await
        .expect_err("test process must stop at the exact submission boundary");
    assert!(!port.messages().iter().any(|message| matches!(
        message,
        ExecutionPortMessage::ModelOpenMessage(_) | ExecutionPortMessage::ModelAckMessage(_)
    )));
    let run = stored_run_json(root);
    match fault {
        winwincode_worker::WorkerSubmissionFault::BeforeIntent => {
            assert_eq!(run["phase"], "prepared");
            assert!(run["submissionDigest"].is_null());
        }
        winwincode_worker::WorkerSubmissionFault::AfterIntent => {
            assert_eq!(run["phase"], "submission_intent");
            assert_eq!(
                run["submissionDigest"],
                exact_submission_digest(&dispatch.job).0
            );
            let thread_id = worker.active_jobs()[0].codex_thread_id.clone();
            let (_, mut adapter) = worker.into_parts();
            let changed_prompt = format!(
                "{}x",
                winwincode_codex::stage_product::stage_product_prompt(&dispatch.job)
                    .expect("canonical stage prompt")
            );
            adapter
                .submit_turn(&thread_id, &changed_prompt)
                .await
                .expect_err("changed sealed prompt must conflict before Kernel submission");
            assert!(
                adapter
                    .take_execution_messages()
                    .expect("read empty model queue")
                    .is_empty()
            );
            let snapshot = DirectorySnapshot::capture(&root.worker());
            drop(adapter);
            snapshot.restore(&root.worker());
            assert!(!rollout_has_exact_turn_start(root));
            return;
        }
    }
    let snapshot = DirectorySnapshot::capture(&root.worker());
    drop(worker);
    snapshot.restore(&root.worker());
    assert!(!rollout_has_exact_turn_start(root));
}

fn rewrite_exact_rollout_as_failed(root: &TestDirectory) {
    let run = stored_run_json(root);
    let path = PathBuf::from(run["rolloutPath"].as_str().expect("durable rollout path"));
    let submission_id = run["submissionId"]
        .as_str()
        .expect("durable submission identity");
    let contents = fs::read_to_string(&path).expect("read completed rollout for failure fixture");
    let mut changed = false;
    let mut encoded = String::new();
    for line in contents.lines() {
        let mut item: serde_json::Value =
            serde_json::from_str(line).expect("decode canonical rollout line");
        if item["type"] == "event_msg"
            && item["payload"]["type"] == "task_complete"
            && item["payload"]["turn_id"] == submission_id
        {
            item["payload"]["error"] = serde_json::json!({
                "message": "deterministic terminal failure",
                "codex_error_info": null
            });
            item["payload"]["last_agent_message"] = serde_json::Value::Null;
            changed = true;
        }
        encoded.push_str(&serde_json::to_string(&item).expect("encode canonical rollout line"));
        encoded.push('\n');
    }
    assert!(changed);
    fs::write(path, encoded).expect("persist deterministic failed rollout fixture");
}

#[derive(Clone, Copy)]
enum RolloutTerminalFixture {
    Completed,
    Failed,
}

async fn capture_rollout_before_adapter_terminal<'root>(
    root: &'root TestDirectory,
    dispatch: &JobDispatchMessage,
    terminal_fixture: RolloutTerminalFixture,
) -> GatewayDriver<'root> {
    let port = RecordedPort::default();
    let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config(root))
        .expect("open adapter before completed-rollout crash");
    let mut worker = winwincode_worker::WorkerMain::new(
        worker_config(),
        port.clone(),
        adapter,
        root.workspace_runtime(),
    );
    register(&mut worker, &port).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
            at("2030-01-01T00:00:00.000Z"),
        )
        .await
        .expect("accept completed-rollout crash dispatch");
    let mut gateway = GatewayDriver::new(root, dispatch.job.clone(), true);
    for _ in 0..200 {
        worker
            .poll_codex(at("2030-01-01T00:00:01.000Z"))
            .await
            .expect("poll until completed Provider response is delivered");
        gateway.drive(&port, &mut worker).await;
        if gateway.open_count == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(gateway.open_count, 1);
    assert_eq!(gateway.idempotent_replays, 0);
    for _ in 0..400 {
        if exact_rollout_terminal(root).is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    if matches!(terminal_fixture, RolloutTerminalFixture::Failed) {
        rewrite_exact_rollout_as_failed(root);
    }
    let terminal = exact_rollout_terminal(root).expect("exact turn terminal in Core rollout");
    match terminal_fixture {
        RolloutTerminalFixture::Completed => {
            assert!(terminal["error"].is_null());
            assert_planner_final_message(&terminal["last_agent_message"]);
        }
        RolloutTerminalFixture::Failed => assert!(terminal["error"].is_object()),
    }
    let run = stored_run_json(root);
    assert!(matches!(
        run["phase"].as_str(),
        Some("submission_intent" | "runtime_started")
    ));
    assert!(run["terminal"].is_null());
    let crash_snapshot = DirectorySnapshot::capture(&root.worker());
    worker
        .shutdown(at("2030-01-01T00:00:02.000Z"))
        .await
        .expect("quiesce completed-rollout crash runtime");
    drop(worker);
    crash_snapshot.restore(&root.worker());
    gateway.cursor = 0;
    gateway.open_count = 0;
    gateway.idempotent_replays = 0;
    gateway
}

#[test]
fn production_worker_kernel_gateway_loopback_and_restart_replay_are_exact() {
    run_on_large_stack(async {
        let root = TestDirectory::new("production-restart");
        let helper = helper_executable();
        let output = Command::new(&helper)
            .arg("--winwincode-helper-handshake")
            .output()
            .expect("execute project-owned winwincode-kernel-helper");
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        assert_eq!(
            String::from_utf8(output.stdout).expect("helper handshake is UTF-8"),
            format!(
                "{{\"protocol\":\"winwincode-kernel-helper\",\"version\":1,\"packageVersion\":\"{}\"}}\n",
                env!("CARGO_PKG_VERSION")
            )
        );

        let dispatch = planning_dispatch(&root);
        let first_port = RecordedPort::default();
        let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config(&root))
            .expect("open production embedded adapter and Kernel RuntimePaths");
        let mut first = winwincode_worker::WorkerMain::new(
            worker_config(),
            first_port.clone(),
            adapter,
            root.workspace_runtime(),
        );
        register(&mut first, &first_port).await;
        first
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
                at("2030-01-01T00:00:00.000Z"),
            )
            .await
            .expect("accept exact production dispatch");
        let mut gateway = GatewayDriver::new(&root, dispatch.job.clone(), true);
        run_until_outcome(&mut first, &first_port, &mut gateway).await;
        gateway.drive(&first_port, &mut first).await;
        assert_eq!(gateway.open_count, 1);
        let first_messages = first_port.messages();
        assert!(
            first_messages
                .iter()
                .any(|message| { matches!(message, ExecutionPortMessage::ModelOpenMessage(_)) })
        );
        assert!(
            first_messages
                .iter()
                .any(|message| { matches!(message, ExecutionPortMessage::ModelAckMessage(_)) })
        );
        let first_facts = replay_facts(&first_messages);
        let first_terminal_facts = unique_terminal_facts(&first_facts);
        assert_eq!(
            first_terminal_facts
                .iter()
                .filter(|message| matches!(message, ExecutionPortMessage::RuntimeEventMessage(_)))
                .count(),
            4
        );
        let baseline = first_terminal_facts
            .iter()
            .find_map(|message| match message {
                ExecutionPortMessage::RuntimeEventMessage(event)
                    if event.event.category == ExecutionEventCategory::Usage =>
                {
                    runtime_payload(event)
                }
                _ => None,
            })
            .expect("terminal performance baseline Usage event");
        assert_eq!(baseline["fact"]["kind"], "performance_baseline");
        assert_eq!(baseline["fact"]["report"]["executionMode"], "react");
        assert_eq!(baseline["fact"]["report"]["observerMode"], "off");
        assert_eq!(baseline["fact"]["report"]["primaryModelCallCount"], 1);
        assert_eq!(baseline["fact"]["report"]["primaryModelInputTokens"], 10);
        assert_eq!(baseline["fact"]["report"]["primaryModelCachedTokens"], 0);
        assert_eq!(baseline["fact"]["report"]["primaryModelOutputTokens"], 5);
        assert_eq!(baseline["fact"]["report"]["turnCount"], 1);
        let outcome = first_terminal_facts
            .iter()
            .find_map(|message| match message {
                ExecutionPortMessage::JobOutcomeMessage(outcome) => Some(outcome),
                _ => None,
            })
            .expect("successful Worker outcome");
        assert_eq!(outcome.outcome.status, ExecutionOutcomeStatus::Succeeded);
        assert_eq!(
            outcome.outcome.usage.as_ref().map(|usage| usage.tokens),
            Some(15)
        );
        first
            .shutdown(at("2030-01-01T00:00:03.000Z"))
            .await
            .expect("shutdown first embedded Worker");
        drop(first);
        simulate_terminal_persisted_before_stopped_trace(&root);

        let replay_port = RecordedPort::default();
        let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config(&root))
            .expect("reopen production adapter from durable run state");
        let mut replay = winwincode_worker::WorkerMain::new(
            worker_config(),
            replay_port.clone(),
            adapter,
            root.workspace_runtime(),
        );
        register(&mut replay, &replay_port).await;
        replay
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
                at("2030-01-01T00:00:00.000Z"),
            )
            .await
            .expect("accept exact replay dispatch");
        let mut replay_gateway = GatewayDriver::new(&root, dispatch.job, true);
        run_until_outcome(&mut replay, &replay_port, &mut replay_gateway).await;
        assert_eq!(
            replay_gateway.open_count, 0,
            "terminal replay must not reopen Provider"
        );
        let expected_replay = first_terminal_facts
            .into_iter()
            .filter(|message| match message {
                ExecutionPortMessage::RuntimeEventMessage(event) => event.event.sequence.0 > 1,
                ExecutionPortMessage::JobOutcomeMessage(_) => true,
                _ => false,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            unique_terminal_facts(&replay_port.messages()),
            expected_replay
        );
        replay
            .shutdown(at("2030-01-01T00:00:03.000Z"))
            .await
            .expect("shutdown replayed embedded Worker");
    });
}

#[cfg(feature = "test-support")]
#[test]
fn submission_intent_crash_boundaries_restart_with_one_exact_turn() {
    for (label, fault) in [
        (
            "before-intent",
            winwincode_worker::WorkerSubmissionFault::BeforeIntent,
        ),
        (
            "after-intent",
            winwincode_worker::WorkerSubmissionFault::AfterIntent,
        ),
    ] {
        run_on_large_stack(async move {
            let root = TestDirectory::new(&format!("production-submission-{label}"));
            let dispatch = planning_dispatch(&root);
            capture_submission_boundary(&root, &dispatch, fault).await;

            let port = RecordedPort::default();
            let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config(&root))
                .expect("reopen adapter after submission-boundary crash");
            let mut worker = winwincode_worker::WorkerMain::new(
                worker_config(),
                port.clone(),
                adapter,
                root.workspace_runtime(),
            );
            register(&mut worker, &port).await;
            worker
                .accept_control(
                    &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
                    at("2030-01-01T00:00:00.000Z"),
                )
                .await
                .expect("accept exact submission-boundary replay");
            let mut gateway = GatewayDriver::new(&root, dispatch.job, true);
            run_until_outcome(&mut worker, &port, &mut gateway).await;
            assert_eq!(gateway.open_count, 1);
            assert_eq!(gateway.idempotent_replays, 0);
            assert_eq!(
                port.messages()
                    .iter()
                    .filter(|message| matches!(message, ExecutionPortMessage::JobOutcomeMessage(_)))
                    .count(),
                1
            );
            worker
                .shutdown(at("2030-01-01T00:00:03.000Z"))
                .await
                .expect("shutdown submission-boundary replay Worker");
        });
    }
}

#[test]
fn completed_rollout_before_adapter_terminal_restarts_without_provider_work() {
    run_on_large_stack(async {
        let root = TestDirectory::new("production-completed-rollout-crash");
        let dispatch = planning_dispatch(&root);
        let mut gateway = capture_rollout_before_adapter_terminal(
            &root,
            &dispatch,
            RolloutTerminalFixture::Completed,
        )
        .await;
        let replay_port = RecordedPort::default();
        let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config(&root))
            .expect("resume adapter from completed Core rollout");
        let mut replay = winwincode_worker::WorkerMain::new(
            worker_config(),
            replay_port.clone(),
            adapter,
            root.workspace_runtime(),
        );
        register(&mut replay, &replay_port).await;
        replay
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch),
                at("2030-01-01T00:00:00.000Z"),
            )
            .await
            .expect("accept completed-rollout replay dispatch");
        run_until_outcome(&mut replay, &replay_port, &mut gateway).await;
        assert_eq!(gateway.open_count, 0);
        assert_eq!(gateway.idempotent_replays, 0);
        let messages = replay_port.messages();
        let outcomes = messages
            .iter()
            .filter_map(|message| match message {
                ExecutionPortMessage::JobOutcomeMessage(outcome) => Some(outcome),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(
            outcomes[0].outcome.status,
            ExecutionOutcomeStatus::Succeeded
        );
        assert_eq!(
            outcomes[0].outcome.usage.as_ref().map(|usage| usage.tokens),
            Some(15)
        );
        let run = stored_run_json(&root);
        assert_planner_final_message(&run["terminal"]["final_message"]);
        replay
            .shutdown(at("2030-01-01T00:00:03.000Z"))
            .await
            .expect("shutdown completed-rollout replay Worker");
    });
}

#[test]
fn failed_rollout_before_adapter_terminal_restarts_without_provider_work() {
    run_on_large_stack(async {
        let root = TestDirectory::new("production-failed-rollout-crash");
        let dispatch = planning_dispatch(&root);
        let mut gateway = capture_rollout_before_adapter_terminal(
            &root,
            &dispatch,
            RolloutTerminalFixture::Failed,
        )
        .await;
        let replay_port = RecordedPort::default();
        let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config(&root))
            .expect("resume adapter from failed Core rollout");
        let mut replay = winwincode_worker::WorkerMain::new(
            worker_config(),
            replay_port.clone(),
            adapter,
            root.workspace_runtime(),
        );
        register(&mut replay, &replay_port).await;
        replay
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch),
                at("2030-01-01T00:00:00.000Z"),
            )
            .await
            .expect("accept failed-rollout replay dispatch");
        run_until_outcome(&mut replay, &replay_port, &mut gateway).await;
        assert_eq!(gateway.open_count, 0);
        assert_eq!(gateway.idempotent_replays, 0);
        let outcomes = replay_port
            .messages()
            .into_iter()
            .filter_map(|message| match message {
                ExecutionPortMessage::JobOutcomeMessage(outcome) => Some(outcome),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].outcome.status, ExecutionOutcomeStatus::Failed);
        assert!(outcomes[0].outcome.usage.is_none());
        let run = stored_run_json(&root);
        assert_eq!(run["terminal"]["kind"], "failed");
        replay
            .shutdown(at("2030-01-01T00:00:03.000Z"))
            .await
            .expect("shutdown failed-rollout replay Worker");
    });
}

#[cfg(feature = "test-support")]
#[test]
fn production_event_poll_faults_retain_one_terminal_before_restart() {
    use winwincode_codex::ProductionEventPollFault;

    for (label, fault, expected_status, expected_summary) in [
        (
            "closed",
            ProductionEventPollFault::Closed,
            ExecutionOutcomeStatus::InfrastructureError,
            "embedded Codex infrastructure failure",
        ),
        (
            "malformed",
            ProductionEventPollFault::MalformedEvent,
            ExecutionOutcomeStatus::InfrastructureError,
            "embedded Codex infrastructure failure",
        ),
        (
            "kernel-error",
            ProductionEventPollFault::KernelError,
            ExecutionOutcomeStatus::InfrastructureError,
            "embedded Codex infrastructure failure",
        ),
        (
            "error-event",
            ProductionEventPollFault::ErrorEvent,
            ExecutionOutcomeStatus::Failed,
            "embedded Codex turn failed",
        ),
    ] {
        run_on_large_stack(async move {
            let root = TestDirectory::new(&format!("production-event-{label}"));
            let dispatch = planning_dispatch(&root);
            let first_port = RecordedPort::default();
            let adapter = winwincode_codex::ProductionCodexAdapter::open(
                adapter_config(&root).with_test_event_poll_fault(fault),
            )
            .expect("open fault-injected production adapter");
            let mut first = winwincode_worker::WorkerMain::new(
                worker_config(),
                first_port.clone(),
                adapter,
                root.workspace_runtime(),
            );
            register(&mut first, &first_port).await;
            first
                .accept_control(
                    &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
                    at("2030-01-01T00:00:00.000Z"),
                )
                .await
                .expect("accept fault-injected dispatch");
            run_until_outcome_without_gateway(&mut first, &first_port).await;

            let first_messages = first_port.messages();
            assert!(!first_messages.iter().any(|message| matches!(
                message,
                ExecutionPortMessage::ModelOpenMessage(_)
                    | ExecutionPortMessage::ModelAckMessage(_)
            )));
            let first_facts = unique_terminal_facts(&first_messages);
            assert_eq!(first_facts.len(), 3);
            let ExecutionPortMessage::RuntimeEventMessage(baseline) = &first_facts[0] else {
                panic!("durable performance baseline must precede the Stopped trace");
            };
            assert_eq!(baseline.event.sequence, ExecutionSequence(1));
            assert_eq!(baseline.event.category, ExecutionEventCategory::Usage);
            let ExecutionPortMessage::RuntimeEventMessage(stopped) = &first_facts[1] else {
                panic!("durable Stopped trace must precede infrastructure Outcome");
            };
            assert_eq!(stopped.event.sequence, ExecutionSequence(2));
            assert_eq!(stopped.event.summary, expected_summary);
            let ExecutionPortMessage::JobOutcomeMessage(outcome) = &first_facts[2] else {
                panic!("durable infrastructure Outcome must follow Stopped trace");
            };
            assert_eq!(outcome.outcome.status, expected_status);
            assert_eq!(outcome.outcome.last_event_sequence.0, 2);
            assert_eq!(stored_run_json(&root)["phase"], "outcome_retained");
            first
                .shutdown(at("2030-01-01T00:00:03.000Z"))
                .await
                .expect("shutdown fault-injected Worker");
            drop(first);

            let replay_port = RecordedPort::default();
            let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config(&root))
                .expect("reopen infrastructure-terminal adapter");
            let mut replay = winwincode_worker::WorkerMain::new(
                worker_config(),
                replay_port.clone(),
                adapter,
                root.workspace_runtime(),
            );
            register(&mut replay, &replay_port).await;
            replay
                .accept_control(
                    &ExecutionPortMessage::JobDispatchMessage(dispatch),
                    at("2030-01-01T00:00:00.000Z"),
                )
                .await
                .expect("accept infrastructure-terminal replay dispatch");
            run_until_outcome_without_gateway(&mut replay, &replay_port).await;
            let replay_messages = replay_port.messages();
            assert!(!replay_messages.iter().any(|message| matches!(
                message,
                ExecutionPortMessage::ModelOpenMessage(_)
                    | ExecutionPortMessage::ModelAckMessage(_)
            )));
            assert_eq!(unique_terminal_facts(&replay_messages), first_facts);
            drop(replay);
        });
    }
}

#[test]
fn pre_start_cancellation_restarts_with_one_exact_stopped_outcome() {
    run_on_large_stack(async {
        let root = TestDirectory::new("production-pre-start-cancel");
        let dispatch = planning_dispatch(&root);
        let first_port = RecordedPort::default();
        let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config(&root))
            .expect("open pre-start cancellation adapter");
        let mut first = winwincode_worker::WorkerMain::new(
            worker_config(),
            first_port.clone(),
            adapter,
            root.workspace_runtime(),
        );
        register(&mut first, &first_port).await;
        first
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
                at("2030-01-01T00:00:00.000Z"),
            )
            .await
            .expect("accept pre-start cancellation dispatch");
        let active = first.active_jobs()[0].clone();
        let cancelled_at = at("2030-01-01T00:00:01.000Z");
        first
            .accept_control(
                &ExecutionPortMessage::JobCancelMessage(JobCancelMessage {
                    kind: JobCancelMessageKind::JobCancel,
                    lease: active.lease.clone(),
                    message_id: ExecutionMessageId(id("xmsg", 91)),
                    reason: JobCancelMessageReason::UserRequested,
                    requested_at: cancelled_at.clone(),
                    request_id: RequestId(id("req", 91)),
                    schema_version: SchemaVersion::WinwincodeV1,
                    sent_at: cancelled_at.clone(),
                    session_identity: active.session_identity.clone(),
                    worker_session_id: active.worker_session_id.clone(),
                }),
                cancelled_at,
            )
            .await
            .expect("cancel before the first Kernel event is polled");
        run_until_outcome_without_gateway(&mut first, &first_port).await;
        let first_messages = first_port.messages();
        assert!(!first_messages.iter().any(|message| matches!(
            message,
            ExecutionPortMessage::ModelOpenMessage(_) | ExecutionPortMessage::ModelAckMessage(_)
        )));
        let first_facts = unique_terminal_facts(&first_messages);
        assert_eq!(first_facts.len(), 3);
        assert!(matches!(
            &first_facts[0],
            ExecutionPortMessage::RuntimeEventMessage(event)
                if event.event.sequence.0 == 1
                    && event.event.category == ExecutionEventCategory::Usage
        ));
        assert!(matches!(
            &first_facts[1],
            ExecutionPortMessage::RuntimeEventMessage(event)
                if event.event.sequence.0 == 2
                    && event.event.summary == "embedded Codex turn cancelled"
        ));
        assert!(matches!(
            &first_facts[2],
            ExecutionPortMessage::JobOutcomeMessage(outcome)
                if outcome.outcome.status == ExecutionOutcomeStatus::Cancelled
                    && outcome.outcome.last_event_sequence.0 == 2
        ));
        first
            .shutdown(at("2030-01-01T00:00:03.000Z"))
            .await
            .expect("shutdown pre-start cancelled Worker");
        drop(first);

        let replay_port = RecordedPort::default();
        let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config(&root))
            .expect("reopen pre-start cancelled adapter");
        let mut replay = winwincode_worker::WorkerMain::new(
            worker_config(),
            replay_port.clone(),
            adapter,
            root.workspace_runtime(),
        );
        register(&mut replay, &replay_port).await;
        replay
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch),
                at("2030-01-01T00:00:00.000Z"),
            )
            .await
            .expect("accept pre-start cancelled replay dispatch");
        run_until_outcome_without_gateway(&mut replay, &replay_port).await;
        let replay_messages = replay_port.messages();
        assert!(!replay_messages.iter().any(|message| matches!(
            message,
            ExecutionPortMessage::ModelOpenMessage(_) | ExecutionPortMessage::ModelAckMessage(_)
        )));
        assert_eq!(unique_terminal_facts(&replay_messages), first_facts);
        drop(replay);
    });
}

#[test]
fn production_worker_cancellation_closes_the_same_gateway_exchange() {
    run_on_large_stack(async {
        let root = TestDirectory::new("production-cancel");
        let dispatch = dispatch(&root);
        let port = RecordedPort::default();
        let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config(&root))
            .expect("open cancellable production adapter");
        let mut worker = winwincode_worker::WorkerMain::new(
            worker_config(),
            port.clone(),
            adapter,
            root.workspace_runtime(),
        );
        register(&mut worker, &port).await;
        worker
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
                at("2030-01-01T00:00:00.000Z"),
            )
            .await
            .expect("accept cancellable dispatch");
        let mut gateway = GatewayDriver::new(&root, dispatch.job, false);
        for _ in 0..200 {
            worker
                .poll_codex(at("2030-01-01T00:00:01.000Z"))
                .await
                .expect("poll until ModelOpen");
            gateway.drive(&port, &mut worker).await;
            if gateway.open_count == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(gateway.open_count, 1);
        let active = worker
            .active_jobs()
            .first()
            .expect("active cancellable Job")
            .to_owned();
        let cancelled_at = at("2030-01-01T00:00:02.000Z");
        worker
            .accept_control(
                &ExecutionPortMessage::JobCancelMessage(JobCancelMessage {
                    kind: JobCancelMessageKind::JobCancel,
                    lease: active.lease.clone(),
                    message_id: ExecutionMessageId(id("xmsg", 90)),
                    reason: JobCancelMessageReason::UserRequested,
                    requested_at: cancelled_at.clone(),
                    request_id: RequestId(id("req", 90)),
                    schema_version: SchemaVersion::WinwincodeV1,
                    sent_at: cancelled_at.clone(),
                    session_identity: active.session_identity.clone(),
                    worker_session_id: active.worker_session_id.clone(),
                }),
                cancelled_at,
            )
            .await
            .expect("cancel embedded production turn");
        gateway.drive(&port, &mut worker).await;
        run_until_outcome(&mut worker, &port, &mut gateway).await;
        let messages = port.messages();
        assert!(messages.iter().any(|message| {
            matches!(message, ExecutionPortMessage::ModelAckMessage(ack) if ack.error.is_some())
        }));
        let outcome = messages
            .iter()
            .find_map(|message| match message {
                ExecutionPortMessage::JobOutcomeMessage(outcome) => Some(outcome),
                _ => None,
            })
            .expect("cancelled terminal outcome");
        assert_eq!(outcome.outcome.status, ExecutionOutcomeStatus::Cancelled);
        assert!(outcome.outcome.usage.is_none());
        worker
            .shutdown(at("2030-01-01T00:00:03.000Z"))
            .await
            .expect("shutdown cancelled embedded Worker");
    });
}

#[test]
fn submission_crash_reopens_the_exact_provider_exchange_and_finishes_once() {
    run_on_large_stack(async {
        let root = TestDirectory::new("production-submission-crash");
        let dispatch = dispatch(&root);
        let first_port = RecordedPort::default();
        let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config(&root))
            .expect("open pre-crash adapter");
        let mut first = winwincode_worker::WorkerMain::new(
            worker_config(),
            first_port.clone(),
            adapter,
            root.workspace_runtime(),
        );
        register(&mut first, &first_port).await;
        first
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
                at("2030-01-01T00:00:00.000Z"),
            )
            .await
            .expect("accept pre-crash dispatch");
        assert_eq!(
            stored_run_json(&root)["submissionDigest"],
            exact_submission_digest(&dispatch.job).0
        );
        let mut gateway = GatewayDriver::new(&root, dispatch.job.clone(), false);
        for _ in 0..200 {
            first
                .poll_codex(at("2030-01-01T00:00:01.000Z"))
                .await
                .expect("poll pre-crash ModelOpen");
            gateway.drive(&first_port, &mut first).await;
            if gateway.open_count == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let original_open = first_port
            .messages()
            .into_iter()
            .find(|message| matches!(message, ExecutionPortMessage::ModelOpenMessage(_)))
            .expect("pre-crash ModelOpen");
        assert_eq!(gateway.open_count, 1);
        assert_eq!(gateway.idempotent_replays, 0);
        let crash_snapshot = DirectorySnapshot::capture(&root.worker());
        first
            .shutdown(at("2030-01-01T00:00:01.000Z"))
            .await
            .expect("quiesce the in-process pre-crash runtime");
        drop(first);
        crash_snapshot.restore(&root.worker());

        let replay_port = RecordedPort::default();
        let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config(&root))
            .expect("recover adapter after submission crash");
        let mut recovered = winwincode_worker::WorkerMain::new(
            worker_config(),
            replay_port.clone(),
            adapter,
            root.workspace_runtime(),
        );
        register(&mut recovered, &replay_port).await;
        recovered
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
                at("2030-01-01T00:00:00.000Z"),
            )
            .await
            .expect("accept recovered dispatch");
        gateway.cursor = 0;
        gateway.complete = true;
        gateway.open_count = 0;
        gateway.idempotent_replays = 0;
        run_until_outcome(&mut recovered, &replay_port, &mut gateway).await;
        let replay_open = replay_port
            .messages()
            .into_iter()
            .find(|message| matches!(message, ExecutionPortMessage::ModelOpenMessage(_)))
            .expect("recovered ModelOpen");
        assert_eq!(replay_open, original_open);
        let original_exchange_id = match &original_open {
            ExecutionPortMessage::ModelOpenMessage(open) => &open.model_exchange_id,
            _ => unreachable!("the captured pre-crash frame is a ModelOpen"),
        };
        assert_eq!(
            replay_port
                .messages()
                .iter()
                .filter(|message| matches!(
                    message,
                    ExecutionPortMessage::ModelOpenMessage(open)
                        if &open.model_exchange_id == original_exchange_id
                ))
                .count(),
            1,
            "the crash-replayed Provider exchange is opened exactly once"
        );
        assert_eq!(
            gateway.open_count, 2,
            "the completed executor turn opens one continuation exchange after its tool call"
        );
        assert_eq!(gateway.idempotent_replays, 1);
        assert_eq!(
            replay_port
                .messages()
                .iter()
                .filter(|message| matches!(message, ExecutionPortMessage::JobOutcomeMessage(_)))
                .count(),
            1
        );
        recovered
            .shutdown(at("2030-01-01T00:00:03.000Z"))
            .await
            .expect("shutdown recovered Worker");
    });
}

#[test]
fn changed_job_replay_is_rejected_before_kernel_or_provider_work() {
    run_on_large_stack(async {
        let root = TestDirectory::new("production-changed-job-replay");
        let original = dispatch(&root);
        let first_port = RecordedPort::default();
        let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config(&root))
            .expect("open original production adapter");
        let mut first = winwincode_worker::WorkerMain::new(
            worker_config(),
            first_port.clone(),
            adapter,
            root.workspace_runtime(),
        );
        register(&mut first, &first_port).await;
        first
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(original.clone()),
                at("2030-01-01T00:00:00.000Z"),
            )
            .await
            .expect("retain original run identity");
        drop(first);

        let mut changed_jobs = Vec::new();
        let mut changed_goal = original.job.clone();
        changed_goal.goal.push_str(" Changed after dispatch.");
        changed_jobs.push(changed_goal);
        let mut changed_profile = original.job.clone();
        changed_profile.execution_profile = "planner".to_owned();
        changed_jobs.push(changed_profile);
        let mut changed_limits = original.job.clone();
        changed_limits.limits.max_runtime_seconds -= 1;
        changed_jobs.push(changed_limits);
        let mut changed_workspace = original.job.clone();
        changed_workspace.workspace.checkout_revision =
            "1123456789abcdef0123456789abcdef01234567".to_owned();
        changed_jobs.push(changed_workspace);

        for changed_job in changed_jobs {
            assert_eq!(
                changed_job.payload_digest, original.job.payload_digest,
                "the adversarial replay keeps the caller-supplied digest"
            );
            let port = RecordedPort::default();
            let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config(&root))
                .expect("reopen exact durable adapter");
            let mut worker = winwincode_worker::WorkerMain::new(
                worker_config(),
                port.clone(),
                adapter,
                root.workspace_runtime(),
            );
            register(&mut worker, &port).await;
            let mut changed = original.clone();
            changed.job = changed_job;
            worker
                .accept_control(
                    &ExecutionPortMessage::JobDispatchMessage(changed),
                    at("2030-01-01T00:00:01.000Z"),
                )
                .await
                .expect("changed replay must emit a durable conflict result");
            let messages = port.messages();
            let result = messages
                .iter()
                .rev()
                .find_map(|message| match message {
                    ExecutionPortMessage::JobDispatchResultMessage(result) => Some(result),
                    _ => None,
                })
                .expect("changed replay must emit a JobDispatchResult");
            assert_eq!(
                result.status,
                JobDispatchResultMessageStatus::RejectedCapability,
                "production recovery reports changed workspace authority as a rejection"
            );
            assert!(
                !messages.iter().any(|message| {
                    matches!(message, ExecutionPortMessage::ModelOpenMessage(_))
                })
            );
            drop(worker);
        }
    });
}

#[test]
fn real_request_user_input_resumes_after_response_loss_and_rejects_forged_replays() {
    run_on_large_stack(async {
        let root = TestDirectory::new("production-input-response");
        let dispatch = input_dispatch(&root);
        let first_port = RecordedPort::default();
        let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config(&root))
            .expect("open production input adapter");
        let mut first = winwincode_worker::WorkerMain::new(
            worker_config(),
            first_port.clone(),
            adapter,
            root.workspace_runtime(),
        );
        register(&mut first, &first_port).await;
        first
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
                at("2030-01-01T00:00:00.000Z"),
            )
            .await
            .expect("accept input dispatch");

        let first_open = poll_until_message(
            &mut first,
            &first_port,
            &at("2030-01-01T00:00:01.000Z"),
            |messages| {
                messages.iter().find_map(|message| match message {
                    ExecutionPortMessage::ModelOpenMessage(open) => Some(open.clone()),
                    _ => None,
                })
            },
            "input flow initial model request was not delivered",
        )
        .await;
        setup_model(&root, &first_open, &dispatch.job);
        let mut first_app = application(&root);
        let first_gateway = opened(
            first_app
                .accept_local(&typed(ExecutionPortMessage::ModelOpenMessage(
                    first_open.clone(),
                )))
                .expect("accept input-flow ModelOpen"),
        );
        let identity = ProviderToolIdentity::try_new(
            ProviderToolKind::Function,
            "request_user_input".to_owned(),
            Some("functions".to_owned()),
        )
        .expect("canonical request_user_input tool");
        let usage = ProviderTokenUsage {
            input_tokens: 10,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 5,
            reasoning_output_tokens: 0,
        };
        let call_id = "provider-input-response-call".to_owned();
        let input_chunks = provider_chunks(
            &first_open,
            &first_gateway,
            [
                ProviderStreamEvent::ResponseStarted {
                    provider_response_id: "provider-input-response-1".to_owned(),
                },
                ProviderStreamEvent::ToolCallStarted {
                    index: 0,
                    provider_call_id: call_id.clone(),
                    identity,
                },
                ProviderStreamEvent::ToolCallArgumentsDelta {
                    index: 0,
                    provider_call_id: call_id.clone(),
                    delta: serde_json::json!({
                        "questions": [{
                            "id": "continue",
                            "header": "Continue",
                            "question": "Continue this turn?",
                            "options": [
                                {"label": "yes", "description": "Continue the turn."},
                                {"label": "no", "description": "Stop the turn."}
                            ]
                        }]
                    })
                    .to_string(),
                },
                ProviderStreamEvent::ToolCallEnded {
                    index: 0,
                    provider_call_id: call_id.clone(),
                },
                ProviderStreamEvent::Usage(usage),
                ProviderStreamEvent::Finished(ProviderFinishReason::ToolCalls),
            ],
            600,
        );
        for chunk in input_chunks {
            first
                .accept_control(
                    &ExecutionPortMessage::ModelChunkMessage(chunk),
                    at("2030-01-01T00:00:02.000Z"),
                )
                .await
                .expect("deliver request_user_input provider response");
        }
        // Persist the Provider-side terminal batch in the local Control Plane
        // fixture as well.  The Worker receives the equivalent hand-authored
        // frames above; this closes the Provider reservation before the crash
        // so the replacement exchange can be admitted with the same route
        // authority.
        first_app
            .complete_loopback_before_product_session_projection_for_test(
                &first_gateway,
                &at("2030-01-01T00:00:02.000Z"),
            )
            .expect("settle interrupted provider exchange in the CP fixture");
        let request = poll_until_message(
            &mut first,
            &first_port,
            &at("2030-01-01T00:00:02.000Z"),
            |messages| {
                messages.iter().find_map(|message| match message {
                    ExecutionPortMessage::InputRequestMessage(request) => Some(request.clone()),
                    _ => None,
                })
            },
            "embedded request_user_input was not delivered",
        )
        .await;
        assert_eq!(
            request.kind,
            winwincode_execution_port::generated::InputRequestMessageKind::InputRequest
        );
        assert_eq!(request.mode, InteractiveInputMode::SingleChoice);
        assert_eq!(request.choices.as_ref().map(Vec::len), Some(2));
        assert!(!request.prompt.is_empty());

        let valid_response = input_response(&request);
        let mut foreign = valid_response.clone();
        foreign.worker_session_id.0.push('X');
        first
            .accept_control(
                &ExecutionPortMessage::InputResponseMessage(foreign),
                at("2030-01-01T00:00:02.000Z"),
            )
            .await
            .expect_err("foreign input response must be rejected");

        let mut expired = valid_response.clone();
        expired.responded_at = request.expires_at.clone();
        expired.sent_at = expired.responded_at.clone();
        first
            .accept_control(
                &ExecutionPortMessage::InputResponseMessage(expired),
                request.expires_at.clone(),
            )
            .await
            .expect_err("expired input response must be rejected");

        // Snapshot before the CP response is accepted.  The input request is
        // already in the durable outbox, while Core is blocked on its exact
        // one-shot request.  Reopening the adapter must replay that frame and
        // accept the same response exactly once.
        let input_before_restart = stored_input_operation_json(&root, &request.input_request_id.0);
        assert_eq!(input_before_restart["state"], "pending");
        assert!(!input_before_restart["turnId"].as_str().unwrap().is_empty());
        let crash_snapshot = DirectorySnapshot::capture(&root.worker());
        drop(first_app);
        drop(first);
        crash_snapshot.restore(&root.worker());

        let replay_port = RecordedPort::default();
        let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config(&root))
            .expect("reopen production input adapter after response loss");
        let mut replay = winwincode_worker::WorkerMain::new(
            worker_config(),
            replay_port.clone(),
            adapter,
            root.workspace_runtime(),
        );
        register(&mut replay, &replay_port).await;
        replay
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
                at("2030-01-01T00:00:02.000Z"),
            )
            .await
            .expect("accept exact input dispatch after restart");
        let rebound_run = stored_run_json(&root);
        let rebound_kernel_session_id = rebound_run["kernelSessionId"]
            .as_str()
            .expect("durable rebound Kernel session identity");
        assert!(!rebound_kernel_session_id.is_empty());
        let replay_request = poll_until_message(
            &mut replay,
            &replay_port,
            &at("2030-01-01T00:00:02.000Z"),
            |messages| {
                messages.iter().find_map(|message| match message {
                    ExecutionPortMessage::InputRequestMessage(request) => Some(request.clone()),
                    _ => None,
                })
            },
            "durable input request was not replayed after restart",
        )
        .await;
        assert_eq!(replay_request, request);
        assert_eq!(
            replay_port
                .messages()
                .iter()
                .filter(|message| matches!(message, ExecutionPortMessage::InputRequestMessage(_)))
                .count(),
            1,
            "restart must replay one exact input request"
        );
        let rebound_input = stored_input_operation_json(&root, &request.input_request_id.0);
        assert_eq!(rebound_input["state"], "pending");
        assert_eq!(
            rebound_input["kernelSessionId"], rebound_run["kernelSessionId"],
            "restart must rebind the durable input operation to the resumed Kernel session"
        );
        assert_eq!(rebound_input["turnId"], input_before_restart["turnId"]);

        replay
            .accept_control(
                &ExecutionPortMessage::InputResponseMessage(valid_response.clone()),
                at("2030-01-01T00:00:02.000Z"),
            )
            .await
            .expect("resolve exact input response through embedded Kernel");
        // A lost ACK causes the CP to retry the exact response.  The durable
        // input operation and outbox both treat that replay as idempotent.
        replay
            .accept_control(
                &ExecutionPortMessage::InputResponseMessage(valid_response.clone()),
                at("2030-01-01T00:00:02.000Z"),
            )
            .await
            .expect("exact input response replay after ACK loss");
        let resolved_input = stored_input_operation_json(&root, &request.input_request_id.0);
        assert_eq!(resolved_input["state"], "resolved");
        assert!(resolved_input["resolutionDigest"].is_string());
        let mut changed = valid_response.clone();
        changed.message_id = ExecutionMessageId(id("xmsg", 951));
        changed.value.as_mut().expect("provided input value").value = "no".to_owned();
        replay
            .accept_control(
                &ExecutionPortMessage::InputResponseMessage(changed),
                at("2030-01-01T00:00:02.000Z"),
            )
            .await
            .expect_err("changed duplicate input response must be rejected");

        // The resumed turn first emits a fresh ModelOpen on the restarted
        // Worker port while it reconstructs the interrupted provider
        // exchange. The original ModelOpen was recorded by the predecessor
        // port, so this is the first message on the replacement port rather
        // than the post-input continuation.
        let recovery_open = poll_until_message(
            &mut replay,
            &replay_port,
            &at("2030-01-01T00:00:02.000Z"),
            |messages| {
                messages.iter().find_map(|message| match message {
                    ExecutionPortMessage::ModelOpenMessage(open) => Some(open.clone()),
                    _ => None,
                })
            },
            "restarted Kernel did not reopen the interrupted provider exchange",
        )
        .await;
        assert_ne!(
            recovery_open.model_exchange_id, first_open.model_exchange_id,
            "restart must allocate a new Provider exchange for the recovered request"
        );
        assert_eq!(
            replay_port
                .messages()
                .iter()
                .filter(|message| matches!(message, ExecutionPortMessage::ModelOpenMessage(_)))
                .count(),
            1,
            "the replacement port's first ModelOpen must be driven before the continuation"
        );

        let mut replay_app = application(&root);
        let recovery_gateway = opened(
            replay_app
                .accept_local(&typed(ExecutionPortMessage::ModelOpenMessage(
                    recovery_open.clone(),
                )))
                .expect("accept restarted recovery ModelOpen"),
        );
        replay_app
            .complete_loopback_before_product_session_projection_for_test(
                &recovery_gateway,
                &at("2030-01-01T00:00:02.000Z"),
            )
            .expect("settle recovered provider exchange in the CP fixture");
        let recovery_identity = ProviderToolIdentity::try_new(
            ProviderToolKind::Function,
            "request_user_input".to_owned(),
            Some("functions".to_owned()),
        )
        .expect("canonical recovery request_user_input tool");
        // The resumed provider exchange replays the same call identity. Core
        // consumes the already accepted response from its recovery waiter;
        // the adapter must not expose a second host prompt for that call.
        let recovery_call_id = call_id.clone();
        for chunk in provider_chunks(
            &recovery_open,
            &recovery_gateway,
            [
                ProviderStreamEvent::ResponseStarted {
                    provider_response_id: "provider-input-response-recovery-1".to_owned(),
                },
                ProviderStreamEvent::ToolCallStarted {
                    index: 0,
                    provider_call_id: recovery_call_id.clone(),
                    identity: recovery_identity,
                },
                ProviderStreamEvent::ToolCallArgumentsDelta {
                    index: 0,
                    provider_call_id: recovery_call_id.clone(),
                    delta: serde_json::json!({
                        "questions": [{
                            "id": "continue",
                            "header": "Continue",
                            "question": "Continue this turn?",
                            "options": [
                                {"label": "yes", "description": "Continue the turn."},
                                {"label": "no", "description": "Stop the turn."}
                            ]
                        }]
                    })
                    .to_string(),
                },
                ProviderStreamEvent::ToolCallEnded {
                    index: 0,
                    provider_call_id: recovery_call_id,
                },
                ProviderStreamEvent::Usage(usage),
                ProviderStreamEvent::Finished(ProviderFinishReason::ToolCalls),
            ],
            650,
        ) {
            replay
                .accept_control(
                    &ExecutionPortMessage::ModelChunkMessage(chunk),
                    at("2030-01-01T00:00:02.000Z"),
                )
                .await
                .expect("deliver recovered request_user_input provider response");
        }
        // The replacement Worker acknowledges each recovered provider frame.
        // Apply those ACKs to the same local Control Plane instance before
        // opening the continuation, so durable admission can retire the
        // completed recovery exchange rather than treating it as active.
        let recovery_acks = replay_port
            .messages()
            .into_iter()
            .filter_map(|message| match message {
                ExecutionPortMessage::ModelAckMessage(ack)
                    if ack.model_exchange_id == recovery_open.model_exchange_id =>
                {
                    Some(ack)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            !recovery_acks.is_empty(),
            "recovery exchange must emit Worker ACKs"
        );
        for ack in recovery_acks {
            replay_app
                .accept_local(&typed(ExecutionPortMessage::ModelAckMessage(ack)))
                .expect("accept recovered provider ACK");
        }

        // The `.nth(1)` is intentional: only this second restarted-port
        // ModelOpen is evidence that the accepted InputResponse resumed the
        // same turn after the recovery exchange completed.
        let second_open = poll_until_message(
            &mut replay,
            &replay_port,
            &at("2030-01-01T00:00:02.000Z"),
            |messages| {
                messages
                    .iter()
                    .filter_map(|message| match message {
                        ExecutionPortMessage::ModelOpenMessage(open) => Some(open.clone()),
                        _ => None,
                    })
                    .nth(1)
            },
            "input response did not continue the Kernel turn",
        )
        .await;
        assert_ne!(
            second_open.model_exchange_id, recovery_open.model_exchange_id,
            "InputResponse must open a distinct continuation exchange"
        );
        assert_eq!(
            replay_port
                .messages()
                .iter()
                .filter(|message| matches!(message, ExecutionPortMessage::ModelOpenMessage(_)))
                .count(),
            2,
            "the post-input continuation must be the second restarted-port ModelOpen"
        );
        let second_gateway = opened(
            replay_app
                .accept_local(&typed(ExecutionPortMessage::ModelOpenMessage(
                    second_open.clone(),
                )))
                .expect("accept post-input ModelOpen"),
        );
        for chunk in provider_chunks(
            &second_open,
            &second_gateway,
            [
                ProviderStreamEvent::ResponseStarted {
                    provider_response_id: "provider-input-response-2".to_owned(),
                },
                ProviderStreamEvent::TextStarted { index: 0 },
                ProviderStreamEvent::TextDelta {
                    index: 0,
                    delta: "input accepted".to_owned(),
                },
                ProviderStreamEvent::TextEnded { index: 0 },
                ProviderStreamEvent::Usage(usage),
                ProviderStreamEvent::Finished(ProviderFinishReason::Stop),
            ],
            700,
        ) {
            replay
                .accept_control(
                    &ExecutionPortMessage::ModelChunkMessage(chunk),
                    at("2030-01-01T00:00:02.000Z"),
                )
                .await
                .expect("deliver post-input final response");
        }
        let outcome = poll_until_message(
            &mut replay,
            &replay_port,
            &at("2030-01-01T00:00:02.000Z"),
            |messages| {
                messages.iter().find_map(|message| match message {
                    ExecutionPortMessage::JobOutcomeMessage(outcome) => Some(outcome.clone()),
                    _ => None,
                })
            },
            "input response flow did not reach terminal outcome",
        )
        .await;
        assert_eq!(outcome.outcome.status, ExecutionOutcomeStatus::Succeeded);
        assert_eq!(
            replay_port
                .messages()
                .iter()
                .filter(|message| matches!(message, ExecutionPortMessage::InputRequestMessage(_)))
                .count(),
            1,
            "the acknowledged input request must not be re-emitted after recovery"
        );
        assert_eq!(
            stored_input_operation_json(&root, &request.input_request_id.0)["state"],
            "resolved",
            "the exact input operation remains terminal after continuation"
        );
        replay
            .shutdown(at("2030-01-01T00:00:03.000Z"))
            .await
            .expect("shutdown input response Worker");
    });
}

#[test]
fn real_shell_approval_and_action_receipt_reach_one_kernel_handler() {
    run_on_large_stack(async {
        let root = TestDirectory::new("production-shell-approval");
        let dispatch = dispatch(&root);
        let port = RecordedPort::default();
        let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config(&root))
            .expect("open approval production adapter");
        let mut worker = winwincode_worker::WorkerMain::new(
            worker_config(),
            port.clone(),
            adapter,
            root.workspace_runtime(),
        );
        register(&mut worker, &port).await;
        worker
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
                at("2030-01-01T00:00:00.000Z"),
            )
            .await
            .expect("accept approval dispatch");
        let active = worker.active_jobs()[0].clone();
        let checkout = detached_checkout(&root);

        let first_open = poll_until_message(
            &mut worker,
            &port,
            &at("2030-01-01T00:00:01.000Z"),
            |messages| {
                messages.iter().find_map(|message| match message {
                    ExecutionPortMessage::ModelOpenMessage(open) => Some(open.clone()),
                    _ => None,
                })
            },
            "initial model request was not delivered",
        )
        .await;
        setup(&root, &first_open, &dispatch.job);
        let mut app = application(&root);
        let first_gateway = opened(
            app.accept_local(&typed(ExecutionPortMessage::ModelOpenMessage(
                first_open.clone(),
            )))
            .expect("accept tool-call ModelOpen"),
        );
        let identity = ProviderToolIdentity::try_new(
            ProviderToolKind::Function,
            "shell_command".to_owned(),
            Some("functions".to_owned()),
        )
        .expect("canonical built-in shell tool");
        let usage = ProviderTokenUsage {
            input_tokens: 10,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 5,
            reasoning_output_tokens: 0,
        };
        let call_id = "provider-shell-approval-call".to_owned();
        let command = "printf 'pub fn fixture_value() -> u64 { 2 }\\n' > src/lib.rs";
        let tool_chunks = provider_chunks(
            &first_open,
            &first_gateway,
            [
                ProviderStreamEvent::ResponseStarted {
                    provider_response_id: "provider-shell-response-1".to_owned(),
                },
                ProviderStreamEvent::ToolCallStarted {
                    index: 0,
                    provider_call_id: call_id.clone(),
                    identity,
                },
                ProviderStreamEvent::ToolCallArgumentsDelta {
                    index: 0,
                    provider_call_id: call_id.clone(),
                    delta: serde_json::json!({
                        "command": command,
                        "workdir": checkout.to_string_lossy(),
                        "justification": "exercise the canonical approval path",
                        "sandbox_permissions": "require_escalated"
                    })
                    .to_string(),
                },
                ProviderStreamEvent::ToolCallEnded {
                    index: 0,
                    provider_call_id: call_id,
                },
                ProviderStreamEvent::Usage(usage),
                ProviderStreamEvent::Finished(ProviderFinishReason::ToolCalls),
            ],
            200,
        );
        for (index, chunk) in tool_chunks.into_iter().enumerate() {
            let sequence = chunk.sequence.0;
            let is_final = chunk.is_final;
            worker
                .accept_control(
                    &ExecutionPortMessage::ModelChunkMessage(chunk),
                    at("2030-01-01T00:00:02.000Z"),
                )
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "deliver canonical tool-call chunk index={index} sequence={sequence} final={is_final}: {error:?}"
                    )
                });
        }
        let approval = poll_until_message(
            &mut worker,
            &port,
            &at("2030-01-01T00:00:02.000Z"),
            |messages| {
                messages.iter().find_map(|message| match message {
                    ExecutionPortMessage::ApprovalRequestMessage(request) => Some(request.clone()),
                    _ => None,
                })
            },
            "embedded approval request was not delivered",
        )
        .await;
        assert!(approval.action.details.is_none());
        let decided_at = at("2030-01-01T00:00:02.000Z");
        worker
            .accept_control(
                &ExecutionPortMessage::ApprovalDecisionMessage(ApprovalDecisionMessage {
                    approval_id: approval.approval_id.clone(),
                    decided_at: decided_at.clone(),
                    decision: ApprovalDecisionMessageDecision::Approved,
                    kind: ApprovalDecisionMessageKind::ApprovalDecision,
                    lease: approval.lease.clone(),
                    message_id: ExecutionMessageId(id("xmsg", 900)),
                    reason: None,
                    schema_version: SchemaVersion::WinwincodeV1,
                    scope: ApprovalDecisionMessageScope::Once,
                    sent_at: decided_at.clone(),
                    session_identity: approval.session_identity.clone(),
                    worker_session_id: approval.worker_session_id.clone(),
                }),
                decided_at.clone(),
            )
            .await
            .expect("resolve exact embedded approval");

        let action = poll_until_message(
            &mut worker,
            &port,
            &decided_at,
            |messages| {
                messages.iter().find_map(|message| match message {
                    ExecutionPortMessage::ActionEnforcementRequestMessage(request) => {
                        Some(request.clone())
                    }
                    _ => None,
                })
            },
            "typed action request was not delivered after approval",
        )
        .await;
        let mut receipt = ActionEnforcementReceiptMessage {
            actor: UserActor {
                id: UserId(id("usr", 9)),
                kind: UserActorKind::User,
            },
            decision: ActionEnforcementDecision::Permit,
            evaluated_at: decided_at.clone(),
            evaluation_sha256: digest('e'),
            job_id: action.job_id,
            kind: ActionEnforcementReceiptMessageKind::ActionEnforcementReceipt,
            lease: action.lease,
            matched_condition_sha256: action.matched_condition_sha256,
            message_id: ExecutionMessageId(id("xmsg", 901)),
            policy_kind: action.policy_kind,
            policy_mode: None,
            policy_version: None,
            receipt_signature: digest('0'),
            request_id: action.request_id,
            resource: action.resource,
            schema_version: SchemaVersion::WinwincodeV1,
            scope: RepositoryScope {
                kind: RepositoryScopeKind::Repository,
                organization_id: OrganizationId(id("org", 1)),
                workspace_id: WorkspaceId(id("wsp", 1)),
                project_id: ProjectId(id("prj", 1)),
                repository_id: RepositoryId(id("rep", 1)),
            },
            sent_at: decided_at.clone(),
            session_identity: action.session_identity,
            subject_sha256: action.subject_sha256,
            worker_session_id: action.worker_session_id,
        };
        ActionEnforcementIssuer::new(action_signing_key())
            .sign(&mut receipt)
            .expect("sign exact action permit");
        worker
            .accept_control(
                &ExecutionPortMessage::ActionEnforcementReceiptMessage(receipt),
                decided_at.clone(),
            )
            .await
            .expect("accept exact action permit");

        let second_open = poll_until_message(
            &mut worker,
            &port,
            &decided_at,
            |messages| {
                messages
                    .iter()
                    .filter_map(|message| match message {
                        ExecutionPortMessage::ModelOpenMessage(open) => Some(open.clone()),
                        _ => None,
                    })
                    .nth(1)
            },
            "approved shell did not enter one handler and request the next model turn",
        )
        .await;
        assert_eq!(
            fs::read_to_string(checkout.join("src/lib.rs"))
                .expect("approved shell changed the detached source checkout"),
            "pub fn fixture_value() -> u64 { 2 }\n"
        );
        let second_gateway = opened(
            app.accept_local(&typed(ExecutionPortMessage::ModelOpenMessage(
                second_open.clone(),
            )))
            .expect("accept post-tool ModelOpen"),
        );
        let final_chunks = provider_chunks(
            &second_open,
            &second_gateway,
            [
                ProviderStreamEvent::ResponseStarted {
                    provider_response_id: "provider-shell-response-2".to_owned(),
                },
                ProviderStreamEvent::TextStarted { index: 0 },
                ProviderStreamEvent::TextDelta {
                    index: 0,
                    delta: "approved shell completed".to_owned(),
                },
                ProviderStreamEvent::TextEnded { index: 0 },
                ProviderStreamEvent::Usage(usage),
                ProviderStreamEvent::Finished(ProviderFinishReason::Stop),
            ],
            300,
        );
        for chunk in final_chunks {
            worker
                .accept_control(
                    &ExecutionPortMessage::ModelChunkMessage(chunk),
                    decided_at.clone(),
                )
                .await
                .expect("deliver post-tool final response");
        }
        let artifact_open = poll_until_message(
            &mut worker,
            &port,
            &decided_at,
            |messages| {
                messages.iter().find_map(|message| match message {
                    ExecutionPortMessage::ArtifactOpenMessage(open) => Some(open.clone()),
                    _ => None,
                })
            },
            "approved writer did not retain a detached candidate artifact",
        )
        .await;
        let artifact = ArtifactReference {
            artifact_id: artifact_open.artifact.artifact_id.clone(),
            digest: artifact_open.artifact.digest.clone(),
        };
        assert_eq!(
            fs::read_to_string(checkout.join("src/lib.rs"))
                .expect("approved shell changed detached source checkout"),
            "pub fn fixture_value() -> u64 { 2 }\n"
        );
        worker
            .accept_control(
                &ExecutionPortMessage::ArtifactAckMessage(candidate_ack(&active, &artifact, 0)),
                decided_at.clone(),
            )
            .await
            .expect("ack detached candidate open");
        worker
            .accept_control(
                &ExecutionPortMessage::ArtifactAckMessage(candidate_ack(&active, &artifact, 1)),
                decided_at.clone(),
            )
            .await
            .expect("ack detached candidate final chunk");
        let messages = port.messages();
        let outcome = messages
            .iter()
            .find_map(|message| match message {
                ExecutionPortMessage::JobOutcomeMessage(outcome) => Some(outcome),
                _ => None,
            })
            .expect("approved writer outcome");
        assert_eq!(outcome.outcome.status, ExecutionOutcomeStatus::Succeeded);
        assert_eq!(outcome.outcome.artifacts, vec![artifact]);
        assert!(
            !checkout.exists(),
            "terminal candidate outcome consumes the detached checkout"
        );
        assert!(
            messages.iter().any(|message| matches!(
                message,
                ExecutionPortMessage::ArtifactChunkMessage(chunk) if chunk.is_final
            )),
            "candidate artifact final chunk was delivered"
        );
        let approval_index = messages
            .iter()
            .position(|message| matches!(message, ExecutionPortMessage::ApprovalRequestMessage(_)))
            .expect("approval request delivery");
        let action_index = messages
            .iter()
            .position(|message| {
                matches!(
                    message,
                    ExecutionPortMessage::ActionEnforcementRequestMessage(_)
                )
            })
            .expect("action request delivery");
        assert!(approval_index < action_index);
    });
}
