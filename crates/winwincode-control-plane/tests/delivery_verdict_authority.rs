// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    Actor, DeliverySubmitVerdictCommand, DeliverySubmitVerdictCommandCommand,
    DeliverySubmitVerdictPayload,
};
use winwincode_control_plane::{
    ControlPlane, ControlPlaneConfig, EventPublishError, EventPublisher,
    LocalDeliveryAdapterConfig, OutboxEvent,
};
use winwincode_delivery::{
    application::{
        verdict::test_support::{VerdictFixtureOutcome, verdict_fixture},
        workrun_execution::{
            DeliveryTerminalOutcomeFacts, TerminalArtifactReference, TerminalOutcomeStatus,
            test_support::{
                active_lease_identity, delivery_terminal_outcome_facts, session_binding_authority,
                terminal_outcome_metadata, terminal_worker_outcome,
            },
        },
    },
    domain::{
        DELIVERY_SCHEMA_VERSION, Delivery, RepositoryKind, RepositoryRef, SessionBinding,
        candidate::freeze_delivery_candidate_from_source,
    },
    store::{
        AtomicPublication, CreateDelivery, DeliveryCommand, DeliveryCommandPort,
        DeliveryJournalPort, DeliveryStore, JournalBackendError, LoadedDeliveryJournal,
    },
};
use winwincode_domain::{
    ArtifactId, DeliveryId, ExecutionAckSequence, ExecutionEventId, ExecutionMessageId,
    ExecutionSequence, Instant, OrganizationId, ProductSessionId, ProjectId, RepositoryId,
    RequestId, Revision, SchemaVersion, Sha256Digest, UserId, WorkspaceId,
};
use winwincode_domain::{RepositoryScope, RepositoryScopeKind, UserActor, UserActorKind};
use winwincode_execution_port::generated::{
    ArtifactReference, EncodedPayload, ExecutionEventCategory, ExecutionEventRecord,
    ExecutionOutcomeStatus,
};
use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, ArtifactAccess,
    ArtifactChunk, ArtifactMeteringAttribution, ArtifactOpen, ArtifactProvenance,
    ArtifactRetention, ArtifactStore, CandidateSourceManifest, LocalArtifactObjectStore,
    LocalGitSourceResolver, NewOutboxEvent, ProductStateStorage, PublicEventScope, ReceiptActorKey,
    ReceiptIdentity, ReceiptScopeKey, SqliteStorage, StateCommit, StateMutation, receipt_scope_key,
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);
const CANDIDATE_MEDIA_TYPE: &str = "application/vnd.winwincode.git-candidate+json";

#[derive(Clone, Copy)]
enum RuntimeFixture {
    Valid,
    ProductFailure,
    StaleCandidate,
    NonJsonEvidence,
    LaterWriterFailed,
    AmbiguousWriter,
    AmbiguousVerification,
}

struct NoopPublisher;

impl EventPublisher for NoopPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Ok(())
    }
}

#[derive(Default)]
struct CapturingJournal {
    publication: std::sync::Mutex<Option<AtomicPublication>>,
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
    worker_id: &'authority winwincode_domain::WorkerId,
    worker_instance_id: &'authority winwincode_domain::WorkerInstanceId,
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

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SeedRuntimeLedger<'ledger> {
    schema_version: u8,
    delivery_id: Option<&'ledger DeliveryId>,
    work_item_id: Option<&'ledger winwincode_domain::WorkItemId>,
    work_run_id: Option<&'ledger winwincode_domain::WorkRunId>,
    product_session_id: &'ledger ProductSessionId,
    execution_job_id: &'ledger winwincode_domain::ExecutionJobId,
    worker_session_id: &'ledger winwincode_domain::WorkerSessionId,
    codex_thread_id: &'ledger winwincode_domain::CodexThreadId,
    lease_id: &'ledger winwincode_domain::LeaseId,
    attempt: u64,
    fencing_token: &'ledger winwincode_domain::FencingToken,
    worker_id: &'ledger winwincode_domain::WorkerId,
    worker_instance_id: &'ledger winwincode_domain::WorkerInstanceId,
    highest_sequence: u64,
    events: Vec<SeedRuntimeLedgerEvent>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SeedRuntimeLedgerEvent {
    event: ExecutionEventRecord,
    event_digest: Sha256Digest,
}

struct SeededVerdict {
    root: PathBuf,
    data: PathBuf,
    repository: PathBuf,
    scope: RepositoryScope,
    delivery: Delivery,
    candidate_digest: Sha256Digest,
    candidate: serde_json::Value,
}

#[test]
#[allow(clippy::too_many_lines)]
fn production_adapter_joins_durable_facts_replays_and_rejects_stale_sources() {
    let valid = seed_verdict("valid", RuntimeFixture::Valid);
    let command = verdict_command(&valid, 900);
    let mut first = start(&valid);
    let completed = first
        .delivery_submit_verdict(&command)
        .expect("production verdict");
    assert_eq!(completed.previous_revision, command.expected_revision);
    assert_eq!(
        completed.current_revision.0,
        command.expected_revision.0 + 1
    );
    first.shutdown().expect("first shutdown");

    let mut restarted = start(&valid);
    let replay = restarted
        .delivery_submit_verdict(&command)
        .expect("restart replay");
    assert_eq!(replay, completed);
    restarted.shutdown().expect("replay shutdown");

    let mut stale_command = verdict_command(&valid, 901);
    stale_command.expected_revision = completed.current_revision;
    stale_command.payload.candidate_digest = Sha256Digest(format!("sha256:{}", "0".repeat(64)));
    let mut stale_host = start(&valid);
    assert_eq!(
        stale_host
            .delivery_submit_verdict(&stale_command)
            .expect_err("caller stale-check digest must not replace durable candidate")
            .code(),
        winwincode_api::generated::ErrorCode::TrustedFactsUnavailable
    );
    stale_host.shutdown().expect("stale shutdown");
    cleanup(valid);

    for (label, fixture) in [
        ("stale-runtime", RuntimeFixture::StaleCandidate),
        ("non-json-evidence", RuntimeFixture::NonJsonEvidence),
        ("later-writer-failed", RuntimeFixture::LaterWriterFailed),
        ("ambiguous-writer", RuntimeFixture::AmbiguousWriter),
        (
            "ambiguous-verification",
            RuntimeFixture::AmbiguousVerification,
        ),
    ] {
        let seeded = seed_verdict(label, fixture);
        let command = verdict_command(&seeded, 910);
        let before_revision = delivery_state_revision(&seeded.data);
        let mut host = start(&seeded);
        assert_eq!(
            host.delivery_submit_verdict(&command)
                .expect_err("stale runtime or Evidence must fail closed")
                .code(),
            winwincode_api::generated::ErrorCode::TrustedFactsUnavailable
        );
        host.shutdown().expect("negative shutdown");
        assert_eq!(delivery_state_revision(&seeded.data), before_revision);
        cleanup(seeded);
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the production rework test keeps rejection, commit, and restart replay together"
)]
fn production_rework_dispatch_uses_failed_candidate_and_replays_after_restart() {
    let seeded = seed_verdict("precise-rework", RuntimeFixture::ProductFailure);
    let mut host = start(&seeded);
    let verdict = verdict_command(&seeded, 930);
    host.delivery_submit_verdict(&verdict)
        .expect("real failing verifier results");
    let read = |host: &ControlPlane| {
        Delivery::decode_json(
            &host
                .load_state(&format!("delivery:{}", seeded.delivery.id().0))
                .unwrap()
                .unwrap()
                .payload,
        )
        .unwrap()
    };
    let failed = read(&host);
    assert_eq!(
        failed.snapshot().verdict.as_ref().unwrap().status,
        winwincode_delivery::domain::DeliveryVerdictStatus::Fail
    );
    let mut evidence = failed
        .snapshot()
        .verdict
        .as_ref()
        .unwrap()
        .criteria
        .iter()
        .filter(|result| result.verdict == winwincode_delivery::domain::CriterionVerdict::Fail)
        .flat_map(|result| result.evidence_refs.clone())
        .collect::<Vec<_>>();
    evidence.sort_by(|a, b| a.0.cmp(&b.0));
    evidence.dedup();
    for (offset, attention) in failed
        .snapshot()
        .attention_items
        .iter()
        .filter(|item| item.status == winwincode_delivery::domain::AttentionItemStatus::Open)
        .enumerate()
    {
        let current = read(&host);
        let command = serde_json::from_value(json!({
            "schemaVersion":SchemaVersion::WinwincodeV1, "requestId":canonical_id("req", 940+offset as u64),
            "command":"delivery.resolve_attention", "scope":seeded.scope, "actor":verdict.actor,
            "expectedRevision":current.revision(), "payload":{
                "deliveryId":current.id(), "attentionItemId":attention.id,
                "decision":"resolve", "resolution":"Remediate the exact failed candidate", "remediation":null
            }
        })).unwrap();
        host.delivery_resolve_attention(&command)
            .expect("resolve actual failure Attention");
    }
    let queued = || {
        rusqlite::Connection::open(seeded.data.join("control-plane.sqlite3"))
            .unwrap()
            .query_row("SELECT COUNT(*) FROM scheduler_execution_jobs", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap()
    };
    assert_eq!(queued(), 0);
    let source = read(&host);
    let command: winwincode_api::generated::WorkRunStartCommand = serde_json::from_value(json!({
        "schemaVersion":SchemaVersion::WinwincodeV1, "requestId":canonical_id("req", 960),
        "command":"workrun.start", "scope":seeded.scope, "actor":verdict.actor,
        "expectedRevision":source.revision(), "payload":{
            "deliveryId":source.id(), "dispatchProfile":"remediator", "rework":{
                "candidateRef":seeded.candidate["candidateRef"], "diffSha256":seeded.candidate["diffSha256"],
                "targets":[{"workItemId":source.snapshot().session_bindings[0].work_item_id,
                    "filePath":seeded.candidate["changedHunks"][0]["filePath"],
                    "sourceHunkSha256":seeded.candidate["changedHunks"][0]["hunkSha256"], "evidenceRefIds":evidence}]
            }
        }
    })).unwrap();
    for field in [
        "filePath",
        "sourceHunkSha256",
        "workItemId",
        "evidenceRefIds",
    ] {
        let mut bad = command.clone();
        let target = &mut bad.payload.rework.as_mut().unwrap().targets[0];
        match field {
            "filePath" => target.file_path = "src/foreign.rs".into(),
            "sourceHunkSha256" => target.source_hunk_sha256 = "f".repeat(64),
            "evidenceRefIds" => {
                target.evidence_ref_ids =
                    vec![winwincode_domain::EvidenceId(canonical_id("evd", 999))];
            }
            "workItemId" => {
                target.work_item_id = winwincode_domain::WorkItemId(canonical_id("wit", 999));
            }
            _ => unreachable!(),
        }
        host.workrun_start(&bad)
            .expect_err("foreign rework scope must be rejected");
        assert_eq!(read(&host), source);
        assert_eq!(queued(), 0, "rejected scope must not enqueue work");
    }
    let accepted = host
        .workrun_start(&command)
        .expect("actual production rework dispatch");
    assert_eq!(queued(), 1);
    let job_bytes = rusqlite::Connection::open(seeded.data.join("control-plane.sqlite3"))
        .unwrap()
        .query_row(
            "SELECT dispatch_payload FROM scheduler_execution_jobs",
            [],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .unwrap();
    let job: winwincode_execution_port::generated::ExecutionJob =
        serde_json::from_slice(&job_bytes).unwrap();
    assert_eq!(job.execution_profile, "remediator");
    assert_eq!(
        job.workspace.checkout_revision,
        seeded.candidate["candidateCommitId"].as_str().unwrap()
    );
    let winwincode_execution_port::generated::ExecutionScope::WorkRunExecutionScope(scope) =
        job.scope
    else {
        unreachable!()
    };
    let authorization = scope.rework_authorization.unwrap();
    assert_eq!(
        authorization.candidate_ref,
        command.payload.rework.as_ref().unwrap().candidate_ref
    );
    assert_eq!(authorization.targets[0].work_item_id, scope.work_item_id);
    let committed = read(&host);
    assert!(committed.snapshot().evidence.is_empty());
    assert!(committed.snapshot().verdict.is_none());
    host.shutdown().unwrap();
    let mut restarted = start(&seeded);
    assert_eq!(
        restarted.workrun_start(&command).expect("durable replay"),
        accepted
    );
    assert_eq!(queued(), 1, "replay must not enqueue a second job");
    restarted.shutdown().unwrap();
    cleanup(seeded);
}

fn seed_verdict(label: &str, runtime_fixture: RuntimeFixture) -> SeededVerdict {
    let root = unique_root(label);
    let data = root.join("data");
    let repository = root.join("repository");
    let (base_commit, candidate_commit) = initialize_repository(&repository);
    let repository = fs::canonicalize(repository).expect("canonical repository");
    let scope = repository_scope(77);
    let delivery = fixture_delivery(&repository, base_commit, runtime_fixture);
    fs::create_dir_all(&data).expect("data directory");
    seed_delivery(&data, &scope, &delivery);
    let (candidate_digest, candidate) = seed_verdict_sources(
        &data,
        &repository,
        &scope,
        &delivery,
        &candidate_commit,
        runtime_fixture,
    );
    SeededVerdict {
        root,
        data,
        repository,
        scope,
        delivery,
        candidate_digest,
        candidate,
    }
}

fn fixture_delivery(
    repository: &Path,
    base_commit: String,
    runtime_fixture: RuntimeFixture,
) -> Delivery {
    let fixture = verdict_fixture(
        &DeliveryId(canonical_id("dlv", 77)),
        VerdictFixtureOutcome::Pass,
    );
    let mut snapshot = fixture.delivery.into_snapshot();
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
    match runtime_fixture {
        RuntimeFixture::LaterWriterFailed => append_failed_writer(&mut snapshot),
        RuntimeFixture::AmbiguousWriter => append_ambiguous_writer(&mut snapshot),
        RuntimeFixture::AmbiguousVerification => append_ambiguous_verifier(&mut snapshot),
        RuntimeFixture::Valid
        | RuntimeFixture::ProductFailure
        | RuntimeFixture::StaleCandidate
        | RuntimeFixture::NonJsonEvidence => {}
    }
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
        binding.worker_id = Some(winwincode_domain::WorkerId(canonical_id(
            "wrk",
            1_000 + index as u64,
        )));
        binding.worker_instance_id = Some(winwincode_domain::WorkerInstanceId(canonical_id(
            "wki",
            1_000 + index as u64,
        )));
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

fn seed_verdict_sources(
    data: &Path,
    repository: &Path,
    scope: &RepositoryScope,
    delivery: &Delivery,
    candidate_commit: &str,
    runtime_fixture: RuntimeFixture,
) -> (Sha256Digest, serde_json::Value) {
    let object_store =
        LocalArtifactObjectStore::open(data.join("artifacts")).expect("local Artifact objects");
    let mut artifacts = ArtifactStore::open(data.join("artifact-catalog"), Box::new(object_store))
        .expect("Artifact catalog");
    let source_resolver =
        LocalGitSourceResolver::open(repository.parent().expect("repository parent"))
            .expect("Git resolver");
    let scope_key = repository_scope_key(scope);
    let mut storage = SqliteStorage::open(data).expect("durable storage");
    let mut writer_candidate = None;
    let current_candidate_ref;

    let writer = delivery
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| binding.execution_profile.as_deref() == Some("executor"))
        .expect("writer binding");
    let (writer_terminal, writer_source) = seed_terminal_and_artifact(
        &mut storage,
        &mut artifacts,
        &source_resolver,
        &scope_key,
        delivery,
        writer,
        candidate_commit,
        1_100,
    );
    if matches!(
        runtime_fixture,
        RuntimeFixture::LaterWriterFailed | RuntimeFixture::AmbiguousWriter
    ) {
        current_candidate_ref = "git-candidate:latest-writer-failed".to_owned();
    } else {
        let frozen =
            freeze_delivery_candidate_from_source(delivery, &writer_source, &writer_terminal)
                .expect("frozen production candidate");
        current_candidate_ref = frozen.candidate_ref().to_owned();
        writer_candidate.replace(frozen);
    }

    for (index, role) in ["reviewer", "verifier"].into_iter().enumerate() {
        let binding = delivery
            .snapshot()
            .session_bindings
            .iter()
            .find(|binding| binding.execution_profile.as_deref() == Some(role))
            .expect("verification binding");
        seed_terminal_and_artifact(
            &mut storage,
            &mut artifacts,
            &source_resolver,
            &scope_key,
            delivery,
            binding,
            candidate_commit,
            1_200 + index as u64,
        );
        seed_runtime(
            &mut storage,
            &scope_key,
            delivery,
            binding,
            &current_candidate_ref,
            role,
            runtime_fixture,
            1_300 + index as u64 * 10,
        );
    }
    Box::new(storage).close().expect("storage close");
    artifacts.close().expect("Artifact close");
    let mut candidate = writer_candidate
        .as_ref()
        .map_or(serde_json::Value::Null, |value| {
            serde_json::to_value(value).unwrap()
        });
    if !candidate.is_null() {
        candidate["changedHunks"] = json!(
            writer_source
                .changed_hunks()
                .iter()
                .map(|hunk| json!({"filePath":hunk.file_path(), "hunkSha256":hunk.hunk_sha256()}))
                .collect::<Vec<_>>()
        );
    }
    let digest = writer_candidate.map_or_else(
        || Sha256Digest(format!("sha256:{}", "f".repeat(64))),
        |candidate| {
            Sha256Digest(
                candidate
                    .candidate_ref()
                    .strip_prefix("git-candidate:")
                    .expect("candidate prefix")
                    .to_owned(),
            )
        },
    );
    (digest, candidate)
}

fn append_failed_writer(snapshot: &mut winwincode_delivery::domain::DeliverySnapshot) {
    let source_index = snapshot
        .session_bindings
        .iter()
        .position(|binding| binding.execution_profile.as_deref() == Some("executor"))
        .expect("executor binding");
    let mut binding = snapshot.session_bindings[source_index].clone();
    binding.id = winwincode_delivery::domain::SessionBindingId(canonical_id("sbn", 8_001));
    binding.work_run_id = winwincode_domain::WorkRunId(canonical_id("wrn", 8_001));
    binding.product_session_id = ProductSessionId(canonical_id("psn", 8_001));
    binding.execution_job_id = winwincode_domain::ExecutionJobId(canonical_id("job", 8_001));
    binding.worker_session_id = Some(winwincode_domain::WorkerSessionId(canonical_id(
        "wsn", 8_001,
    )));
    binding.codex_thread_id = Some(winwincode_domain::CodexThreadId(canonical_id("cdx", 8_001)));
    binding.attempt = 1;
    binding.bound_at_millis = snapshot
        .session_bindings
        .iter()
        .map(|current| current.bound_at_millis)
        .max()
        .expect("latest binding")
        .saturating_add(10);
    snapshot.updated_at_millis = binding.bound_at_millis;
    let source_binding = &snapshot.session_bindings[source_index];
    let mut accepted = snapshot
        .work_run_aggregate
        .runs
        .iter()
        .find(|accepted| accepted.id == source_binding.work_run_id)
        .expect("source canonical WorkRun")
        .clone();
    accepted.id = binding.work_run_id.clone();
    accepted.product_session_id = Some(binding.product_session_id.clone());
    accepted.attempt = i64::try_from(binding.attempt).expect("bounded attempt");
    accepted.state = winwincode_domain::WorkRunState::Failed;
    binding.execution_profile = Some("remediator".into());
    snapshot.work_run_aggregate.runs.push(accepted);
    snapshot.session_bindings.push(binding);
}

fn append_ambiguous_verifier(snapshot: &mut winwincode_delivery::domain::DeliverySnapshot) {
    let source_index = snapshot
        .session_bindings
        .iter()
        .position(|binding| binding.execution_profile.as_deref() == Some("verifier"))
        .expect("verifier binding");
    let mut binding = snapshot.session_bindings[source_index].clone();
    binding.id = winwincode_delivery::domain::SessionBindingId(canonical_id("sbn", 8_002));
    binding.work_run_id = winwincode_domain::WorkRunId(canonical_id("wrn", 8_002));
    binding.product_session_id = ProductSessionId(canonical_id("psn", 8_002));
    binding.execution_job_id = winwincode_domain::ExecutionJobId(canonical_id("job", 8_002));
    binding.worker_session_id = Some(winwincode_domain::WorkerSessionId(canonical_id(
        "wsn", 8_002,
    )));
    binding.codex_thread_id = Some(winwincode_domain::CodexThreadId(canonical_id("cdx", 8_002)));
    let source_binding = &snapshot.session_bindings[source_index];
    let mut accepted = snapshot
        .work_run_aggregate
        .runs
        .iter()
        .find(|accepted| accepted.id == source_binding.work_run_id)
        .expect("source canonical WorkRun")
        .clone();
    accepted.id = binding.work_run_id.clone();
    accepted.product_session_id = Some(binding.product_session_id.clone());
    accepted.attempt = i64::try_from(binding.attempt).expect("bounded attempt");
    snapshot.work_run_aggregate.runs.push(accepted);
    snapshot.session_bindings.push(binding);
}

fn append_ambiguous_writer(snapshot: &mut winwincode_delivery::domain::DeliverySnapshot) {
    let source_index = snapshot
        .session_bindings
        .iter()
        .position(|binding| binding.execution_profile.as_deref() == Some("executor"))
        .expect("executor binding");
    let mut binding = snapshot.session_bindings[source_index].clone();
    binding.id = winwincode_delivery::domain::SessionBindingId(canonical_id("sbn", 8_003));
    binding.work_run_id = winwincode_domain::WorkRunId(canonical_id("wrn", 8_003));
    binding.product_session_id = ProductSessionId(canonical_id("psn", 8_003));
    binding.execution_job_id = winwincode_domain::ExecutionJobId(canonical_id("job", 8_003));
    binding.worker_session_id = Some(winwincode_domain::WorkerSessionId(canonical_id(
        "wsn", 8_003,
    )));
    binding.codex_thread_id = Some(winwincode_domain::CodexThreadId(canonical_id("cdx", 8_003)));
    let source_binding = &snapshot.session_bindings[source_index];
    let mut accepted = snapshot
        .work_run_aggregate
        .runs
        .iter()
        .find(|accepted| accepted.id == source_binding.work_run_id)
        .expect("source canonical WorkRun")
        .clone();
    // Preserve one CandidateReady producer. This fixture targets ambiguous
    // writer bindings, not an invalid aggregate with two current candidates.
    accepted.state = winwincode_domain::WorkRunState::Settled;
    accepted.id = binding.work_run_id.clone();
    accepted.product_session_id = Some(binding.product_session_id.clone());
    accepted.attempt = i64::try_from(binding.attempt).expect("bounded attempt");
    snapshot.work_run_aggregate.runs.push(accepted);
    snapshot.session_bindings.push(binding);
}

#[allow(clippy::too_many_arguments)]
fn seed_terminal_and_artifact(
    storage: &mut SqliteStorage,
    artifacts: &mut ArtifactStore,
    resolver: &LocalGitSourceResolver,
    scope: &ReceiptScopeKey,
    delivery: &Delivery,
    binding: &SessionBinding,
    candidate_commit: &str,
    seed: u64,
) -> (
    DeliveryTerminalOutcomeFacts,
    winwincode_storage::ValidatedGitSourceArtifact,
) {
    let worker_session = binding.worker_session_id.clone().expect("WorkerSession");
    let codex_thread = binding.codex_thread_id.clone().expect("CodexThread");
    let lease_id = binding.lease_id.clone().expect("lease");
    let fencing_token = binding.fencing_token.clone().expect("fence");
    let worker_id = binding.worker_id.clone().expect("Worker");
    let worker_instance = binding.worker_instance_id.clone().expect("WorkerInstance");
    let finished_at = binding.bound_at_millis.saturating_add(9);
    let SeededCandidateArtifact {
        artifact_id,
        digest,
        source,
    } = seed_candidate_artifact(
        artifacts,
        resolver,
        scope,
        delivery,
        binding,
        candidate_commit,
        seed,
        finished_at,
    );
    let terminal = delivery_terminal_outcome_facts(
        session_binding_authority(
            active_lease_identity(
                binding.execution_job_id.clone(),
                binding.attempt,
                lease_id.clone(),
                fencing_token.clone(),
                worker_id.clone(),
                worker_instance.clone(),
                worker_session.clone(),
            ),
            Instant("2026-08-25T00:00:00.000Z".into()),
            Instant("2026-08-25T01:00:00.000Z".into()),
        ),
        terminal_worker_outcome(
            binding.work_run_id.clone(),
            binding.execution_job_id.clone(),
            binding.attempt,
            lease_id,
            fencing_token,
            worker_id,
            worker_instance,
            worker_session,
            TerminalOutcomeStatus::Succeeded,
            terminal_outcome_metadata(
                Some(codex_thread),
                finished_at,
                ExecutionAckSequence(4),
                vec![TerminalArtifactReference {
                    artifact_id: artifact_id.clone(),
                    digest: digest.clone(),
                }],
            ),
        ),
    );
    let persisted = SeedTerminalAuthority {
        schema_version: 1,
        delivery_id: delivery.id(),
        work_run_id: &binding.work_run_id,
        job_id: &binding.execution_job_id,
        attempt: binding.attempt,
        lease_id: binding.lease_id.as_ref().expect("lease"),
        fencing_token: binding.fencing_token.as_ref().expect("fence"),
        worker_id: binding.worker_id.as_ref().expect("Worker"),
        worker_instance_id: binding.worker_instance_id.as_ref().expect("WorkerInstance"),
        worker_session_id: binding.worker_session_id.as_ref().expect("WorkerSession"),
        issued_at: Instant("2026-08-25T00:00:00.000Z".into()),
        expires_at: Instant("2026-08-25T01:00:00.000Z".into()),
        artifacts: vec![ArtifactReference {
            artifact_id,
            digest,
        }],
        codex_thread_id: &binding.codex_thread_id,
        finished_at_millis: finished_at,
        last_event_sequence: ExecutionAckSequence(4),
        status: ExecutionOutcomeStatus::Succeeded,
        disposition: SeedTerminalDisposition::Settled {
            delivery_revision: delivery.revision(),
        },
    };
    write_state_once(
        storage,
        format!("delivery-terminal-authority:{}", binding.execution_job_id.0),
        serde_json::to_vec(&persisted).expect("terminal JSON"),
        seed + 20_000,
    );
    (terminal, source)
}

struct SeededCandidateArtifact {
    artifact_id: ArtifactId,
    digest: Sha256Digest,
    source: winwincode_storage::ValidatedGitSourceArtifact,
}

#[allow(clippy::too_many_arguments)]
fn seed_candidate_artifact(
    artifacts: &mut ArtifactStore,
    resolver: &LocalGitSourceResolver,
    scope: &ReceiptScopeKey,
    delivery: &Delivery,
    binding: &SessionBinding,
    candidate_commit: &str,
    seed: u64,
    finished_at: u64,
) -> SeededCandidateArtifact {
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
    let bytes = CandidateSourceManifest::new(candidate_commit.to_owned())
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
                organization_id: OrganizationId("org_00000000000000000000000091".into()),
                workspace_id: WorkspaceId("wsp_00000000000000000000000091".into()),
                project_id: ProjectId("prj_00000000000000000000000091".into()),
                repository_id: RepositoryId("rep_00000000000000000000000091".into()),
                delivery_id: Some(delivery.id().clone()),
                product_session_id: Some(ProductSessionId(canonical_id("psn", seed))),
                user_id: UserId("usr_00000000000000000000000091".into()),
            },
            ArtifactRetention::Indefinite,
            finished_at.saturating_sub(1),
        ))
        .expect("Artifact open");
    artifacts
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
    SeededCandidateArtifact {
        artifact_id,
        digest,
        source,
    }
}

#[allow(clippy::too_many_arguments)]
fn seed_runtime(
    storage: &mut SqliteStorage,
    scope: &ReceiptScopeKey,
    delivery: &Delivery,
    binding: &SessionBinding,
    candidate_ref: &str,
    role: &str,
    fixture: RuntimeFixture,
    seed: u64,
) {
    let runtime_candidate = match fixture {
        RuntimeFixture::StaleCandidate => "git-candidate:stale-runtime",
        RuntimeFixture::Valid
        | RuntimeFixture::ProductFailure
        | RuntimeFixture::NonJsonEvidence
        | RuntimeFixture::LaterWriterFailed
        | RuntimeFixture::AmbiguousWriter
        | RuntimeFixture::AmbiguousVerification => candidate_ref,
    };
    let cited_event = match fixture {
        RuntimeFixture::NonJsonEvidence => format!("event-{role}-binary"),
        RuntimeFixture::Valid
        | RuntimeFixture::ProductFailure
        | RuntimeFixture::StaleCandidate
        | RuntimeFixture::LaterWriterFailed
        | RuntimeFixture::AmbiguousWriter
        | RuntimeFixture::AmbiguousVerification => format!("event-{role}-source"),
    };
    let events = runtime_events(
        binding.bound_at_millis.saturating_add(9),
        runtime_candidate,
        &delivery.snapshot().spec.id.0,
        delivery.snapshot().spec.revision,
        &delivery.snapshot().spec.acceptance_criteria[0].id.0,
        role,
        &cited_event,
        matches!(fixture, RuntimeFixture::ProductFailure),
    );
    let stream = runtime_stream_id(scope, &binding.execution_job_id);
    for highest in 1..=events.len() {
        let ledger = SeedRuntimeLedger {
            schema_version: 1,
            delivery_id: Some(delivery.id()),
            work_item_id: None,
            work_run_id: Some(&binding.work_run_id),
            product_session_id: &binding.product_session_id,
            execution_job_id: &binding.execution_job_id,
            worker_session_id: binding.worker_session_id.as_ref().expect("WorkerSession"),
            codex_thread_id: binding.codex_thread_id.as_ref().expect("CodexThread"),
            lease_id: binding.lease_id.as_ref().expect("lease"),
            attempt: binding.attempt,
            fencing_token: binding.fencing_token.as_ref().expect("fence"),
            worker_id: binding.worker_id.as_ref().expect("Worker"),
            worker_instance_id: binding.worker_instance_id.as_ref().expect("WorkerInstance"),
            highest_sequence: highest as u64,
            events: events[..highest].to_vec(),
        };
        write_state_revision(
            storage,
            stream.clone(),
            highest as u64 - 1,
            serde_json::to_vec(&ledger).expect("runtime JSON"),
            seed + highest as u64,
        );
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the runtime fixture inputs mirror one complete verification result"
)]
fn runtime_events(
    finished_at_millis: u64,
    candidate_ref: &str,
    delivery_spec_id: &str,
    delivery_spec_revision: u64,
    criterion_id: &str,
    role: &str,
    cited_event: &str,
    failed: bool,
) -> Vec<SeedRuntimeLedgerEvent> {
    let source_id = ExecutionEventId(format!("event-{role}-source"));
    let binary = b"\0verification-binary\xff";
    let payloads = [
        (
            ExecutionEventCategory::Lifecycle,
            ExecutionEventId(format!("event-{role}-policy")),
            encoded_json(&json!({
                "protocol": "winwincode.verification-session-policy.v1",
                "workspace_mode": "candidate-read-only",
                "permission_profile": "candidate-read-only-restricted",
                "candidate_ref": candidate_ref,
            })),
        ),
        (
            ExecutionEventCategory::Activity,
            ExecutionEventId(format!("event-{role}-binary")),
            EncodedPayload {
                content_type: "application/octet-stream".into(),
                data_base64: STANDARD.encode(binary),
                payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(binary))),
            },
        ),
        (
            if role == "reviewer" {
                ExecutionEventCategory::Command
            } else {
                ExecutionEventCategory::Test
            },
            source_id,
            encoded_json(&json!({"status": "completed", "exit_code": i32::from(failed)})),
        ),
        (
            ExecutionEventCategory::Activity,
            ExecutionEventId(format!("event-{role}-result")),
            encoded_json(&json!({
                "protocol": "winwincode.independent-verification-result.v1",
                "delivery_spec_id": delivery_spec_id,
                "delivery_spec_revision": delivery_spec_revision,
                "candidate_ref": candidate_ref,
                "findings": [{
                    "finding_id": format!("finding-{role}"),
                    "criterion_id": criterion_id,
                    "verdict": if failed { "fail" } else { "pass" },
                    "explanation": format!("{role} accepted the current candidate"),
                    "evidence_sources": [{
                        "type": if role == "reviewer" { "command" } else { "test" },
                        "event_id": cited_event,
                    }],
                }],
            })),
        ),
    ];
    payloads
        .into_iter()
        .enumerate()
        .map(|(index, (category, event_id, payload))| {
            let sequence = index + 1;
            let occurred_at_millis = finished_at_millis.saturating_sub(4 - sequence as u64);
            let event = ExecutionEventRecord {
                category,
                event_id,
                occurred_at: fixture_instant(occurred_at_millis),
                payload: Some(payload),
                sequence: ExecutionSequence(i64::try_from(sequence).expect("bounded sequence")),
                summary: format!("{role} verification fact"),
            };
            let digest = Sha256Digest(format!(
                "sha256:{:x}",
                Sha256::digest(serde_json::to_vec(&event).expect("event JSON"))
            ));
            SeedRuntimeLedgerEvent {
                event,
                event_digest: digest,
            }
        })
        .collect()
}

fn fixture_instant(millis: u64) -> Instant {
    const BASE: u64 = 1_800_000_000_000;
    let offset = millis.checked_sub(BASE).expect("fixture time after base");
    assert!(offset < 1_000, "fixture time stays inside one second");
    Instant(format!("2027-01-15T08:00:00.{offset:03}Z"))
}

fn encoded_json(value: &serde_json::Value) -> EncodedPayload {
    let bytes = serde_json::to_vec(value).expect("JSON");
    EncodedPayload {
        content_type: "application/json".into(),
        data_base64: STANDARD.encode(&bytes),
        payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))),
    }
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

fn verdict_command(seeded: &SeededVerdict, seed: u64) -> DeliverySubmitVerdictCommand {
    DeliverySubmitVerdictCommand {
        actor: Actor::UserActor(UserActor {
            id: UserId(canonical_id("usr", seed)),
            kind: UserActorKind::User,
        }),
        command: DeliverySubmitVerdictCommandCommand::DeliverySubmitVerdict,
        expected_revision: Revision(i64::try_from(seeded.delivery.revision()).expect("revision")),
        payload: DeliverySubmitVerdictPayload {
            candidate_digest: seeded.candidate_digest.clone(),
            delivery_id: seeded.delivery.id().clone(),
        },
        request_id: RequestId(canonical_id("req", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: seeded.scope.clone(),
    }
}

fn start(seeded: &SeededVerdict) -> ControlPlane {
    ControlPlane::start_local_with_delivery_adapters(
        ControlPlaneConfig::local(&seeded.data),
        Box::new(NoopPublisher),
        LocalDeliveryAdapterConfig::new(&seeded.repository, seeded.scope.clone()),
    )
    .expect("production Control Plane")
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

fn runtime_stream_id(
    scope: &ReceiptScopeKey,
    job_id: &winwincode_domain::ExecutionJobId,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.runtime-ledger-stream.v1\0");
    digest.update((scope.as_bytes().len() as u64).to_be_bytes());
    digest.update(scope.as_bytes());
    digest.update((job_id.0.len() as u64).to_be_bytes());
    digest.update(job_id.0.as_bytes());
    format!("runtime:{:x}", digest.finalize())
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
        .status()
        .expect("run Git");
    assert!(status.success(), "Git command failed: {arguments:?}");
}

fn git_text(repository: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(repository)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run Git query");
    assert!(output.status.success(), "Git query failed: {arguments:?}");
    String::from_utf8(output.stdout)
        .expect("Git output")
        .trim()
        .to_owned()
}

fn delivery_state_revision(data: &Path) -> i64 {
    rusqlite::Connection::open(data.join("control-plane.sqlite3"))
        .expect("open database")
        .query_row(
            "SELECT revision FROM product_state WHERE stream_id LIKE 'delivery:%'",
            [],
            |row| row.get(0),
        )
        .expect("Delivery state revision")
}

fn cleanup(seeded: SeededVerdict) {
    fs::remove_dir_all(seeded.root).expect("fixture cleanup");
}

fn canonical_id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn unique_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "winwincode-production-verdict-{label}-{}-{}",
        std::process::id(),
        NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
    ))
}
