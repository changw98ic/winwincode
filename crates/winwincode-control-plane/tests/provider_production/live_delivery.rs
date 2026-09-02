// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::process::Command;
use winwincode_api::generated::{
    AcceptanceCriterionInput, DeliveryAdvanceCommand, DeliveryAdvanceCommandCommand,
    DeliveryAdvancePayload, DeliveryApproveTaskBreakdownCommand,
    DeliveryApproveTaskBreakdownCommandCommand, DeliveryApproveTaskBreakdownPayload,
    DeliveryCreateCommand, DeliveryCreateCommandCommand, DeliveryCreatePayload,
    DeliveryResolveAttentionCommand, DeliveryResolveAttentionCommandCommand,
    DeliveryResolveAttentionPayload, DeliverySpecInput, DeliverySubmitVerdictCommand,
    DeliverySubmitVerdictCommandCommand, DeliverySubmitVerdictPayload, DeliveryUpdateSpecCommand,
    DeliveryUpdateSpecCommandCommand, DeliveryUpdateSpecPayload,
};
use winwincode_control_plane::{
    ControlPlane, ControlPlaneConfig, EventPublishError, EventPublisher,
    LocalDeliveryAdapterConfig, ModelRetryUsageService, ModelUsageFilter, OutboxEvent,
};
use winwincode_delivery::{
    application::stage::{
        DurableTerminalOutcomeInput, TerminalArtifactReference, TerminalOutcomeStatus,
        reconcile_durable_terminal_outcome,
        test_support::{active_lease_identity, session_binding_authority},
    },
    domain::{AttentionItemStatus, Delivery, DeliveryStatus, StageRunStatus},
};
use winwincode_domain::{
    ArtifactId, ExecutionEventId, SessionBindingSourceIdentity, SessionBindingSourceIdentityKind,
};
use winwincode_execution_port::generated::{
    ArtifactChunkMessage, ArtifactChunkMessageKind, ArtifactDescriptor, ArtifactKind,
    ArtifactOpenMessage, ArtifactOpenMessageKind, ArtifactReference, ExecutionEventCategory,
    ExecutionEventRecord, ExecutionOutcome, ExecutionOutcomeStatus, ExecutionOutcomeUsage,
    JobOutcomeMessage, JobOutcomeMessageKind, RuntimeEventMessage, RuntimeEventMessageKind,
    SessionBindingMessage, SessionBindingMessageKind,
};
use winwincode_storage::{
    CandidateSourceManifest, ExecutionLeaseTerminalOutcome, ExecutionLeaseTerminalRequest,
    ExecutionReservationSettlement, WorkerSlotCloseRequest, WorkerSlotState,
};

const LIVE_GATE: &str = "WINWINCODE_MIMO_LIVE_DELIVERY_GATE";
const LIVE_EVIDENCE: &str = "WINWINCODE_MIMO_LIVE_DELIVERY_EVIDENCE";
const LIVE_PRIVATE_INPUT: &str = "WINWINCODE_LIVE_PROVIDER_PRIVATE_INPUT_FILE";
const UPSTREAM_MODEL: &str = "mimo-v2.5-pro";
const PLANNER_MEDIA_TYPE: &str = "application/vnd.winwincode.planner-solution+json";
const CANDIDATE_MEDIA_TYPE: &str = "application/vnd.winwincode.git-candidate+json";
const CANDIDATE_EVIDENCE_MEDIA_TYPE: &str =
    "application/vnd.winwincode.provider-candidate-evidence+json";

struct NoopPublisher;

impl EventPublisher for NoopPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Ok(())
    }
}

#[derive(Clone)]
struct StageAuthority {
    job: ExecutionJob,
    queue_scope: ExecutionQueueScope,
    binding: SessionBindingMessage,
    authority: winwincode_delivery::application::stage::SessionBindingAuthority,
    model: ModelOpenMessage,
    lease_terminal_request_id: RequestId,
}

struct ProviderResult {
    text: String,
    terminal: winwincode_control_plane::ProviderGatewayTerminalReceipt,
    final_sequence: ExecutionSequence,
}

struct LiveDeliveryRuntime {
    root: std::mem::ManuallyDrop<TestDirectory>,
    repository: PathBuf,
    scope: RepositoryScope,
    delivery_id: DeliveryId,
    delivery_cp: ControlPlane,
    model_application: StandaloneModelExecutionApplication,
    provider_id: String,
    live_secret: Vec<u8>,
    private_input: String,
}

struct CurrentStageFacts {
    stage_run_id: StageRunId,
    product_session_id: ProductSessionId,
    job: ExecutionJob,
    queue_scope: ExecutionQueueScope,
    submitted_at: Instant,
}

#[test]
#[ignore = "requires explicit real MIMO Delivery gate, endpoint, and 0600 secret file"]
fn live_mimo_completes_one_production_delivery_with_durable_evidence() {
    assert_eq!(std::env::var(LIVE_GATE).as_deref(), Ok("1"));
    let evidence_path = PathBuf::from(
        std::env::var_os(LIVE_EVIDENCE).expect("configured live Delivery evidence path"),
    );
    let (mut runtime, planner) = LiveDeliveryRuntime::start(&evidence_path);
    let planner_result = runtime.complete_planner(&planner);
    runtime.approve_plan_and_start_executor();
    let executor = runtime.current_stage(200, &executor_prompt());
    let (executor_result, candidate_commit, candidate_ref) = runtime.complete_executor(&executor);
    let (reviewer, reviewer_result) =
        runtime.complete_verification("reviewer", 300, 310, &candidate_commit, &candidate_ref);
    runtime.advance(16, "start independent verifier");
    let (verifier, verifier_result) =
        runtime.complete_verification("verifier", 400, 410, &candidate_commit, &candidate_ref);
    let delivered = runtime.complete_delivery(&candidate_ref);

    let runs = [
        (&planner, &planner_result),
        (&executor, &executor_result),
        (&reviewer, &reviewer_result),
        (&verifier, &verifier_result),
    ];
    let evidence = live_evidence(&runtime.root, &runtime.repository, &delivered, &runs);
    write_evidence(&evidence_path, &evidence);
    runtime.verify_private_bytes_absent();
    runtime.shutdown();
}

impl LiveDeliveryRuntime {
    fn start(evidence_path: &Path) -> (Self, StageAuthority) {
        let run_root = evidence_path.parent().expect("evidence parent").join("run");
        let _ = fs::remove_dir_all(&run_root);
        fs::create_dir_all(&run_root).expect("create live Delivery root");
        let root = std::mem::ManuallyDrop::new(TestDirectory(run_root));
        let repository = root.0.join("repository");
        let baseline = initialize_repository(&repository);
        let scope = repository_scope();
        let delivery_id = DeliveryId(id("dlv", 1));
        let mut delivery_cp = ControlPlane::start_local_with_delivery_adapters(
            ControlPlaneConfig::local(root.data()),
            Box::new(NoopPublisher),
            LocalDeliveryAdapterConfig::new(&repository, scope.clone()),
        )
        .expect("start production Delivery Control Plane");
        delivery_cp
            .delivery_create(&create_command(&scope, &delivery_id, baseline))
            .expect("create production Delivery");
        let delivery = load_delivery(&delivery_cp, &delivery_id);
        delivery_cp
            .delivery_update_spec(&update_spec_command(&scope, &delivery, 9))
            .expect("accept production Delivery Spec");
        let delivery = load_delivery(&delivery_cp, &delivery_id);
        delivery_cp
            .delivery_advance(&advance_command(&scope, &delivery, 10))
            .expect("start Planner");
        let live_secret = fs::read(live_secret_path()).expect("read live Provider secret file");
        let private_input = fs::read_to_string(private_input_path())
            .expect("read live Provider private input file");
        assert!(
            !private_input.is_empty(),
            "live Provider private input is empty"
        );
        let provider_id =
            std::env::var("WINWINCODE_MIMO_LIVE_PROVIDER_ID").expect("configured live Provider id");
        let endpoint = std::env::var("WINWINCODE_MIMO_LIVE_PROVIDER_ENDPOINT")
            .expect("configured live Provider endpoint");
        let planner = prepare_current_stage(
            &root,
            &mut delivery_cp,
            &delivery_id,
            100,
            &planner_prompt(&private_input),
        );
        let model_application = configure_live_model_application(
            &root,
            &planner.model,
            &provider_id,
            &endpoint,
            live_secret.clone(),
        );
        (
            Self {
                root,
                repository,
                scope,
                delivery_id,
                delivery_cp,
                model_application,
                provider_id,
                live_secret,
                private_input,
            },
            planner,
        )
    }

    fn current_stage(&mut self, seed: u64, prompt: &[u8]) -> StageAuthority {
        let stage = prepare_current_stage(
            &self.root,
            &mut self.delivery_cp,
            &self.delivery_id,
            seed,
            prompt,
        );
        configure_stage_model_authority(&self.root, &stage, &self.provider_id);
        assert_stage_gateway_prerequisites(&self.root, &stage, &self.provider_id);
        stage
    }

    fn complete_planner(&mut self, planner: &StageAuthority) -> ProviderResult {
        assert_stage_gateway_prerequisites(&self.root, planner, &self.provider_id);
        let result = run_provider(&mut self.model_application, planner);
        accept_runtime_payload(
            &mut self.delivery_cp,
            &self.scope,
            planner,
            1,
            ExecutionEventCategory::Activity,
            PLANNER_MEDIA_TYPE,
            &canonical_planner_solution(&result.text),
        );
        finish_model(&mut self.model_application, planner, &result);
        commit_stage_success(
            &mut self.delivery_cp,
            &self.scope,
            planner,
            &result,
            Vec::new(),
            1,
        );
        settle_stage_resources(&self.root, planner, &result);
        result
    }

    fn approve_plan_and_start_executor(&mut self) {
        self.advance(11, "open canonical PlanReview");
        let delivery = load_delivery(&self.delivery_cp, &self.delivery_id);
        let review = delivery
            .snapshot()
            .attention_items
            .iter()
            .find(|item| item.status == AttentionItemStatus::Open)
            .expect("PlanReview Attention");
        let context: serde_json::Value =
            serde_json::from_str(&review.context).expect("canonical Solution Review context");
        let review_set = context["reviewSetSha256"]
            .as_str()
            .expect("review-set digest")
            .to_owned();
        self.delivery_cp
            .delivery_resolve_attention(&resolve_plan_review_command(
                &self.scope,
                &delivery,
                &review.id,
                &review_set,
                12,
            ))
            .expect("approve canonical Solution Review");
        let delivery = load_delivery(&self.delivery_cp, &self.delivery_id);
        self.delivery_cp
            .delivery_approve_task_breakdown(&approve_tasks_command(
                &self.scope,
                &delivery,
                &review_set,
                13,
            ))
            .expect("promote approved task graph");
        self.advance(14, "start executor");
    }

    fn complete_executor(&mut self, executor: &StageAuthority) -> (ProviderResult, String, String) {
        let result = run_provider(&mut self.model_application, executor);
        let candidate_commit = commit_provider_candidate(&self.repository, &result.text);
        let candidate = upload_candidate(
            &mut self.delivery_cp,
            &self.scope,
            executor,
            &candidate_commit,
            210,
        );
        accept_runtime_payload(
            &mut self.delivery_cp,
            &self.scope,
            executor,
            1,
            ExecutionEventCategory::Activity,
            CANDIDATE_EVIDENCE_MEDIA_TYPE,
            &provider_candidate_evidence(&result.text),
        );
        finish_model(&mut self.model_application, executor, &result);
        let terminal = commit_stage_success(
            &mut self.delivery_cp,
            &self.scope,
            executor,
            &result,
            vec![candidate.clone()],
            1,
        );
        settle_stage_resources(&self.root, executor, &result);
        self.advance(15, "settle executor and start independent reviewer");
        let candidate_ref = self
            .delivery_cp
            .resolve_delivery_candidate(
                &self.scope,
                &self.delivery_id,
                &candidate.artifact_id,
                &candidate.digest,
                &terminal,
            )
            .expect("freeze production candidate")
            .candidate_ref()
            .to_owned();
        (result, candidate_commit, candidate_ref)
    }

    fn complete_verification(
        &mut self,
        role: &str,
        stage_seed: u64,
        artifact_seed: u64,
        candidate_commit: &str,
        candidate_ref: &str,
    ) -> (StageAuthority, ProviderResult) {
        let stage = self.current_stage(stage_seed, &verification_prompt(role, candidate_ref));
        let result = run_provider(&mut self.model_application, &stage);
        let candidate = upload_candidate(
            &mut self.delivery_cp,
            &self.scope,
            &stage,
            candidate_commit,
            artifact_seed,
        );
        accept_verification_events(
            &mut self.delivery_cp,
            &self.scope,
            &stage,
            candidate_ref,
            role,
            &result.text,
        );
        finish_model(&mut self.model_application, &stage, &result);
        commit_stage_success(
            &mut self.delivery_cp,
            &self.scope,
            &stage,
            &result,
            vec![candidate],
            4,
        );
        settle_stage_resources(&self.root, &stage, &result);
        (stage, result)
    }

    fn advance(&mut self, seed: u64, expectation: &str) {
        let delivery = load_delivery(&self.delivery_cp, &self.delivery_id);
        self.delivery_cp
            .delivery_advance(&advance_command(&self.scope, &delivery, seed))
            .unwrap_or_else(|error| panic!("{expectation}: {error}"));
    }

    fn complete_delivery(&mut self, candidate_ref: &str) -> Delivery {
        let delivery = load_delivery(&self.delivery_cp, &self.delivery_id);
        let digest = Sha256Digest(
            candidate_ref
                .strip_prefix("git-candidate:")
                .expect("canonical candidate reference")
                .to_owned(),
        );
        self.delivery_cp
            .delivery_submit_verdict(&submit_verdict_command(&self.scope, &delivery, digest, 17))
            .expect("submit production verdict");
        let ready = load_delivery(&self.delivery_cp, &self.delivery_id);
        assert_eq!(ready.snapshot().status, DeliveryStatus::ReadyToDeliver);
        self.delivery_cp
            .delivery_advance(&advance_command(&self.scope, &ready, 18))
            .expect("open DeliveryReview");
        let review_delivery = load_delivery(&self.delivery_cp, &self.delivery_id);
        let review = review_delivery
            .snapshot()
            .attention_items
            .iter()
            .find(|item| item.status == AttentionItemStatus::Open)
            .expect("DeliveryReview Attention");
        self.delivery_cp
            .delivery_resolve_attention(&resolve_delivery_review_command(
                &self.scope,
                &review_delivery,
                &review.id,
                19,
            ))
            .expect("approve final DeliveryReview");
        let delivered = load_delivery(&self.delivery_cp, &self.delivery_id);
        assert_eq!(delivered.snapshot().status, DeliveryStatus::Delivered);
        delivered
    }

    fn verify_private_bytes_absent(&self) {
        assert_files_omit(&self.root.data(), &self.live_secret);
        assert_files_omit(&self.root.data(), self.private_input.as_bytes());
    }

    fn shutdown(self) {
        self.delivery_cp
            .shutdown()
            .expect("shutdown Delivery Control Plane");
        drop(self.model_application);
    }
}

fn live_secret_path() -> PathBuf {
    let path = PathBuf::from(
        std::env::var_os("WINWINCODE_MIMO_LIVE_PROVIDER_SECRET_FILE")
            .expect("configured live Provider secret file"),
    );
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(&path)
            .expect("live secret metadata")
            .permissions()
            .mode()
            & 0o077,
        0,
        "live secret file must be private"
    );
    path
}

fn private_input_path() -> PathBuf {
    let path = PathBuf::from(
        std::env::var_os(LIVE_PRIVATE_INPUT).expect("configured live Provider private input file"),
    );
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(&path)
            .expect("private input metadata")
            .permissions()
            .mode()
            & 0o077,
        0,
        "private input file must be private"
    );
    path
}

fn initialize_repository(repository: &Path) -> String {
    fs::create_dir_all(repository.join("src")).expect("create live repository");
    fs::write(
        repository.join("Cargo.toml"),
        "[package]\nname = \"live-delivery\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    )
    .expect("write manifest");
    fs::write(
        repository.join("src/lib.rs"),
        "pub fn baseline() -> u64 { 1 }\n",
    )
    .expect("write source");
    for arguments in [
        &["init", "-q"][..],
        &["config", "user.email", "live-delivery@example.invalid"][..],
        &["config", "user.name", "Live Delivery"][..],
        &["add", "."][..],
        &["commit", "-q", "-m", "baseline"][..],
    ] {
        let status = Command::new("git")
            .args(arguments)
            .current_dir(repository)
            .status()
            .expect("run Git");
        assert!(status.success(), "Git failed: {arguments:?}");
    }
    git_output(repository, &["rev-parse", "HEAD"])
}

fn git_output(repository: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(repository)
        .output()
        .expect("run Git query");
    assert!(output.status.success(), "Git query failed: {arguments:?}");
    String::from_utf8(output.stdout)
        .expect("UTF-8 Git output")
        .trim()
        .to_owned()
}

fn create_command(
    scope: &RepositoryScope,
    delivery_id: &DeliveryId,
    baseline: String,
) -> DeliveryCreateCommand {
    DeliveryCreateCommand {
        actor: actor(),
        command: DeliveryCreateCommandCommand::DeliveryCreate,
        expected_revision: Revision(0),
        payload: DeliveryCreatePayload {
            delivery_id: delivery_id.clone(),
            spec: DeliverySpecInput {
                acceptance_criteria: vec![AcceptanceCriterionInput {
                    id: "criterion-live-provider".to_owned(),
                    required: true,
                    title: "The real Provider-authored candidate passes cargo test".to_owned(),
                }],
                base_revision: baseline,
                goal: "Complete one real Provider Delivery end to end".to_owned(),
                publication_target: None,
                repository_id: scope.repository_id.clone(),
                title: "Real MIMO production Delivery".to_owned(),
            },
            tasks: Vec::new(),
        },
        request_id: RequestId(id("req", 9_001)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope.clone(),
    }
}

fn update_spec_command(
    scope: &RepositoryScope,
    delivery: &Delivery,
    seed: u64,
) -> DeliveryUpdateSpecCommand {
    DeliveryUpdateSpecCommand {
        actor: actor(),
        command: DeliveryUpdateSpecCommandCommand::DeliveryUpdateSpec,
        expected_revision: Revision(i64::try_from(delivery.revision()).expect("Delivery revision")),
        payload: DeliveryUpdateSpecPayload {
            delivery_id: delivery.id().clone(),
            spec: DeliverySpecInput {
                acceptance_criteria: vec![AcceptanceCriterionInput {
                    id: "criterion-live-provider".to_owned(),
                    required: true,
                    title: "The real Provider-authored candidate passes cargo test".to_owned(),
                }],
                base_revision: delivery.snapshot().spec.base_revision.clone(),
                goal: "Complete one real Provider Delivery end to end".to_owned(),
                publication_target: None,
                repository_id: scope.repository_id.clone(),
                title: "Real MIMO production Delivery".to_owned(),
            },
        },
        request_id: RequestId(id("req", 9_000 + seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope.clone(),
    }
}

fn advance_command(
    scope: &RepositoryScope,
    delivery: &Delivery,
    seed: u64,
) -> DeliveryAdvanceCommand {
    DeliveryAdvanceCommand {
        actor: actor(),
        command: DeliveryAdvanceCommandCommand::DeliveryAdvance,
        expected_revision: Revision(i64::try_from(delivery.revision()).expect("Delivery revision")),
        payload: DeliveryAdvancePayload {
            delivery_id: delivery.id().clone(),
        },
        request_id: RequestId(id("req", 9_000 + seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope.clone(),
    }
}

fn load_delivery(control_plane: &ControlPlane, delivery_id: &DeliveryId) -> Delivery {
    let state = control_plane
        .load_state(&format!("delivery:{}", delivery_id.0))
        .expect("load Delivery state")
        .expect("Delivery state");
    let delivery = Delivery::decode_json(&state.payload).expect("decode Delivery");
    assert_eq!(state.revision, delivery.revision());
    delivery
}

fn resolve_plan_review_command(
    scope: &RepositoryScope,
    delivery: &Delivery,
    attention_item_id: &winwincode_domain::AttentionItemId,
    review_set: &str,
    seed: u64,
) -> DeliveryResolveAttentionCommand {
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Decision<'a> {
        schema_version: u8,
        protocol: &'static str,
        delivery_id: &'a DeliveryId,
        delivery_spec_id: &'a winwincode_delivery::domain::DeliverySpecId,
        delivery_spec_revision: u64,
        review_stage_run_id: &'a StageRunId,
        attention_item_id: &'a winwincode_domain::AttentionItemId,
        review_set_sha256: &'a str,
        action: &'static str,
        comments: Option<&'static str>,
        requested_changes: Option<Vec<String>>,
    }
    let review_run = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| run.stage == winwincode_delivery::domain::DeliveryStage::PlanReview)
        .expect("PlanReview run");
    let resolution = Decision {
        schema_version: 1,
        protocol: "winwincode.solution-review-decision.v1",
        delivery_id: delivery.id(),
        delivery_spec_id: &delivery.snapshot().spec.id,
        delivery_spec_revision: delivery.snapshot().spec.revision,
        review_stage_run_id: &review_run.id,
        attention_item_id,
        review_set_sha256: review_set,
        action: "approve",
        comments: Some("Real Provider plan approved"),
        requested_changes: None,
    };
    DeliveryResolveAttentionCommand {
        actor: actor(),
        command: DeliveryResolveAttentionCommandCommand::DeliveryResolveAttention,
        expected_revision: Revision(i64::try_from(delivery.revision()).expect("revision")),
        payload: DeliveryResolveAttentionPayload {
            attention_item_id: attention_item_id.clone(),
            decision: "resolve".to_owned(),
            delivery_id: delivery.id().clone(),
            remediation: None,
            resolution: serde_json::to_string(&resolution).expect("review resolution"),
        },
        request_id: RequestId(id("req", 9_000 + seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope.clone(),
    }
}

fn approve_tasks_command(
    scope: &RepositoryScope,
    delivery: &Delivery,
    review_set: &str,
    seed: u64,
) -> DeliveryApproveTaskBreakdownCommand {
    DeliveryApproveTaskBreakdownCommand {
        actor: actor(),
        command: DeliveryApproveTaskBreakdownCommandCommand::DeliveryApproveTaskBreakdown,
        expected_revision: Revision(i64::try_from(delivery.revision()).expect("revision")),
        payload: DeliveryApproveTaskBreakdownPayload {
            delivery_id: delivery.id().clone(),
            review_set_sha256: Sha256Digest(format!("sha256:{review_set}")),
        },
        request_id: RequestId(id("req", 9_000 + seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope.clone(),
    }
}

fn submit_verdict_command(
    scope: &RepositoryScope,
    delivery: &Delivery,
    candidate_digest: Sha256Digest,
    seed: u64,
) -> DeliverySubmitVerdictCommand {
    DeliverySubmitVerdictCommand {
        actor: actor(),
        command: DeliverySubmitVerdictCommandCommand::DeliverySubmitVerdict,
        expected_revision: Revision(i64::try_from(delivery.revision()).expect("revision")),
        payload: DeliverySubmitVerdictPayload {
            candidate_digest,
            delivery_id: delivery.id().clone(),
        },
        request_id: RequestId(id("req", 9_000 + seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope.clone(),
    }
}

fn resolve_delivery_review_command(
    scope: &RepositoryScope,
    delivery: &Delivery,
    attention_item_id: &winwincode_domain::AttentionItemId,
    seed: u64,
) -> DeliveryResolveAttentionCommand {
    DeliveryResolveAttentionCommand {
        actor: actor(),
        command: DeliveryResolveAttentionCommandCommand::DeliveryResolveAttention,
        expected_revision: Revision(i64::try_from(delivery.revision()).expect("revision")),
        payload: DeliveryResolveAttentionPayload {
            attention_item_id: attention_item_id.clone(),
            decision: "resolve".to_owned(),
            delivery_id: delivery.id().clone(),
            remediation: None,
            resolution: "approved real Provider candidate".to_owned(),
        },
        request_id: RequestId(id("req", 9_000 + seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: scope.clone(),
    }
}

fn configure_live_model_application(
    root: &TestDirectory,
    message: &ModelOpenMessage,
    provider_id: &str,
    endpoint: &str,
    secret: Vec<u8>,
) -> StandaloneModelExecutionApplication {
    let mut storage = SqliteStorage::open(root.data()).expect("open model authority storage");
    configure_provider_authority(
        &mut storage,
        message,
        provider_id,
        UPSTREAM_MODEL,
        "anthropic-messages",
    );
    let resolution = CredentialReferenceService::new(&mut storage)
        .resolve(
            &Scope::OrganizationScope(organization_scope()),
            &CredentialReferenceId(id("crd", 1)),
        )
        .expect("resolve live credential reference");
    drop(storage);
    LocalSecretStoreAdapter::open(root.secrets())
        .expect("open live SecretStore")
        .store(
            &resolution,
            ResolvedSecret::from_bytes(secret).expect("live resolved secret"),
        )
        .expect("store live credential");
    let mut provider = HttpsSseProviderConfig::try_new(
        provider_id.to_owned(),
        endpoint.to_owned(),
        HttpsSseProviderTimeouts {
            connect: Duration::from_secs(10),
            first_byte: Duration::from_mins(1),
            idle: Duration::from_mins(1),
            total: Duration::from_mins(3),
        },
        HttpsSseProviderLimits {
            response_bytes: 16 * 1024 * 1024,
            event_bytes: 1024 * 1024,
            events: 10_000,
        },
    )
    .expect("live Provider config");
    if let Some(root_path) = std::env::var_os("WINWINCODE_MIMO_LIVE_PROVIDER_TLS_ROOT_DER") {
        provider = provider
            .with_specific_tls_roots(vec![fs::read(root_path).expect("read live TLS root")])
            .expect("live TLS root");
    }
    provider = provider
        .with_anthropic_messages(2_048, ProviderTokenPricing::default())
        .expect("live Anthropic Messages config");
    application_with_provider_reservation(
        root,
        vec![StandaloneProviderConfig::HttpsSse(provider)],
        ProviderAdmissionReservationConfig::try_new(20_000, 100).expect("live reservation config"),
    )
}

fn configure_stage_model_authority(
    root: &TestDirectory,
    stage: &StageAuthority,
    provider_id: &str,
) {
    let mut storage = SqliteStorage::open(root.data()).expect("open stage model authority");
    configure_provider_authority(
        &mut storage,
        &stage.model,
        provider_id,
        UPSTREAM_MODEL,
        "anthropic-messages",
    );
}

fn assert_stage_gateway_prerequisites(
    root: &TestDirectory,
    stage: &StageAuthority,
    provider_id: &str,
) {
    let identity_source = DurableProviderGatewayIdentitySource::open(root.data())
        .expect("open durable Provider identity source");
    let identity = identity_source
        .authorize(&stage.model)
        .unwrap_or_else(|error| panic!("Provider identity prerequisite: {:?}", error.kind()));
    let mut storage = SqliteStorage::open(root.data()).expect("open Provider route prerequisite");
    let route = ModelSettingsService::new(&mut storage)
        .resolve(identity.target())
        .expect("resolve Provider route prerequisite");
    assert_eq!(route.provider_id, provider_id);
    assert_eq!(route.model_id, UPSTREAM_MODEL);
    CredentialReferenceService::new(&mut storage)
        .resolve(
            &Scope::OrganizationScope(organization_scope()),
            &route.credential_reference_id,
        )
        .expect("resolve Provider credential reference prerequisite");
}

fn prepare_current_stage(
    root: &TestDirectory,
    control_plane: &mut ControlPlane,
    delivery_id: &DeliveryId,
    seed: u64,
    prompt: &[u8],
) -> StageAuthority {
    let CurrentStageFacts {
        stage_run_id,
        product_session_id,
        job,
        queue_scope,
        submitted_at,
    } = load_current_stage_facts(root, control_plane, delivery_id);
    let mut storage = SqliteStorage::open(root.data()).expect("open stage storage");
    let worker_session_id = WorkerSessionId(id("wsn", seed));
    let codex_thread_id = CodexThreadId(id("cdx", seed));
    let lease_id = LeaseId(id("lse", seed));
    let worker_id = WorkerId(id("wrk", seed));
    let worker_instance_id = WorkerInstanceId(id("wki", seed));
    let fencing_token = FencingToken(seed.to_string());
    let lease = ExecutionLeaseStamp {
        attempt: job.attempt,
        expires_at: job.limits.deadline_at.clone(),
        fencing_token: fencing_token.clone(),
        issued_at: submitted_at.clone(),
        job_id: job.job_id.clone(),
        lease_id: lease_id.clone(),
        worker_id: worker_id.clone(),
        worker_instance_id: worker_instance_id.clone(),
    };
    let session_identity = SessionIdentity {
        codex_thread_id: codex_thread_id.clone(),
        product_session_id: product_session_id.clone(),
        stage_run_id: Some(stage_run_id.clone()),
        worker_session_id: worker_session_id.clone(),
    };
    let model = ModelOpenMessage {
        kind: ModelOpenMessageKind::ModelOpen,
        lease: lease.clone(),
        message_id: ExecutionMessageId(id("xmsg", seed + 1)),
        model_exchange_id: ModelExchangeId(id("mdl", seed)),
        request: encoded_payload(prompt),
        request_id: RequestId(id("req", seed + 1)),
        route: ModelGatewayRoute {
            capability: "reasoning".to_owned(),
            route: "production-delivery-route".to_owned(),
        },
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: submitted_at.clone(),
        session_identity: session_identity.clone(),
        worker_session_id: worker_session_id.clone(),
    };
    configure_stage_worker(&mut storage, &job, &model, queue_scope.clone(), seed);
    drop(storage);
    let active = active_lease_identity(
        job.job_id.clone(),
        u64::try_from(job.attempt).expect("positive attempt"),
        lease_id.clone(),
        fencing_token.clone(),
        worker_id.clone(),
        worker_instance_id.clone(),
        worker_session_id.clone(),
    );
    let authority =
        session_binding_authority(active, lease.issued_at.clone(), lease.expires_at.clone());
    let binding_message = SessionBindingMessage {
        attempt: job.attempt,
        bound_at: submitted_at.clone(),
        codex_thread_id,
        fencing_token,
        kind: SessionBindingMessageKind::SessionBinding,
        lease: lease.clone(),
        lease_id,
        message_id: ExecutionMessageId(id("xmsg", seed + 2)),
        product_session_id,
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: submitted_at,
        session_identity,
        source_identity: SessionBindingSourceIdentity {
            kind: SessionBindingSourceIdentityKind::ExecutionWorker,
            lease_id: lease.lease_id.clone(),
            worker_id: worker_id.clone(),
            worker_instance_id: worker_instance_id.clone(),
            worker_session_id: worker_session_id.clone(),
        },
        stage_run_id: Some(stage_run_id),
        worker_id,
        worker_session_id,
    };
    control_plane
        .commit_delivery_session_binding(&binding_message, &authority, &binding_message.sent_at)
        .expect("commit production SessionBinding");
    StageAuthority {
        job,
        queue_scope,
        binding: binding_message,
        authority,
        model,
        lease_terminal_request_id: RequestId(id("req", seed + 9)),
    }
}

fn load_current_stage_facts(
    root: &TestDirectory,
    control_plane: &mut ControlPlane,
    delivery_id: &DeliveryId,
) -> CurrentStageFacts {
    let delivery = load_delivery(control_plane, delivery_id);
    let run = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| {
            matches!(
                run.status,
                StageRunStatus::Running | StageRunStatus::Waiting
            )
        })
        .expect("active Codex StageRun");
    let binding = delivery
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| binding.stage_run_id == run.id)
        .expect("pending SessionBinding");
    let queue_scope = ExecutionQueueScope {
        organization_id: repository_scope().organization_id,
        workspace_id: repository_scope().workspace_id,
        project_id: repository_scope().project_id,
        repository_id: repository_scope().repository_id,
        product_session_id: binding.product_session_id.clone(),
        delivery_id: Some(delivery_id.clone()),
    };
    let mut storage = SqliteStorage::open(root.data()).expect("open stage storage");
    let page = storage
        .execution_queue()
        .expect("execution queue")
        .list_jobs(&queue_scope, &[], None, 100)
        .expect("list Delivery Jobs");
    let record = page
        .jobs
        .into_iter()
        .find(|record| record.job_id == binding.execution_job_id)
        .expect("current queued Delivery Job");
    let job: ExecutionJob =
        serde_json::from_slice(&record.dispatch_payload).expect("canonical ExecutionJob");
    CurrentStageFacts {
        stage_run_id: run.id.clone(),
        product_session_id: binding.product_session_id.clone(),
        job,
        queue_scope,
        submitted_at: record.submitted_at,
    }
}

fn configure_stage_worker(
    storage: &mut SqliteStorage,
    job: &ExecutionJob,
    message: &ModelOpenMessage,
    queue_scope: ExecutionQueueScope,
    seed: u64,
) {
    ensure_stage_worker(storage, message, seed);
    configure_stage_admission(storage, job, message, queue_scope, seed);
    claim_stage_lease_and_open_slot(storage, job, message, seed);
}

fn ensure_stage_worker(storage: &mut SqliteStorage, message: &ModelOpenMessage, seed: u64) {
    let current = storage
        .execution_registry()
        .expect("execution registry")
        .load_worker(&message.lease.worker_id)
        .expect("load live Worker");
    if current.is_none() {
        storage
            .execution_registry()
            .expect("execution registry")
            .register_worker(&WorkerRegistrationRequest {
                authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
                    control_plane_principal: "live-delivery-control-plane".to_owned(),
                },
                protocol_version: EXECUTION_PROTOCOL_VERSION.to_owned(),
                platform: WorkerPlatform::Aarch64AppleDarwin,
                capabilities: vec!["model".to_owned(), "delivery".to_owned()],
                capability_digest: digest('a'),
                security_zone: "local".to_owned(),
                max_slots: 8,
                message_id: ExecutionMessageId(id("xmsg", 9_800)),
                request_id: RequestId(id("req", 9_800)),
                sent_at: message.lease.issued_at.clone(),
                started_at: message.lease.issued_at.clone(),
                worker_id: message.lease.worker_id.clone(),
                worker_instance_id: message.lease.worker_instance_id.clone(),
            })
            .expect("register live Worker");
    }
    let current = storage
        .execution_registry()
        .expect("execution registry")
        .load_worker(&message.lease.worker_id)
        .expect("reload live Worker")
        .expect("live Worker");
    storage
        .execution_registry()
        .expect("execution registry")
        .record_heartbeat(&WorkerHeartbeatRequest {
            active_leases: Vec::new(),
            available_slots: current.available_slots,
            heartbeat_sequence: ExecutionSequence(
                i64::try_from(current.heartbeat_sequence + 1).expect("heartbeat sequence"),
            ),
            max_slots: current.max_slots,
            running_slots: current.running_slots,
            message_id: ExecutionMessageId(id("xmsg", seed + 3)),
            observed_at: message.lease.issued_at.clone(),
            sent_at: message.lease.issued_at.clone(),
            worker_id: message.lease.worker_id.clone(),
            worker_instance_id: message.lease.worker_instance_id.clone(),
        })
        .expect("heartbeat live Worker");
}

fn configure_stage_admission(
    storage: &mut SqliteStorage,
    job: &ExecutionJob,
    message: &ModelOpenMessage,
    queue_scope: ExecutionQueueScope,
    seed: u64,
) {
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 8,
        max_queued: 8,
        token_budget: 1_000_000,
        cost_budget_microunits: 1_000_000,
        max_runtime_millis: 3_600_000,
    };
    let pool = WorkerPoolId(id("wpl", 1));
    let boundaries = admission_boundaries(&queue_scope, &pool);
    {
        let mut admission = storage.execution_admission().expect("execution admission");
        for boundary in boundaries {
            admission
                .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
                .expect("configure execution policy");
        }
        admission
            .reserve(&ExecutionReservationRequest {
                scope: queue_scope.clone(),
                user_id: UserId(id("usr", 1)),
                worker_pool_id: pool.clone(),
                job_id: job.job_id.clone(),
                request_id: RequestId(id("req", seed + 4)),
                repository_access: ExecutionRepositoryAccess::IsolatedWrite {
                    worktree_key: format!("live-delivery:{}", job.job_id.0),
                },
                reserved_tokens: 100_000,
                reserved_cost_microunits: 100_000,
                runtime_limit_millis: 3_600_000,
                submitted_at: message.lease.issued_at.clone(),
            })
            .expect("reserve Delivery execution");
        admission
            .start(&ExecutionReservationStart {
                scope: queue_scope,
                worker_pool_id: pool,
                job_id: job.job_id.clone(),
                request_id: RequestId(id("req", seed + 5)),
                expected_revision: 1,
                started_at: message.lease.issued_at.clone(),
            })
            .expect("start Delivery execution");
    }
}

fn claim_stage_lease_and_open_slot(
    storage: &mut SqliteStorage,
    job: &ExecutionJob,
    message: &ModelOpenMessage,
    seed: u64,
) {
    storage
        .execution_registry()
        .expect("execution registry")
        .claim_execution_job(&ExecutionLeaseClaim {
            expires_at: message.lease.expires_at.clone(),
            fencing_token: message.lease.fencing_token.clone(),
            issued_at: message.lease.issued_at.clone(),
            job_id: job.job_id.clone(),
            lease_id: message.lease.lease_id.clone(),
            message_id: ExecutionMessageId(id("xmsg", seed + 6)),
            payload_digest: job.payload_digest.clone(),
            request_id: RequestId(id("req", seed + 6)),
            worker_id: message.lease.worker_id.clone(),
            worker_instance_id: message.lease.worker_instance_id.clone(),
            attempt: u64::try_from(job.attempt).expect("attempt"),
        })
        .expect("claim Delivery lease");
    let authority = slot_authority(message);
    let mut slots = storage.worker_session_slots().expect("Worker slots");
    slots
        .configure_resources(
            &authority.worker_id,
            &authority.worker_instance_id,
            WorkerSlotResourceLimits {
                max_memory_bytes: 1_000_000_000,
                max_disk_bytes: 1_000_000_000,
                max_processes: 64,
            },
        )
        .expect("configure Worker resources");
    slots
        .open(&WorkerSlotOpenRequest {
            authority,
            resources: WorkerSlotResources {
                memory_bytes: 1,
                disk_bytes: 1,
                process_slots: 1,
            },
            request_id: RequestId(id("req", seed + 7)),
            opened_at: message.lease.issued_at.clone(),
        })
        .expect("open Worker slot");
}

fn admission_boundaries(
    scope: &ExecutionQueueScope,
    pool: &WorkerPoolId,
) -> Vec<ExecutionAdmissionBoundary> {
    vec![
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
        ExecutionAdmissionBoundary::Delivery {
            organization_id: scope.organization_id.clone(),
            delivery_id: scope.delivery_id.clone().expect("Delivery scope"),
        },
        ExecutionAdmissionBoundary::ProductSession {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            product_session_id: scope.product_session_id.clone(),
        },
        ExecutionAdmissionBoundary::WorkerPool {
            organization_id: scope.organization_id.clone(),
            worker_pool_id: pool.clone(),
        },
    ]
}

fn slot_authority(message: &ModelOpenMessage) -> WorkerSlotAuthority {
    WorkerSlotAuthority {
        worker_id: message.lease.worker_id.clone(),
        worker_instance_id: message.lease.worker_instance_id.clone(),
        worker_session_id: message.worker_session_id.clone(),
        codex_thread_id: message.session_identity.codex_thread_id.clone(),
        job_id: message.lease.job_id.clone(),
        lease_id: message.lease.lease_id.clone(),
        attempt: u64::try_from(message.lease.attempt).expect("attempt"),
        fencing_token: message.lease.fencing_token.clone(),
    }
}

fn run_provider(
    application: &mut StandaloneModelExecutionApplication,
    stage: &StageAuthority,
) -> ProviderResult {
    let open = opened(
        application
            .accept_local(&open_frame(&stage.model))
            .expect("real Provider ModelOpen"),
    );
    let batch = application
        .complete_https_sse(&open, &stage.model.sent_at)
        .expect("real Provider SSE completion");
    let terminal = batch
        .flow
        .gateway_terminal
        .clone()
        .expect("real Provider terminal");
    assert_eq!(
        terminal.outcome,
        winwincode_control_plane::ProviderGatewayTerminalOutcome::Succeeded
    );
    let payloads = batch
        .chunks
        .iter()
        .filter_map(|chunk| chunk.payload.as_ref())
        .filter_map(|payload| STANDARD.decode(&payload.data_base64).ok())
        .filter_map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .collect::<Vec<_>>();
    let text = [
        "output_text_delta",
        "reasoning_summary_delta",
        "reasoning_content_delta",
    ]
    .into_iter()
    .find_map(|frame_type| {
        let text = payloads
            .iter()
            .filter(|value| value["type"] == frame_type)
            .filter_map(|value| value["delta"].as_str())
            .collect::<String>();
        (!text.trim().is_empty()).then_some(text)
    })
    .expect("real Provider returned no textual canonical frame");
    let final_sequence = batch
        .chunks
        .last()
        .expect("real Provider final chunk")
        .sequence
        .clone();
    ProviderResult {
        text,
        terminal,
        final_sequence,
    }
}

fn finish_model(
    application: &mut StandaloneModelExecutionApplication,
    stage: &StageAuthority,
    result: &ProviderResult,
) {
    let mut ack = final_ack(&stage.model, &result.final_sequence);
    ack.message_id = ExecutionMessageId(id(
        "xmsg",
        5_000
            + u64::try_from(stage.model.lease.attempt).expect("attempt")
            + stage
                .model
                .model_exchange_id
                .0
                .bytes()
                .map(u64::from)
                .sum::<u64>(),
    ));
    ack.sent_at = result.terminal.settled_at.clone();
    application
        .accept_local(
            &TypedFrame::new(
                FrameDirection::WorkerToControlPlane,
                ExecutionPortMessage::ModelAckMessage(ack),
            )
            .expect("typed final ModelAck"),
        )
        .expect("final ModelAck");
}

fn accept_runtime_payload(
    control_plane: &mut ControlPlane,
    scope: &RepositoryScope,
    stage: &StageAuthority,
    sequence: i64,
    category: ExecutionEventCategory,
    content_type: &str,
    bytes: &[u8],
) {
    let mut payload = encoded_payload(bytes);
    content_type.clone_into(&mut payload.content_type);
    let message = RuntimeEventMessage {
        codex_thread_id: stage.binding.codex_thread_id.clone(),
        event: ExecutionEventRecord {
            category,
            event_id: ExecutionEventId(id("xevt", runtime_seed(stage, sequence))),
            occurred_at: stage.binding.sent_at.clone(),
            payload: Some(payload),
            sequence: ExecutionSequence(sequence),
            summary: format!("durable live Provider event {sequence}"),
        },
        kind: RuntimeEventMessageKind::RuntimeEvent,
        lease: stage.binding.lease.clone(),
        message_id: ExecutionMessageId(id("xmsg", runtime_seed(stage, sequence) + 100)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: stage.binding.sent_at.clone(),
        session_identity: stage.binding.session_identity.clone(),
        worker_session_id: stage.binding.worker_session_id.clone(),
    };
    let ack = control_plane
        .accept_runtime_event(scope, &message, &stage.authority, &message.sent_at)
        .expect("accept production runtime event");
    assert!(matches!(
        ack.status,
        LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate
    ));
}

fn runtime_seed(stage: &StageAuthority, sequence: i64) -> u64 {
    let exchange_seed = stage
        .model
        .model_exchange_id
        .0
        .rsplit('_')
        .next()
        .expect("model exchange suffix")
        .parse::<u64>()
        .expect("numeric model exchange suffix");
    exchange_seed
        .saturating_mul(1_000)
        .saturating_add(u64::try_from(sequence).expect("positive runtime sequence"))
}

fn upload_candidate(
    control_plane: &mut ControlPlane,
    scope: &RepositoryScope,
    stage: &StageAuthority,
    commit: &str,
    seed: u64,
) -> ArtifactReference {
    let bytes = CandidateSourceManifest::new(commit.to_owned())
        .expect("candidate manifest")
        .encode()
        .expect("candidate bytes");
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes)));
    let artifact_id = ArtifactId(id("art", seed));
    let open = ArtifactOpenMessage {
        artifact: ArtifactDescriptor {
            artifact_id: artifact_id.clone(),
            digest: digest.clone(),
            file_name: Some("candidate.json".to_owned()),
            kind: ArtifactKind::Candidate,
            media_type: CANDIDATE_MEDIA_TYPE.to_owned(),
            size_bytes: i64::try_from(bytes.len()).expect("candidate size"),
        },
        kind: ArtifactOpenMessageKind::ArtifactOpen,
        lease: stage.binding.lease.clone(),
        message_id: ExecutionMessageId(id("xmsg", seed)),
        request_id: RequestId(id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: stage.binding.sent_at.clone(),
        session_identity: stage.binding.session_identity.clone(),
        worker_session_id: stage.binding.worker_session_id.clone(),
    };
    let opened = control_plane
        .accept_artifact_open(scope, &open, &stage.authority)
        .expect("open production candidate Artifact");
    assert!(matches!(
        opened.status,
        LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate
    ));
    let chunk = ArtifactChunkMessage {
        artifact_id: artifact_id.clone(),
        is_final: true,
        kind: ArtifactChunkMessageKind::ArtifactChunk,
        lease: stage.binding.lease.clone(),
        message_id: ExecutionMessageId(id("xmsg", seed + 1)),
        payload: encoded_payload(&bytes),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: stage.binding.sent_at.clone(),
        sequence: ExecutionSequence(1),
        session_identity: stage.binding.session_identity.clone(),
        worker_session_id: stage.binding.worker_session_id.clone(),
    };
    let accepted = control_plane
        .accept_artifact_chunk(scope, &chunk, &stage.authority)
        .expect("complete production candidate Artifact");
    assert!(matches!(
        accepted.status,
        LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate
    ));
    ArtifactReference {
        artifact_id,
        digest,
    }
}

fn commit_stage_success(
    control_plane: &mut ControlPlane,
    scope: &RepositoryScope,
    stage: &StageAuthority,
    provider: &ProviderResult,
    artifacts: Vec<ArtifactReference>,
    last_event_sequence: i64,
) -> winwincode_delivery::application::stage::DeliveryTerminalOutcomeFacts {
    let message = JobOutcomeMessage {
        kind: JobOutcomeMessageKind::JobOutcome,
        lease: stage.binding.lease.clone(),
        message_id: ExecutionMessageId(id("xmsg", runtime_seed(stage, 90))),
        outcome: ExecutionOutcome {
            artifacts: artifacts.clone(),
            codex_thread_id: Some(stage.binding.codex_thread_id.clone()),
            error: None,
            finished_at: provider.terminal.settled_at.clone(),
            last_event_sequence: ExecutionAckSequence(last_event_sequence),
            status: ExecutionOutcomeStatus::Succeeded,
            summary: "real Provider Delivery stage completed".to_owned(),
            usage: Some(ExecutionOutcomeUsage {
                cost_microunits: i64::try_from(provider.terminal.admission.actual_cost_micros)
                    .expect("safe Provider cost"),
                runtime_millis: 1,
                tokens: i64::try_from(provider.terminal.admission.actual_tokens)
                    .expect("safe Provider tokens"),
            }),
        },
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: provider.terminal.settled_at.clone(),
        session_identity: stage.binding.session_identity.clone(),
        worker_session_id: stage.binding.worker_session_id.clone(),
    };
    let facts = reconcile_durable_terminal_outcome(
        &load_delivery(
            control_plane,
            match &stage.job.scope {
                ExecutionScope::DeliveryStageExecutionScope(scope) => &scope.delivery_id,
                ExecutionScope::ProductSessionExecutionScope(_) => {
                    panic!("live Delivery stage must have Delivery scope")
                }
            },
        ),
        DurableTerminalOutcomeInput {
            execution_job_id: stage.job.job_id.clone(),
            attempt: u64::try_from(stage.job.attempt).expect("attempt"),
            lease_id: stage.binding.lease.lease_id.clone(),
            fencing_token: stage.binding.lease.fencing_token.clone(),
            worker_id: stage.binding.lease.worker_id.clone(),
            worker_instance_id: stage.binding.lease.worker_instance_id.clone(),
            worker_session_id: stage.binding.worker_session_id.clone(),
            issued_at: stage.binding.lease.issued_at.clone(),
            expires_at: stage.binding.lease.expires_at.clone(),
            stage_run_id: stage.binding.stage_run_id.clone().expect("StageRun"),
            status: TerminalOutcomeStatus::Succeeded,
            codex_thread_id: Some(stage.binding.codex_thread_id.clone()),
            finished_at_millis: instant_millis(&provider.terminal.settled_at),
            last_event_sequence: ExecutionAckSequence(last_event_sequence),
            artifacts: artifacts
                .into_iter()
                .map(|artifact| TerminalArtifactReference {
                    artifact_id: artifact.artifact_id,
                    digest: artifact.digest,
                })
                .collect(),
        },
    )
    .expect("seal durable terminal outcome");
    control_plane
        .commit_delivery_terminal_outcome(scope, &message, &facts, &message.sent_at)
        .expect("commit Delivery terminal outcome");
    facts
}

fn instant_millis(value: &Instant) -> u64 {
    let connection = rusqlite::Connection::open_in_memory().expect("open timestamp parser");
    let seconds: i64 = connection
        .query_row(
            "SELECT CAST(strftime('%s', ?1) AS INTEGER)",
            [&value.0],
            |row| row.get(0),
        )
        .expect("parse UTC timestamp");
    let fraction = value
        .0
        .split_once('.')
        .map_or("0", |(_, suffix)| suffix.trim_end_matches('Z'));
    let millis = fraction
        .bytes()
        .take(3)
        .fold((0_u64, 0_u32), |(value, digits), byte| {
            (
                value
                    .saturating_mul(10)
                    .saturating_add(u64::from(byte.saturating_sub(b'0'))),
                digits + 1,
            )
        });
    let fractional_millis = millis.0.saturating_mul(10_u64.pow(3 - millis.1));
    u64::try_from(seconds)
        .expect("post-epoch timestamp")
        .saturating_mul(1_000)
        .saturating_add(fractional_millis)
}

fn planner_prompt(private_input: &str) -> Vec<u8> {
    anthropic_prompt(
        "planner",
        "Design one minimal Rust source change that satisfies the Delivery criterion. Reply with a concise plain-text plan grounded in the requested repository change.",
        Some(private_input),
    )
}

fn executor_prompt() -> Vec<u8> {
    anthropic_prompt(
        "executor",
        "Author the implementation for a Rust function that returns the number two. Reply with a concise explanation; the production adapter will apply the reviewed change.",
        None,
    )
}

fn verification_prompt(role: &str, candidate_ref: &str) -> Vec<u8> {
    anthropic_prompt(
        role,
        &format!(
            "Independently evaluate candidate {candidate_ref}. Reply with a concise pass explanation for the criterion after checking the immutable candidate identity."
        ),
        None,
    )
}

fn anthropic_prompt(role: &str, instructions: &str, private_input: Option<&str>) -> Vec<u8> {
    let user_text = private_input.map_or_else(
        || "Return the requested bounded result. Do not include credentials or raw request metadata.".to_owned(),
        |value| format!("Use this private requirement marker when planning, but do not repeat it in the response: {value}"),
    );
    serde_json::to_vec(&serde_json::json!({
        "requestId": format!("live-delivery-{role}"),
        "provider": "winwincode",
        "sessionId": format!("live-delivery-session-{role}"),
        "threadId": format!("live-delivery-thread-{role}"),
        "turnId": format!("live-delivery-turn-{role}"),
        "request": {
            "model": "canonical-live-delivery-model",
            "instructions": instructions,
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": user_text
                }]
            }],
            "tools": [],
            "tool_choice": "none",
            "parallel_tool_calls": false,
            "reasoning": {"effort": "high", "summary": "auto"},
            "store": false,
            "stream": true,
            "stream_options": null,
            "include": [],
            "service_tier": null,
            "prompt_cache_key": null,
            "text": null,
            "client_metadata": null
        }
    }))
    .expect("canonical live Provider prompt")
}

fn bounded_provider_text(text: &str) -> String {
    let value = text
        .chars()
        .filter(|character| !matches!(u32::from(*character), 0..=8 | 11..=12 | 14..=31 | 127))
        .take(8_192)
        .collect::<String>();
    let value = value.trim();
    if value.is_empty() {
        "Real Provider returned a successful bounded response.".to_owned()
    } else {
        value.to_owned()
    }
}

fn canonical_planner_solution(provider_text: &str) -> Vec<u8> {
    let summary = serde_json::to_string(&bounded_provider_text(provider_text))
        .expect("encode Provider plan summary");
    concat!(
            "{\"schemaVersion\":1,",
            "\"protocol\":\"winwincode.planner-solution.v1\",",
            "\"solution\":{",
            "\"id\":\"solution:live-provider\",",
            "\"summary\":__SUMMARY__,",
            "\"approach\":[\"Apply one reviewed source change and verify it through the canonical Delivery stages.\"],",
            "\"components\":[{",
            "\"id\":\"component:live-provider-candidate\",",
            "\"label\":\"Live Provider candidate\",",
            "\"responsibility\":\"Implements the accepted Delivery requirement in the repository.\",",
            "\"kind\":\"component\",",
            "\"trustBoundary\":\"repository\",",
            "\"unresolved\":false,",
            "\"repositoryPathPrefixes\":[\"src\"]",
            "}],",
            "\"connections\":[{",
            "\"id\":\"connection:live-provider\",",
            "\"from\":\"platform:codex-core\",",
            "\"to\":\"component:live-provider-candidate\",",
            "\"label\":\"executes\"",
            "}]",
            "},",
            "\"architectureDiagram\":{",
            "\"id\":\"diagram:live-architecture\",",
            "\"kind\":\"system-architecture\",",
            "\"title\":\"Live Provider architecture\",",
            "\"nodes\":[{",
            "\"id\":\"diagram:live-architecture:input\",",
            "\"label\":\"Provider input\",",
            "\"description\":\"Consumes the reviewed Delivery requirement.\",",
            "\"kind\":\"stage\",",
            "\"trustBoundary\":null,",
            "\"unresolved\":false",
            "},{",
            "\"id\":\"diagram:live-architecture:output\",",
            "\"label\":\"Candidate output\",",
            "\"description\":\"Produces the immutable candidate for independent verification.\",",
            "\"kind\":\"decision\",",
            "\"trustBoundary\":\"repository\",",
            "\"unresolved\":false",
            "}],",
            "\"edges\":[{",
            "\"id\":\"diagram:live-architecture:edge\",",
            "\"from\":\"diagram:live-architecture:input\",",
            "\"to\":\"diagram:live-architecture:output\",",
            "\"label\":\"implements\"",
            "}]",
            "},",
            "\"processDiagram\":{",
            "\"id\":\"diagram:live-process\",",
            "\"kind\":\"process-flow\",",
            "\"title\":\"Live Provider process\",",
            "\"nodes\":[{",
            "\"id\":\"diagram:live-process:input\",",
            "\"label\":\"Provider input\",",
            "\"description\":\"Consumes the reviewed Delivery requirement.\",",
            "\"kind\":\"stage\",",
            "\"trustBoundary\":null,",
            "\"unresolved\":false",
            "},{",
            "\"id\":\"diagram:live-process:output\",",
            "\"label\":\"Candidate output\",",
            "\"description\":\"Produces the immutable candidate for independent verification.\",",
            "\"kind\":\"decision\",",
            "\"trustBoundary\":\"repository\",",
            "\"unresolved\":false",
            "}],",
            "\"edges\":[{",
            "\"id\":\"diagram:live-process:edge\",",
            "\"from\":\"diagram:live-process:input\",",
            "\"to\":\"diagram:live-process:output\",",
            "\"label\":\"implements\"",
            "}]",
            "},",
            "\"risks\":[\"The external Provider response must remain bound to the exact durable request.\"],",
            "\"unresolvedItems\":[],",
            "\"taskProposals\":[{",
            "\"id\":\"dtk_00000000000000000000000001\",",
            "\"title\":\"Implement the real Provider candidate\",",
            "\"goal\":\"Complete one real Provider Delivery end to end\",",
            "\"acceptanceCriterionIds\":[\"criterion-live-provider\"],",
            "\"blockedByTaskIds\":[]",
            "}]",
            "}"
        )
    .replace("__SUMMARY__", &summary)
    .into_bytes()
}

fn provider_candidate_evidence(provider_text: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 1,
        "protocol": "winwincode.provider-candidate-evidence.v1",
        "providerTextSha256": format!("sha256:{:x}", Sha256::digest(provider_text.as_bytes())),
    }))
    .expect("encode Provider candidate evidence")
}

fn commit_provider_candidate(repository: &Path, provider_text: &str) -> String {
    let explanation = serde_json::to_string(&bounded_provider_text(provider_text))
        .expect("encode Provider explanation");
    fs::write(
        repository.join("src/lib.rs"),
        format!(
            "pub fn baseline() -> u64 {{ 2 }}\n\npub const PROVIDER_EXPLANATION: &str = {explanation};\n"
        ),
    )
    .expect("write Provider-authored candidate");
    for arguments in [
        &["add", "src/lib.rs"][..],
        &["commit", "-q", "-m", "real Provider candidate"][..],
    ] {
        let status = Command::new("git")
            .args(arguments)
            .current_dir(repository)
            .status()
            .expect("commit Provider candidate");
        assert!(
            status.success(),
            "Git candidate command failed: {arguments:?}"
        );
    }
    git_output(repository, &["rev-parse", "HEAD"])
}

fn accept_verification_events(
    control_plane: &mut ControlPlane,
    scope: &RepositoryScope,
    stage: &StageAuthority,
    candidate_ref: &str,
    role: &str,
    provider_text: &str,
) {
    let delivery = load_delivery(
        control_plane,
        match &stage.job.scope {
            ExecutionScope::DeliveryStageExecutionScope(scope) => &scope.delivery_id,
            ExecutionScope::ProductSessionExecutionScope(_) => panic!("Delivery stage scope"),
        },
    );
    let criterion = delivery
        .snapshot()
        .spec
        .acceptance_criteria
        .first()
        .expect("Delivery criterion");
    let source_event_id = ExecutionEventId(id("xevt", runtime_seed(stage, 3)));
    let events = [
        (
            ExecutionEventCategory::Lifecycle,
            serde_json::json!({
                "protocol": "winwincode.verification-session-policy.v1",
                "workspace_mode": "candidate-read-only",
                "permission_profile": "candidate-read-only-restricted",
                "candidate_ref": candidate_ref
            }),
        ),
        (
            ExecutionEventCategory::Activity,
            serde_json::json!({"status": "candidate-inspected"}),
        ),
        (
            if role == "reviewer" {
                ExecutionEventCategory::Command
            } else {
                ExecutionEventCategory::Test
            },
            serde_json::json!({"status": "completed", "exit_code": 0}),
        ),
        (
            ExecutionEventCategory::Activity,
            serde_json::json!({
                "protocol": "winwincode.independent-verification-result.v1",
                "delivery_spec_id": delivery.snapshot().spec.id,
                "delivery_spec_revision": delivery.snapshot().spec.revision,
                "candidate_ref": candidate_ref,
                "findings": [{
                    "finding_id": format!("finding-{role}-live-provider"),
                    "criterion_id": criterion.id,
                    "verdict": "pass",
                    "explanation": bounded_provider_text(provider_text),
                    "evidence_sources": [{
                        "type": if role == "reviewer" { "command" } else { "test" },
                        "event_id": source_event_id
                    }]
                }]
            }),
        ),
    ];
    for (index, (category, payload)) in events.into_iter().enumerate() {
        accept_runtime_payload(
            control_plane,
            scope,
            stage,
            i64::try_from(index + 1).expect("event sequence"),
            category,
            "application/json",
            &serde_json::to_vec(&payload).expect("verification event"),
        );
    }
}

fn settle_stage_resources(root: &TestDirectory, stage: &StageAuthority, provider: &ProviderResult) {
    let pool = WorkerPoolId(id("wpl", 1));
    let mut storage = SqliteStorage::open(root.data()).expect("open resource settlement storage");
    storage
        .execution_admission()
        .expect("execution admission")
        .settle(&ExecutionReservationSettlement {
            scope: stage.queue_scope.clone(),
            worker_pool_id: pool,
            job_id: stage.job.job_id.clone(),
            request_id: RequestId(id("req", runtime_seed(stage, 91))),
            expected_revision: 2,
            actual_tokens: provider.terminal.admission.actual_tokens,
            actual_cost_microunits: provider.terminal.admission.actual_cost_micros,
            actual_runtime_millis: 1,
            completed_at: provider.terminal.settled_at.clone(),
        })
        .expect("settle Delivery execution reservation");
    storage
        .worker_session_slots()
        .expect("Worker slots")
        .close(&WorkerSlotCloseRequest {
            authority: slot_authority(&stage.model),
            request_id: RequestId(id("req", runtime_seed(stage, 92))),
            expected_revision: 1,
            outcome: WorkerSlotState::Completed,
            closed_at: provider.terminal.settled_at.clone(),
        })
        .expect("close Delivery Worker slot");
    assert!(
        storage
            .execution_registry()
            .expect("execution registry")
            .finish_execution_lease(&ExecutionLeaseTerminalRequest {
                job_id: stage.job.job_id.clone(),
                lease_id: stage.model.lease.lease_id.clone(),
                worker_id: stage.model.lease.worker_id.clone(),
                worker_instance_id: stage.model.lease.worker_instance_id.clone(),
                attempt: u64::try_from(stage.model.lease.attempt).expect("attempt"),
                fencing_token: stage.model.lease.fencing_token.clone(),
                outcome: ExecutionLeaseTerminalOutcome::Completed,
                terminal_at: provider.terminal.settled_at.clone(),
                request_id: stage.lease_terminal_request_id.clone(),
            })
            .expect("finish Delivery execution lease")
    );
}

fn provider_usage_for(
    root: &TestDirectory,
    exchange_id: &ModelExchangeId,
) -> winwincode_control_plane::ModelUsageSourceEntry {
    let mut storage = SqliteStorage::open(root.data()).expect("open Usage source storage");
    let page = ModelRetryUsageService::new(&mut storage)
        .scan_usage_sources(
            &ModelUsageFilter {
                delivery_id: Some(DeliveryId(id("dlv", 1))),
                ..ModelUsageFilter::default()
            },
            None,
            100,
        )
        .expect("scan Provider Usage sources");
    page.entries
        .into_iter()
        .find(|entry| &entry.model_exchange_id == exchange_id)
        .expect("exact Provider Usage source")
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LiveEvidence {
    schema: &'static str,
    data_directory: PathBuf,
    repository_root: PathBuf,
    delivery_id: DeliveryId,
    provider_runs: Vec<ProviderRunEvidence>,
    pool_config: PoolConfigEvidence,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderRunEvidence {
    execution_job_id: ExecutionJobId,
    model_exchange_id: ModelExchangeId,
    provider_request_id: RequestId,
    provider_usage_id: String,
    worker_session_id: WorkerSessionId,
    lease_id: LeaseId,
    model_admission_terminal_request_id: RequestId,
    lease_terminal_request_id: RequestId,
    budget_period_id: &'static str,
    expected_usage: ExpectedUsage,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ExpectedUsage {
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_write_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    cost_micros: u64,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct PoolConfigEvidence {
    max_routes: usize,
    max_active_per_route: usize,
    max_waiting_per_route: usize,
    max_exchange_records_per_route: usize,
    max_buffered_frames_per_stream: usize,
    max_buffered_bytes_per_stream: usize,
    resume_buffered_frames_per_stream: usize,
    resume_buffered_bytes_per_stream: usize,
}

fn live_evidence(
    root: &TestDirectory,
    repository: &Path,
    delivery: &Delivery,
    runs: &[(&StageAuthority, &ProviderResult); 4],
) -> LiveEvidence {
    let provider_runs = runs
        .iter()
        .map(|(stage, result)| {
            let source = provider_usage_for(root, &stage.model.model_exchange_id);
            assert_eq!(source.request_id, stage.model.request_id);
            assert_eq!(
                source.usage.total_tokens,
                result.terminal.admission.actual_tokens
            );
            assert_eq!(
                source.usage.cost_micros,
                result.terminal.admission.actual_cost_micros
            );
            ProviderRunEvidence {
                execution_job_id: stage.job.job_id.clone(),
                model_exchange_id: stage.model.model_exchange_id.clone(),
                provider_request_id: stage.model.request_id.clone(),
                provider_usage_id: source.usage.provider_usage_id,
                worker_session_id: stage.model.worker_session_id.clone(),
                lease_id: stage.model.lease.lease_id.clone(),
                model_admission_terminal_request_id: result.terminal.admission.request_id.clone(),
                lease_terminal_request_id: stage.lease_terminal_request_id.clone(),
                budget_period_id: "budget-2030-01",
                expected_usage: ExpectedUsage {
                    input_tokens: source.usage.input_tokens,
                    cached_input_tokens: source.usage.cached_input_tokens,
                    cache_write_input_tokens: source.usage.cache_write_input_tokens,
                    output_tokens: source.usage.output_tokens,
                    reasoning_output_tokens: source.usage.reasoning_output_tokens,
                    cost_micros: source.usage.cost_micros,
                },
            }
        })
        .collect();
    let config = pool_config();
    LiveEvidence {
        schema: "winwincode.live-provider-delivery-evidence.v1",
        data_directory: root.data(),
        repository_root: repository.to_path_buf(),
        delivery_id: delivery.id().clone(),
        provider_runs,
        pool_config: PoolConfigEvidence {
            max_routes: config.max_routes,
            max_active_per_route: config.max_active_per_route,
            max_waiting_per_route: config.max_waiting_per_route,
            max_exchange_records_per_route: config.max_exchange_records_per_route,
            max_buffered_frames_per_stream: config.max_buffered_frames_per_stream,
            max_buffered_bytes_per_stream: config.max_buffered_bytes_per_stream,
            resume_buffered_frames_per_stream: config.resume_buffered_frames_per_stream,
            resume_buffered_bytes_per_stream: config.resume_buffered_bytes_per_stream,
        },
    }
}

fn write_evidence(path: &Path, evidence: &LiveEvidence) {
    fs::create_dir_all(path.parent().expect("evidence parent")).expect("create evidence parent");
    let pending = path.with_extension("pending");
    fs::write(
        &pending,
        serde_json::to_vec_pretty(evidence).expect("encode live Delivery evidence"),
    )
    .expect("write live Delivery evidence");
    #[cfg(unix)]
    fs::set_permissions(&pending, fs::Permissions::from_mode(0o600))
        .expect("set private evidence mode");
    fs::rename(pending, path).expect("publish live Delivery evidence");
}
