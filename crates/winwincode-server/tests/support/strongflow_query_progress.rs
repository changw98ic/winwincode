// SPDX-License-Identifier: Apache-2.0

//! Durable source fixture, using the same journal/artifact seed pattern as
//! `winwincode-control-plane/tests/delivery_verdict_authority.rs`.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

#[path = "../../../../tests/support/git_candidate.rs"]
mod git_candidate;
use git_candidate::candidate_bundle;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{Actor, QueryRequest, Scope};
use winwincode_control_plane::{
    ControlPlane, ControlPlaneConfig, DurableWorkerInteractionOutbound, EventPublishError,
    EventPublisher, OutboxEvent, ProductSessionExecutionConfig,
    strongflow_projection::{
        SqliteTrustedPublicationProjectionAdapter, SqliteTrustedRuntimeProjectionAdapter,
        StrongFlowProjectionQueryPort, StrongFlowProjectionSources,
    },
};
use winwincode_delivery::{
    application::verdict::test_support::{VerdictFixtureOutcome, verdict_fixture},
    domain::{DELIVERY_SCHEMA_VERSION, Delivery, RepositoryKind, RepositoryRef, SessionBinding},
    store::{
        AtomicPublication, CreateDelivery, DeliveryCommand, DeliveryCommandPort,
        DeliveryJournalPort, DeliveryStore, JournalBackendError, LoadedDeliveryJournal,
    },
};
use winwincode_domain::{
    ArtifactId, DeliveryId, ExecutionAckSequence, ExecutionMessageId, ExecutionSequence, Instant,
    OrganizationId, ProjectId, RepositoryId, RepositoryScope, RepositoryScopeKind, RequestId,
    SchemaVersion, Sha256Digest, UserActor, UserActorKind, UserId, WorkerId, WorkerInstanceId,
    WorkspaceId,
};
use winwincode_execution_port::generated::{
    ArtifactReference, ExecutionOutcomeStatus, ExecutionPortMessage, WorkerCapacity,
    WorkerHeartbeatMessage, WorkerHeartbeatMessageKind,
};
use winwincode_server::{
    AuthenticatedPrincipal, DurableEventHub, DurableEventHubConfig, StandaloneApplicationClock,
    StandaloneControlPlaneApplication,
};
use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, ArtifactAccess,
    ArtifactChunk, ArtifactError, ArtifactMeteringAttribution, ArtifactObject, ArtifactOpen,
    ArtifactProvenance, ArtifactRetention, ArtifactStore, EXECUTION_PROTOCOL_VERSION,
    GitCandidateArtifactManifest, GitSourceResolver, LocalArtifactObjectStore,
    LocalGitSourceResolver, NewOutboxEvent, ProductStateStorage, PublicEventScope, ReceiptActorKey,
    ReceiptIdentity, ReceiptScopeKey, SqliteStorage, StateCommit, StateMutation,
    ValidatedGitSourceArtifact, WorkerAuthenticationIdentity, WorkerPlatform,
    WorkerRegistrationRequest, WorkerRegistrationStatus, receipt_scope_key,
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);
const CANDIDATE_MEDIA_TYPE: &str = "application/vnd.winwincode.git-candidate+json";

#[derive(Default)]
pub struct SourceGate {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

impl SourceGate {
    pub fn wait_until_entered(&self, timeout: Duration) -> bool {
        let state = self.state.lock().expect("source barrier lock");
        self.changed
            .wait_timeout_while(state, timeout, |state| !state.0)
            .expect("wait for source resolver")
            .0
            .0
    }

    pub fn release(&self) {
        self.state.lock().expect("source barrier lock").1 = true;
        self.changed.notify_all();
    }

    fn pause_once(&self) {
        let mut state = self.state.lock().expect("source barrier lock");
        if state.0 {
            return;
        }
        state.0 = true;
        self.changed.notify_all();
        let (state, timed_out) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(30), |state| !state.1)
            .expect("wait for source release");
        assert!(!timed_out.timed_out() && state.1, "source barrier watchdog");
    }
}

struct SlowSourceResolver {
    inner: LocalGitSourceResolver,
    gate: Arc<SourceGate>,
}

impl GitSourceResolver for SlowSourceResolver {
    fn resolve_candidate(
        &self,
        artifact: &ArtifactObject,
        repository_locator: &str,
        base_revision: &str,
    ) -> Result<ValidatedGitSourceArtifact, ArtifactError> {
        self.gate.pause_once();
        self.inner
            .resolve_candidate(artifact, repository_locator, base_revision)
    }

    fn controlled_repository_root(&self) -> Option<&Path> {
        Some(self.inner.controlled_repository_root())
    }
}

struct HubPublisher(Arc<DurableEventHub>);

impl EventPublisher for HubPublisher {
    fn publish(&mut self, event: &OutboxEvent) -> Result<(), EventPublishError> {
        self.0
            .publish_committed(event)
            .map(|_| ())
            .map_err(|error| EventPublishError::new(error.to_string()))
    }
}

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
    fn now_instant(&self) -> Instant {
        now()
    }
}

pub struct Fixture {
    root: PathBuf,
    pub application: Arc<StandaloneControlPlaneApplication>,
    pub gate: Arc<SourceGate>,
    pub scope: RepositoryScope,
    pub delivery_id: DeliveryId,
    pub worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    pin_reference: String,
}

impl Fixture {
    pub fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "winwincode-query-progress-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        let data = root.join("data");
        let repository = root.join("repository");
        let (base, candidate) = initialize_repository(&repository);
        fs::create_dir_all(&data).expect("data directory");
        let repository = fs::canonicalize(repository).expect("canonical repository");
        let scope = repository_scope(77);
        let delivery = fixture_delivery(&repository, base);
        seed_delivery(&data, &scope, &delivery);
        let pin_reference = seed_candidate(&data, &repository, &scope, &delivery, &candidate);
        preflight_query(&data, &root, &scope, &delivery);
        let mut storage = SqliteStorage::open(&data).expect("application storage");
        let worker_id = WorkerId(canonical_id("wrk", 90_000));
        let worker_instance_id = WorkerInstanceId(canonical_id("wki", 90_000));
        let registration = storage
            .execution_registry()
            .expect("Worker registry")
            .register_worker(&WorkerRegistrationRequest {
                authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
                    control_plane_principal: "query-progress-fixture".into(),
                },
                protocol_version: EXECUTION_PROTOCOL_VERSION.into(),
                platform: WorkerPlatform::Aarch64AppleDarwin,
                capabilities: vec!["codex".into()],
                capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
                security_zone: "local".into(),
                max_slots: 1,
                message_id: ExecutionMessageId(canonical_id("xmsg", 90_000)),
                request_id: RequestId(canonical_id("req", 90_000)),
                sent_at: now(),
                started_at: now(),
                worker_id: worker_id.clone(),
                worker_instance_id: worker_instance_id.clone(),
            })
            .expect("register independent Worker");
        assert_eq!(registration.status, WorkerRegistrationStatus::Accepted);
        let hub = Arc::new(
            DurableEventHub::open(data.join("events"), DurableEventHubConfig::default())
                .expect("event hub"),
        );
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&data),
            Box::new(HubPublisher(Arc::clone(&hub))),
        )
        .expect("local Control Plane");
        control_plane
            .install_strongflow_projection_sources(StrongFlowProjectionSources::new(
                Box::new(SqliteTrustedRuntimeProjectionAdapter::from_sqlite_storage()),
                Box::new(SqliteTrustedPublicationProjectionAdapter),
            ))
            .expect("production projection sources");
        let gate = Arc::new(SourceGate::default());
        control_plane
            .install_git_source_resolver(Box::new(SlowSourceResolver {
                inner: LocalGitSourceResolver::open(&root).expect("real Git source resolver"),
                gate: Arc::clone(&gate),
            }))
            .expect("one trusted source resolver");
        let outbound = DurableWorkerInteractionOutbound::new(
            SqliteStorage::open(&data).expect("outbound storage"),
            winwincode_storage::WorkerOutboundQueueConfig::default(),
        )
        .expect("canonical Worker outbound");
        let execution = ProductSessionExecutionConfig::try_new(
            scope.clone(),
            "fixture-checkout-revision",
            "codex-chat",
            Some(3_600),
            1_073_741_824,
        )
        .expect("execution config");
        let application = StandaloneControlPlaneApplication::new_with_clock(
            control_plane,
            storage,
            outbound,
            hub,
            Arc::new(FixedClock),
            execution,
        )
        .expect("real application composition");
        Self {
            root,
            application: Arc::new(application),
            gate,
            scope,
            delivery_id: delivery.id().clone(),
            worker_id,
            worker_instance_id,
            pin_reference,
        }
    }

    pub fn principal(&self) -> AuthenticatedPrincipal {
        AuthenticatedPrincipal::new(
            Actor::UserActor(UserActor {
                id: UserId(canonical_id("usr", 77)),
                kind: UserActorKind::User,
            }),
            vec![serde_json::from_value::<Scope>(json!(self.scope)).expect("API scope")],
        )
        .expect("authenticated principal")
    }

    pub fn delivery_get(&self, cursor: Option<&Value>, request: u64) -> QueryRequest {
        serde_json::from_value(json!({
            "schemaVersion": "winwincode/v1", "requestId": canonical_id("req", request),
            "actor": {"kind": "user", "id": canonical_id("usr", 77)},
            "scope": self.scope, "query": "delivery.get",
            "parameters": {"deliveryId": self.delivery_id, "atCursor": cursor},
            "page": {"cursor": null, "limit": 20}
        }))
        .expect("generated delivery.get envelope")
    }

    pub fn candidate_history(&self, cursor: &Value, request: u64) -> QueryRequest {
        serde_json::from_value(json!({
            "schemaVersion": "winwincode/v1", "requestId": canonical_id("req", request),
            "actor": {"kind": "user", "id": canonical_id("usr", 77)},
            "scope": self.scope, "query": "candidate.list",
            "parameters": {"deliveryId": self.delivery_id, "atCursor": cursor, "readPageLimit": 20},
            "page": {"cursor": null, "limit": 20}
        }))
        .expect("generated candidate.list envelope")
    }

    pub fn remove_pin(&self) {
        git(
            &self.root.join("repository"),
            &["update-ref", "-d", &self.pin_reference],
        );
    }

    pub fn pin_is_missing(&self) -> bool {
        let output = Command::new("git")
            .current_dir(self.root.join("repository"))
            .args(["rev-parse", "--verify", "--quiet", &self.pin_reference])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .expect("read pin ref");
        output.status.code() == Some(1) && output.stdout.is_empty()
    }

    pub fn heartbeat(&self) -> ExecutionPortMessage {
        ExecutionPortMessage::WorkerHeartbeatMessage(WorkerHeartbeatMessage {
            active_leases: vec![],
            capacity: WorkerCapacity {
                available_slots: 1,
                running_jobs: 0,
            },
            heartbeat_sequence: ExecutionSequence(1),
            kind: WorkerHeartbeatMessageKind::WorkerHeartbeat,
            message_id: ExecutionMessageId(canonical_id("xmsg", 90_001)),
            observed_at: now(),
            sent_at: now(),
            schema_version: SchemaVersion::WinwincodeV1,
            worker_id: self.worker_id.clone(),
            worker_instance_id: self.worker_instance_id.clone(),
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.gate.release();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn now() -> Instant {
    Instant("2027-01-15T08:00:00.000Z".into())
}
fn canonical_id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SeedCatalogEntry<'entry> {
    schema_version: u8,
    repository_scope: &'entry RepositoryScope,
    delivery_id: &'entry DeliveryId,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SeedTerminalAuthority<'authority> {
    schema_version: u8,
    delivery_id: &'authority DeliveryId,
    work_run_id: &'authority winwincode_domain::WorkRunId,
    job_id: &'authority winwincode_domain::ExecutionJobId,
    attempt: u64,
    lease_id: &'authority winwincode_domain::LeaseId,
    fencing_token: &'authority winwincode_domain::FencingToken,
    worker_id: &'authority WorkerId,
    worker_instance_id: &'authority WorkerInstanceId,
    worker_session_id: &'authority winwincode_domain::WorkerSessionId,
    issued_at: Instant,
    expires_at: Instant,
    artifacts: Vec<ArtifactReference>,
    codex_thread_id: &'authority Option<winwincode_domain::CodexThreadId>,
    finished_at_millis: u64,
    last_event_sequence: ExecutionAckSequence,
    status: ExecutionOutcomeStatus,
    disposition: SeedTerminalDisposition,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SeedTerminalDisposition {
    Settled { delivery_revision: u64 },
}

fn seed_candidate(
    data: &Path,
    repository: &Path,
    scope: &RepositoryScope,
    delivery: &Delivery,
    candidate: &str,
) -> String {
    let objects = LocalArtifactObjectStore::open(data.join("artifacts")).expect("Artifact objects");
    let mut artifacts = ArtifactStore::open(data.join("artifact-catalog"), Box::new(objects))
        .expect("Artifact catalog");
    let resolver = LocalGitSourceResolver::open(repository.parent().expect("repository parent"))
        .expect("fixture source resolver");
    let writer = delivery
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| binding.execution_profile.as_deref() == Some("executor"))
        .expect("candidate-producing writer");
    let finished_at = writer.bound_at_millis + 9;
    let (artifact_id, digest, receipt, source) = seed_candidate_artifact(
        &mut artifacts,
        &resolver,
        &repository_scope_key(scope),
        delivery,
        writer,
        repository,
        candidate,
        1_100,
        finished_at,
    );
    let authority = SeedTerminalAuthority {
        schema_version: 1,
        delivery_id: delivery.id(),
        work_run_id: &writer.work_run_id,
        job_id: &writer.execution_job_id,
        attempt: writer.attempt,
        lease_id: writer.lease_id.as_ref().expect("lease"),
        fencing_token: writer.fencing_token.as_ref().expect("fence"),
        worker_id: writer.worker_id.as_ref().expect("Worker"),
        worker_instance_id: writer.worker_instance_id.as_ref().expect("Worker instance"),
        worker_session_id: writer.worker_session_id.as_ref().expect("WorkerSession"),
        issued_at: Instant("2026-08-25T00:00:00.000Z".into()),
        expires_at: Instant("2026-08-25T01:00:00.000Z".into()),
        artifacts: vec![ArtifactReference {
            artifact_id,
            digest,
        }],
        codex_thread_id: &writer.codex_thread_id,
        finished_at_millis: finished_at,
        last_event_sequence: ExecutionAckSequence(4),
        status: ExecutionOutcomeStatus::Succeeded,
        disposition: SeedTerminalDisposition::Settled {
            delivery_revision: delivery.revision(),
        },
    };
    let mut storage = SqliteStorage::open(data).expect("terminal storage");
    write_state_once(
        &mut storage,
        format!("delivery-terminal-authority:{}", writer.execution_job_id.0),
        serde_json::to_vec(&authority).expect("terminal authority JSON"),
        21_100,
    );
    let pin = storage
        .git_candidate_retention(repository.parent().expect("controlled root"))
        .expect("fixture retention owner")
        .pin_after_final_artifact_ack(
            &receipt,
            &source,
            &Sha256Digest(format!("sha256:{}", "f".repeat(64))),
        )
        .expect("fixture Candidate retained");
    let reference = pin.reference_name().to_owned();
    Box::new(storage).close().expect("terminal storage close");
    artifacts.close().expect("fixture Artifact close");
    reference
}

fn fixture_delivery(repository: &Path, base_commit: String) -> Delivery {
    let fixture = verdict_fixture(
        &DeliveryId(canonical_id("dlv", 77)),
        VerdictFixtureOutcome::Pass,
    );
    let mut snapshot = fixture.delivery.into_snapshot();
    // The candidate is ready for review. No reviewer/verifier has run yet.
    // The verdict fixture's completed verification roles require runtime
    // ledgers and are outside this query/ingress regression.
    snapshot
        .session_bindings
        .retain(|binding| binding.execution_profile.as_deref() == Some("executor"));
    let writer_run_id = snapshot.session_bindings[0].work_run_id.clone();
    snapshot
        .work_run_aggregate
        .runs
        .retain(|run| run.id == writer_run_id);
    snapshot.spec.repository = RepositoryRef {
        schema_version: DELIVERY_SCHEMA_VERSION,
        kind: RepositoryKind::LocalGit,
        locator: repository
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .expect("portable repository locator")
            .to_owned(),
    };
    snapshot.spec.base_revision = base_commit;
    for (index, binding) in snapshot.session_bindings.iter_mut().enumerate() {
        let seed = format!("production-{index}");
        let previous_work_run_id = binding.work_run_id.clone();
        binding.execution_job_id =
            winwincode_domain::ExecutionJobId(canonical_id("job", 1_000 + index as u64));
        binding.worker_session_id = Some(winwincode_domain::WorkerSessionId(canonical_id(
            "wsn",
            1_000 + index as u64,
        )));
        binding.codex_thread_id = Some(winwincode_domain::CodexThreadId(canonical_id(
            "cdx",
            1_000 + index as u64,
        )));
        *binding = binding.clone().with_test_authority(&seed, binding.attempt);
        binding.worker_id = Some(WorkerId(canonical_id("wrk", 1_000 + index as u64)));
        binding.worker_instance_id =
            Some(WorkerInstanceId(canonical_id("wki", 1_000 + index as u64)));
        binding.lease_id = Some(winwincode_domain::LeaseId(canonical_id(
            "lse",
            1_000 + index as u64,
        )));
        binding.fencing_token = Some(winwincode_domain::FencingToken(
            (1_000 + index as u64).to_string(),
        ));
        binding
            .runtime_context
            .as_mut()
            .expect("runtime context")
            .agent_identity
            .worker_id = binding.worker_id.clone().expect("Worker");
        let run = snapshot
            .work_run_aggregate
            .runs
            .iter_mut()
            .find(|run| run.id == previous_work_run_id)
            .expect("canonical fixture WorkRun");
        run.id = binding.work_run_id.clone();
        run.execution_job_id = binding.execution_job_id.clone();
        run.product_session_id = Some(binding.product_session_id.clone());
        run.worker_session_id = binding.worker_session_id.clone().expect("WorkerSession");
        run.codex_thread_id.clone_from(&binding.codex_thread_id);
        run.worker_id = binding.worker_id.clone().expect("Worker");
        run.worker_instance_id = binding.worker_instance_id.clone().expect("Worker instance");
        run.lease_id = binding.lease_id.clone().expect("lease");
        run.fencing_token
            .clone_from(&binding.fencing_token.as_ref().expect("fence").0);
    }
    Delivery::try_from_snapshot(snapshot).expect("production verdict Delivery")
}

fn preflight_query(
    data: &Path,
    repository_root: &Path,
    scope: &RepositoryScope,
    delivery: &Delivery,
) {
    let mut control_plane =
        ControlPlane::start_local(ControlPlaneConfig::local(data), Box::new(NoopPublisher))
            .expect("preflight Control Plane");
    control_plane
        .install_strongflow_projection_sources(StrongFlowProjectionSources::new(
            Box::new(SqliteTrustedRuntimeProjectionAdapter::from_sqlite_storage()),
            Box::new(SqliteTrustedPublicationProjectionAdapter),
        ))
        .expect("preflight production sources");
    control_plane
        .install_git_source_resolver(Box::new(
            LocalGitSourceResolver::open(repository_root).expect("preflight real resolver"),
        ))
        .expect("preflight trusted resolver");
    let query = serde_json::from_value(json!({
        "schemaVersion": "winwincode/v1", "requestId": canonical_id("req", 90_099),
        "actor": {"kind": "user", "id": canonical_id("usr", 77)},
        "scope": scope, "query": "delivery.get",
        "parameters": {"deliveryId": delivery.id(), "atCursor": null},
        "page": {"cursor": null, "limit": 20}
    }))
    .expect("preflight generated delivery.get");
    let result = control_plane.delivery_get(&query);
    let diagnostic = result.as_ref().err().map(|error| format!("{error:?}"));
    control_plane.shutdown().expect("preflight shutdown");
    assert!(
        result.is_ok(),
        "production fixture preflight failed: {diagnostic:?}"
    );
    let response =
        serde_json::to_value(result.expect("checked preflight result")).expect("preflight JSON");
    assert!(
        response["result"]["currentCandidate"].is_object(),
        "{response}"
    );
    assert!(response["result"]["readCursor"].is_object(), "{response}");
}

#[allow(clippy::too_many_arguments)]
fn seed_candidate_artifact(
    artifacts: &mut ArtifactStore,
    resolver: &LocalGitSourceResolver,
    scope: &ReceiptScopeKey,
    delivery: &Delivery,
    binding: &SessionBinding,
    repository: &Path,
    candidate_commit: &str,
    seed: u64,
    finished_at: u64,
) -> (
    ArtifactId,
    Sha256Digest,
    winwincode_storage::ArtifactWriteReceipt,
    ValidatedGitSourceArtifact,
) {
    let provenance = ArtifactProvenance::execution_job(
        binding.execution_job_id.clone(),
        binding.attempt,
        binding.lease_id.clone().expect("lease"),
        binding.fencing_token.clone().expect("fence"),
        binding.worker_id.clone().expect("Worker"),
        binding.worker_instance_id.clone().expect("WorkerInstance"),
        binding.worker_session_id.clone().expect("WorkerSession"),
    )
    .expect("Artifact provenance");
    let artifact_id = ArtifactId(canonical_id("art", seed));
    let bytes = GitCandidateArtifactManifest::new(
        candidate_commit.to_owned(),
        candidate_bundle(
            repository,
            &delivery.snapshot().spec.base_revision,
            candidate_commit,
        ),
    )
    .expect("candidate manifest")
    .encode()
    .expect("manifest encode");
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes)));
    artifacts
        .open_artifact(ArtifactOpen::new(
            scope.clone(),
            ExecutionMessageId(canonical_id("xmsg", seed * 2)),
            RequestId(canonical_id("req", seed * 2)),
            artifact_id.clone(),
            "candidate",
            CANDIDATE_MEDIA_TYPE,
            digest.clone(),
            bytes.len() as u64,
            Some("candidate.json".into()),
            provenance.clone(),
            ArtifactMeteringAttribution {
                organization_id: OrganizationId(canonical_id("org", 77)),
                workspace_id: WorkspaceId(canonical_id("wsp", 77)),
                project_id: ProjectId(canonical_id("prj", 77)),
                repository_id: RepositoryId(canonical_id("rep", 77)),
                delivery_id: Some(delivery.id().clone()),
                product_session_id: Some(binding.product_session_id.clone()),
                user_id: UserId(canonical_id("usr", 77)),
            },
            ArtifactRetention::Indefinite,
            finished_at.saturating_sub(1),
        ))
        .expect("Artifact open");
    let receipt = artifacts
        .append_chunk(&ArtifactChunk::new(
            scope.clone(),
            ExecutionMessageId(canonical_id("xmsg", seed * 2 + 1)),
            artifact_id.clone(),
            provenance.clone(),
            finished_at,
            1,
            "application/octet-stream",
            digest.clone(),
            bytes,
            true,
        ))
        .expect("Artifact complete");
    let object = artifacts
        .read_exact(&ArtifactAccess::new(
            scope.clone(),
            artifact_id.clone(),
            digest.clone(),
            provenance,
        ))
        .expect("Artifact read");
    let source = resolver
        .resolve_candidate(
            &object,
            &delivery.snapshot().spec.repository.locator,
            &delivery.snapshot().spec.base_revision,
        )
        .expect("source resolution");
    (artifact_id, digest, receipt, source)
}

fn seed_delivery(root: &Path, scope: &RepositoryScope, delivery: &Delivery) {
    let capture = CapturingJournal::default();
    DeliveryStore::borrowed(&capture)
        .execute(DeliveryCommand::SeedForTest(CreateDelivery {
            request_id: RequestId(canonical_id("req", 50_000)),
            request_digest: "a".repeat(64),
            snapshot: delivery.clone(),
        }))
        .expect("seed Delivery publication");
    let AtomicPublication::Create {
        delivery_id,
        manifest,
        first_record,
    } = capture
        .publication
        .into_inner()
        .expect("publication lock")
        .expect("Delivery publication")
    else {
        panic!("Delivery seed must create a journal");
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
    let catalog_stream = delivery_catalog_stream(scope, delivery.id());
    let catalog = serde_json::to_vec(&SeedCatalogEntry {
        schema_version: 1,
        repository_scope: scope,
        delivery_id: delivery.id(),
    })
    .expect("catalog JSON");
    let mut storage = SqliteStorage::open(root).expect("seed storage");
    storage
        .commit(
            &StateCommit::new(
                receipt_identity(repository_scope_key(scope), 50_001),
                digest(50_001),
                format!("delivery:{}", delivery.id().0),
                0,
                delivery.encode_json().expect("Delivery JSON"),
                vec![NewOutboxEvent::internal(
                    "fixture-delivery-seed",
                    "fixture.seed.internal",
                    b"{}".to_vec(),
                )],
            )
            .with_journal_publication(publication)
            .with_state_mutation(
                StateMutation::new(catalog_stream, 0, catalog).expect("catalog mutation"),
            ),
        )
        .expect("seed Delivery");
    Box::new(storage).close().expect("seed storage close");
}

fn write_state_once(storage: &mut SqliteStorage, stream: String, payload: Vec<u8>, seed: u64) {
    write_state_revision(storage, stream, 0, payload, seed);
}

fn write_state_revision(
    storage: &mut SqliteStorage,
    stream: String,
    expected_revision: u64,
    payload: Vec<u8>,
    seed: u64,
) {
    storage
        .commit(&StateCommit::new(
            receipt_identity(
                ReceiptScopeKey::from_encoded(b"production-verdict-fixture".to_vec())
                    .expect("fixture scope"),
                seed,
            ),
            digest(seed),
            stream,
            expected_revision,
            payload,
            vec![NewOutboxEvent::internal(
                format!("fixture-state-{seed}"),
                "fixture.seed.internal",
                b"{}".to_vec(),
            )],
        ))
        .expect("seed product state");
}

fn receipt_identity(scope: ReceiptScopeKey, seed: u64) -> ReceiptIdentity {
    ReceiptIdentity::new(
        ReceiptActorKey::from_encoded(format!("fixture-actor-{seed}").into_bytes())
            .expect("actor key"),
        scope,
        RequestId(canonical_id("req", seed)),
    )
    .expect("receipt identity")
}

fn digest(seed: u64) -> Sha256Digest {
    Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(format!("fixture-{seed}"))
    ))
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

fn repository_scope_key(scope: &RepositoryScope) -> ReceiptScopeKey {
    receipt_scope_key(&PublicEventScope::Repository {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
    })
    .expect("repository scope key")
}

fn delivery_catalog_stream(scope: &RepositoryScope, delivery_id: &DeliveryId) -> String {
    format!(
        "delivery-catalog:{:x}:{}",
        Sha256::digest(serde_json::to_vec(scope).expect("scope JSON")),
        delivery_id.0
    )
}

fn initialize_repository(repository: &Path) -> (String, String) {
    fs::create_dir_all(repository.join("src")).expect("repository directory");
    git(repository, &["init", "-q", "-b", "main"]);
    git(
        repository,
        &["config", "user.email", "fixture@example.invalid"],
    );
    git(repository, &["config", "user.name", "Fixture"]);
    fs::write(repository.join("src/lib.rs"), "pub fn base() {}\n").expect("base source");
    git(repository, &["add", "."]);
    git(repository, &["commit", "-q", "-m", "base"]);
    let base = git_text(repository, &["rev-parse", "HEAD"]);
    fs::write(
        repository.join("src/lib.rs"),
        "pub fn base() {}\npub fn candidate() {}\n",
    )
    .expect("candidate source");
    git(repository, &["add", "."]);
    git(repository, &["commit", "-q", "-m", "candidate"]);
    let candidate = git_text(repository, &["rev-parse", "HEAD"]);
    (base, candidate)
}

fn git(repository: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .args(arguments)
        .current_dir(repository)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .expect("run Git");
    assert!(status.success(), "Git command failed: {arguments:?}");
}

fn git_text(repository: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(repository)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("run Git query");
    assert!(output.status.success(), "Git query failed: {arguments:?}");
    String::from_utf8(output.stdout)
        .expect("Git output")
        .trim()
        .to_owned()
}
