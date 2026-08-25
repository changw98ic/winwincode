use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Barrier, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

use winwincode_api::generated::{
    Actor, ArtifactReference, CommandEnvelope, CommandName, DeliveryStageExecutionScope,
    DeliveryStageExecutionScopeKind, ExecutionJob, ExecutionLeaseStamp, ExecutionLimits,
    ExecutionOutcome, ExecutionOutcomeStatus, ExecutionScope, ExecutionWorkspace,
    ExecutionWorkspaceWriteMode, JobOutcomeMessage, JobOutcomeMessageKind, RepositoryScope,
    SchemaVersion, Scope, UserActor,
};
use winwincode_control_plane::{
    CommitError, ControlPlane, ControlPlaneConfig, EventPublishError, EventPublisher, OutboxEvent,
    StateChange,
};
use winwincode_delivery::{
    application::{
        stage::test_support::{
            active_lease_identity, delivery_terminal_outcome_facts, session_binding_authority,
            terminal_outcome_metadata, terminal_worker_outcome,
        },
        verdict::{
            SubmitVerdictFacts,
            test_support::{VerdictFixtureOutcome, verdict_facts_fixture, verdict_fixture},
        },
    },
    domain::{Delivery, DeliveryStatus, DeliveryTaskStatus, StageRunStatus},
    store::{
        AtomicPublication, CreateDelivery, DeliveryCommand, DeliveryCommandPort,
        DeliveryJournalPort, DeliveryStore, JournalBackendError, LoadedDeliveryJournal,
    },
};
use winwincode_domain::{
    ArtifactId, CodexThreadId, DeliveryId, ExecutionAckSequence, ExecutionJobId,
    ExecutionMessageId, FencingToken, Instant, LeaseId, OrganizationId, ProductSessionId,
    ProjectId, RepositoryId, RequestId, Revision, Sha256Digest, StageRunId, UserId, WorkerId,
    WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, NewOutboxEvent,
    ProductStateStorage, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey, SqliteStorage,
    StateCommit,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-terminal-outcome-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn canonical_id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
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

#[derive(Default)]
struct RecordingPublisher;

impl EventPublisher for RecordingPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Ok(())
    }
}

struct FailingPublisher;

impl EventPublisher for FailingPublisher {
    fn publish(&mut self, _event: &OutboxEvent) -> Result<(), EventPublishError> {
        Err(EventPublishError::new(
            "injected terminal publication failure",
        ))
    }
}

fn repository_scope(seed: u64) -> RepositoryScope {
    RepositoryScope {
        kind: winwincode_api::generated::RepositoryScopeKind::Repository,
        organization_id: OrganizationId(canonical_id("org", seed)),
        workspace_id: WorkspaceId(canonical_id("wsp", seed)),
        project_id: ProjectId(canonical_id("prj", seed)),
        repository_id: RepositoryId(canonical_id("rep", seed)),
    }
}

fn running_final_verifier(
    seed: u64,
) -> (
    Delivery,
    winwincode_delivery::domain::FrozenDeliveryCandidate,
) {
    let fixture = verdict_fixture(
        &DeliveryId(canonical_id("dlv", seed)),
        VerdictFixtureOutcome::Pass,
    );
    let mut snapshot = fixture.delivery.into_snapshot();
    let run = snapshot
        .stage_runs
        .iter_mut()
        .find(|run| run.role == "verifier")
        .expect("final verifier run");
    run.id = StageRunId(canonical_id("run", seed));
    run.status = StageRunStatus::Running;
    run.finished_at_millis = None;
    let binding = snapshot
        .session_bindings
        .iter_mut()
        .find(|binding| binding.id.0 == "binding-verifier-1")
        .expect("final verifier binding");
    binding.stage_run_id = run.id.clone();
    binding.product_session_id = ProductSessionId(canonical_id("psn", seed));
    binding.execution_job_id = ExecutionJobId(canonical_id("job", seed));
    binding.worker_session_id = Some(WorkerSessionId(canonical_id("wsn", seed)));
    binding.codex_thread_id = Some(CodexThreadId(canonical_id("cdx", seed)));
    let delivery = Delivery::try_from_snapshot(snapshot).expect("running final verifier Delivery");
    (delivery, fixture.candidate)
}

fn running_non_final_executor(seed: u64) -> Delivery {
    let (delivery, _candidate) = running_final_verifier(seed);
    let mut snapshot = delivery.into_snapshot();
    snapshot.status = DeliveryStatus::Executing;
    for task in &mut snapshot.tasks {
        task.status = DeliveryTaskStatus::Active;
    }
    let run = snapshot
        .stage_runs
        .iter_mut()
        .find(|run| run.id == StageRunId(canonical_id("run", seed)))
        .expect("active run");
    run.stage = winwincode_delivery::domain::DeliveryStage::Executing;
    run.role = "executor".into();
    Delivery::try_from_snapshot(snapshot).expect("running non-final executor")
}

fn execution_job(delivery: &Delivery, scope: &RepositoryScope) -> ExecutionJob {
    let run = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| run.status == StageRunStatus::Running)
        .expect("active run");
    let binding = delivery
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| binding.stage_run_id == run.id)
        .expect("final verifier binding");
    ExecutionJob {
        attempt: 1,
        execution_profile: "strongflow-verifier".into(),
        goal: "Verify the frozen candidate".into(),
        job_id: binding.execution_job_id.clone(),
        limits: ExecutionLimits {
            deadline_at: Instant("2027-01-15T09:00:00.000Z".into()),
            max_artifact_bytes: 10_000_000,
            max_runtime_seconds: 3_600,
        },
        payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        scope: ExecutionScope::DeliveryStageExecutionScope(DeliveryStageExecutionScope {
            delivery_id: delivery.id().clone(),
            delivery_task_id: run.delivery_task_id.clone(),
            kind: DeliveryStageExecutionScopeKind::DeliveryStage,
            product_session_id: binding.product_session_id.clone(),
            rework_authorization: None,
            stage_run_id: run.id.clone(),
        }),
        workspace: ExecutionWorkspace {
            checkout_revision: "candidate-checkout".into(),
            repository_id: scope.repository_id.clone(),
            write_mode: ExecutionWorkspaceWriteMode::Candidate,
        },
    }
}

fn seed_delivery_and_job(root: &Path, delivery: &Delivery, job: &ExecutionJob) {
    let seed = job
        .job_id
        .0
        .strip_prefix("job_")
        .and_then(|suffix| suffix.parse::<u64>().ok())
        .expect("fixture job suffix");
    let capture = CapturingJournal::default();
    DeliveryStore::borrowed(&capture)
        .execute(DeliveryCommand::SeedForTest(CreateDelivery {
            request_id: RequestId("seed-terminal-journal".into()),
            request_digest: "b".repeat(64),
            snapshot: delivery.clone(),
        }))
        .expect("seed journal publication");
    let AtomicPublication::Create {
        delivery_id,
        manifest,
        first_record,
    } = capture
        .publication
        .into_inner()
        .expect("publication lock")
        .expect("seed publication")
    else {
        panic!("seed must create the Delivery journal");
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
    let scope_key = repository_receipt_scope(&repository_scope(
        job.workspace
            .repository_id
            .0
            .strip_prefix("rep_")
            .and_then(|suffix| suffix.parse::<u64>().ok())
            .expect("fixture repository suffix"),
    ));
    let mut storage = SqliteStorage::open(root).expect("seed storage");
    let receipt = storage
        .commit(
            &StateCommit::new(
                ReceiptIdentity::new(
                    ReceiptActorKey::from_encoded(b"seed-actor".to_vec()).expect("actor key"),
                    scope_key,
                    RequestId(canonical_id("req", seed + 5_000)),
                )
                .expect("receipt identity"),
                Sha256Digest(format!("sha256:{}", "b".repeat(64))),
                format!("delivery:{}", delivery.id().0),
                0,
                delivery.encode_json().expect("Delivery JSON"),
                vec![NewOutboxEvent::internal(
                    format!("execution-job:{}", job.job_id.0),
                    "execution.job.dispatch",
                    serde_json::to_vec(job).expect("ExecutionJob JSON"),
                )],
            )
            .with_journal_publication(publication),
        )
        .expect("seed Delivery and ExecutionJob");
    storage
        .mark_published(&receipt.events[0].event_id)
        .expect("seed event acknowledgement");
    Box::new(storage).close().expect("seed close");
}

fn repository_receipt_scope(scope: &RepositoryScope) -> ReceiptScopeKey {
    fn field(encoded: &mut Vec<u8>, value: &[u8]) {
        encoded.extend_from_slice(&(value.len() as u64).to_be_bytes());
        encoded.extend_from_slice(value);
    }
    let mut encoded = Vec::new();
    field(&mut encoded, b"winwincode.command-receipt.scope.v1");
    field(&mut encoded, b"repository");
    for value in [
        &scope.organization_id.0,
        &scope.workspace_id.0,
        &scope.project_id.0,
        &scope.repository_id.0,
    ] {
        field(&mut encoded, value.as_bytes());
    }
    ReceiptScopeKey::from_encoded(encoded).expect("repository receipt scope")
}

fn install_terminal_failure(root: &Path, member: &str) {
    let sql = match member {
        "state" => {
            "CREATE TRIGGER fail_terminal_member BEFORE UPDATE ON product_state \
                    WHEN NEW.stream_id LIKE 'delivery:%' AND NEW.revision = 2 \
                    BEGIN SELECT RAISE(ABORT, 'injected terminal state failure'); END;"
        }
        "journal" => {
            "CREATE TRIGGER fail_terminal_member BEFORE INSERT ON aggregate_journal_records \
                      WHEN NEW.aggregate_type = 'delivery' AND NEW.sequence = 2 \
                      BEGIN SELECT RAISE(ABORT, 'injected terminal journal failure'); END;"
        }
        "receipt" => {
            "CREATE TRIGGER fail_terminal_member BEFORE INSERT ON command_receipts \
                      WHEN NEW.stream_id LIKE 'delivery:%' AND NEW.revision = 2 \
                      BEGIN SELECT RAISE(ABORT, 'injected terminal receipt failure'); END;"
        }
        "outbox" => {
            "CREATE TRIGGER fail_terminal_member BEFORE INSERT ON outbox \
                     WHEN NEW.topic = 'delivery.stage.terminal' \
                     BEGIN SELECT RAISE(ABORT, 'injected terminal outbox failure'); END;"
        }
        _ => panic!("unknown terminal atomic member"),
    };
    let connection =
        rusqlite::Connection::open(root.join("control-plane.sqlite3")).expect("failure injector");
    connection.execute_batch(sql).expect("failure trigger");
    connection.close().expect("failure injector close");
}

fn durable_terminal_counts(root: &Path, delivery_id: &DeliveryId) -> (i64, i64, i64, i64) {
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("durable count connection");
    let revision = connection
        .query_row(
            "SELECT revision FROM product_state WHERE stream_id = ?1",
            [format!("delivery:{}", delivery_id.0)],
            |row| row.get(0),
        )
        .expect("state revision");
    let journal = connection
        .query_row(
            "SELECT COUNT(*) FROM aggregate_journal_records WHERE aggregate_type = 'delivery' AND aggregate_id = ?1",
            [&delivery_id.0],
            |row| row.get(0),
        )
        .expect("journal count");
    let receipts = connection
        .query_row("SELECT COUNT(*) FROM command_receipts", [], |row| {
            row.get(0)
        })
        .expect("receipt count");
    let outbox = connection
        .query_row("SELECT COUNT(*) FROM outbox", [], |row| row.get(0))
        .expect("outbox count");
    connection.close().expect("durable count close");
    (revision, journal, receipts, outbox)
}

fn terminal_receipt_count(root: &Path, delivery_id: &DeliveryId) -> i64 {
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("terminal receipt count connection");
    let count = connection
        .query_row(
            "SELECT COUNT(*) FROM command_receipts WHERE stream_id = ?1 AND revision > 1",
            [format!("delivery:{}", delivery_id.0)],
            |row| row.get(0),
        )
        .expect("terminal receipt count");
    connection.close().expect("terminal receipt count close");
    count
}

fn terminal_message(
    job: &ExecutionJob,
    delivery: &Delivery,
    seed: u64,
    status: ExecutionOutcomeStatus,
) -> JobOutcomeMessage {
    let run = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| run.status == StageRunStatus::Running)
        .expect("active run");
    let binding = delivery
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| binding.stage_run_id == run.id)
        .expect("final verifier binding");
    JobOutcomeMessage {
        kind: JobOutcomeMessageKind::JobOutcome,
        lease: ExecutionLeaseStamp {
            attempt: 1,
            expires_at: Instant("2027-01-15T08:05:00.000Z".into()),
            fencing_token: FencingToken(seed.to_string()),
            issued_at: Instant("2027-01-15T08:00:00.200Z".into()),
            job_id: job.job_id.clone(),
            lease_id: LeaseId(canonical_id("lse", seed)),
            worker_id: WorkerId(canonical_id("wrk", seed)),
            worker_instance_id: WorkerInstanceId(canonical_id("wki", seed)),
        },
        message_id: ExecutionMessageId(canonical_id("xmsg", seed)),
        outcome: ExecutionOutcome {
            artifacts: vec![ArtifactReference {
                artifact_id: ArtifactId(canonical_id("art", seed)),
                digest: Sha256Digest(format!("sha256:{}", "c".repeat(64))),
            }],
            codex_thread_id: binding.codex_thread_id.clone(),
            error: None,
            finished_at: Instant("2027-01-15T08:01:00.000Z".into()),
            last_event_sequence: ExecutionAckSequence(12),
            status,
            summary: "Final verifier completed".into(),
        },
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:01:00.100Z".into()),
        worker_session_id: binding.worker_session_id.clone().expect("WorkerSession"),
    }
}

fn outcome_facts(
    delivery: &Delivery,
    message: &JobOutcomeMessage,
) -> winwincode_delivery::application::stage::DeliveryTerminalOutcomeFacts {
    let run = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| run.role == "verifier")
        .expect("final verifier run");
    outcome_facts_for_stage(delivery, message, run.id.clone())
}

fn outcome_facts_for_stage(
    _delivery: &Delivery,
    message: &JobOutcomeMessage,
    stage_run_id: StageRunId,
) -> winwincode_delivery::application::stage::DeliveryTerminalOutcomeFacts {
    let lease = active_lease_identity(
        message.lease.job_id.clone(),
        u64::try_from(message.lease.attempt).expect("attempt"),
        message.lease.lease_id.clone(),
        message.lease.fencing_token.clone(),
        message.lease.worker_id.clone(),
        message.lease.worker_instance_id.clone(),
        message.worker_session_id.clone(),
    );
    let authority = session_binding_authority(
        lease,
        message.lease.issued_at.clone(),
        message.lease.expires_at.clone(),
    );
    let metadata = terminal_outcome_metadata(
        message.outcome.codex_thread_id.clone(),
        1_800_000_060_000,
        message.outcome.last_event_sequence.clone(),
        message
            .outcome
            .artifacts
            .iter()
            .map(
                |artifact| winwincode_delivery::application::stage::TerminalArtifactReference {
                    artifact_id: artifact.artifact_id.clone(),
                    digest: artifact.digest.clone(),
                },
            )
            .collect(),
    );
    let outcome = terminal_worker_outcome(
        stage_run_id,
        message.lease.job_id.clone(),
        1,
        message.lease.lease_id.clone(),
        message.lease.fencing_token.clone(),
        message.lease.worker_id.clone(),
        message.lease.worker_instance_id.clone(),
        message.worker_session_id.clone(),
        match message.outcome.status {
            ExecutionOutcomeStatus::Succeeded => {
                winwincode_delivery::application::stage::TerminalOutcomeStatus::Succeeded
            }
            ExecutionOutcomeStatus::Failed => {
                winwincode_delivery::application::stage::TerminalOutcomeStatus::Failed
            }
            ExecutionOutcomeStatus::InfrastructureError => {
                winwincode_delivery::application::stage::TerminalOutcomeStatus::InfrastructureError
            }
            ExecutionOutcomeStatus::Cancelled => {
                winwincode_delivery::application::stage::TerminalOutcomeStatus::Cancelled
            }
        },
        metadata,
    );
    delivery_terminal_outcome_facts(authority, outcome)
}

fn verdict_command(
    seed: u64,
    delivery: &Delivery,
    candidate: &winwincode_delivery::domain::FrozenDeliveryCandidate,
) -> CommandEnvelope {
    CommandEnvelope {
        actor: Actor::UserActor(UserActor {
            id: UserId(canonical_id("usr", seed)),
            kind: winwincode_api::generated::UserActorKind::User,
        }),
        command: CommandName::DeliverySubmitVerdict,
        expected_revision: Revision(i64::try_from(delivery.revision()).expect("revision")),
        payload: serde_json::json!({
            "deliveryId": delivery.id().0,
            "candidateDigest": candidate.candidate_ref()
                .strip_prefix("git-candidate:")
                .expect("candidate digest"),
        }),
        request_id: RequestId(canonical_id("req", seed + 1000)),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: Scope::RepositoryScope(repository_scope(seed)),
    }
}

#[test]
fn final_verifier_outcome_is_durable_before_verdict() {
    let seed = 1;
    let root = temporary_directory("final-verifier");
    let scope = repository_scope(seed);
    let (delivery, candidate) = running_final_verifier(seed);
    let job = execution_job(&delivery, &scope);
    let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane start");

    let stale_fixture = verdict_fixture(delivery.id(), VerdictFixtureOutcome::Pass);
    control_plane
        .commit_delivery_verdict(
            &verdict_command(seed, &delivery, &candidate),
            SubmitVerdictFacts {
                expected_revision: delivery.revision(),
                candidate: &candidate,
                verification: &stale_fixture.verification,
                evidence: &stale_fixture.evidence,
                produced_at_millis: 1_800_000_060_100,
            },
        )
        .expect_err("a Running final verifier cannot produce a verdict");

    let commit = control_plane
        .commit_delivery_terminal_outcome(&scope, &message, &facts)
        .expect("terminal outcome commit");
    assert!(!commit.receipt().idempotent_replay);
    assert_eq!(commit.receipt().revision, 2);
    assert_eq!(commit.receipt().events.len(), 3);

    let stored = control_plane
        .load_state(&format!("delivery:{}", delivery.id().0))
        .expect("state read")
        .expect("Delivery state");
    let settled = Delivery::decode_json(&stored.payload).expect("settled Delivery");
    let final_verifier = settled
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| run.id == StageRunId(canonical_id("run", seed)))
        .expect("final verifier");
    assert_eq!(final_verifier.status, StageRunStatus::Succeeded);
    assert_eq!(final_verifier.finished_at_millis, Some(1_800_000_060_000));

    let verdict_facts = verdict_facts_fixture(&settled, &candidate, VerdictFixtureOutcome::Pass);
    control_plane
        .commit_delivery_verdict(
            &verdict_command(seed, &settled, &candidate),
            SubmitVerdictFacts {
                expected_revision: settled.revision(),
                candidate: &candidate,
                verification: verdict_facts.verification(),
                evidence: verdict_facts.evidence(),
                produced_at_millis: 1_800_000_060_100,
            },
        )
        .expect("verdict after terminal outcome");

    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn exact_replay_precedes_current_state_journal_job_and_replacement_facts() {
    let seed = 2;
    let root = temporary_directory("receipt-first-replay");
    let scope = repository_scope(seed);
    let (delivery, _candidate) = running_final_verifier(seed);
    let job = execution_job(&delivery, &scope);
    let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane start");
    let first = control_plane
        .commit_delivery_terminal_outcome(&scope, &message, &facts)
        .expect("initial terminal outcome");

    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("corruption injector");
    connection
        .execute(
            "UPDATE product_state SET payload = X'00' WHERE stream_id = ?1",
            [format!("delivery:{}", delivery.id().0)],
        )
        .expect("break current state");
    connection
        .execute(
            "DELETE FROM aggregate_journal_records WHERE aggregate_type = 'delivery' AND aggregate_id = ?1",
            [&delivery.id().0],
        )
        .expect("remove current journal");
    connection
        .execute(
            "DELETE FROM outbox WHERE event_id = ?1",
            [format!("execution-job:{}", job.job_id.0)],
        )
        .expect("remove durable job");
    connection.close().expect("corruption injector close");

    let mut replacement_message = message.clone();
    replacement_message.lease.lease_id = LeaseId(canonical_id("lse", seed + 100));
    let replacement_facts = outcome_facts(&delivery, &replacement_message);
    let replay = control_plane
        .commit_delivery_terminal_outcome(&scope, &message, &replacement_facts)
        .expect("receipt-first replay");

    assert!(replay.receipt().idempotent_replay);
    assert_eq!(replay.receipt().revision, first.receipt().revision);
    assert_eq!(replay.receipt().events, first.receipt().events);
    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn same_message_identity_with_changed_body_is_a_request_conflict() {
    let seed = 3;
    let root = temporary_directory("message-conflict");
    let scope = repository_scope(seed);
    let (delivery, _candidate) = running_final_verifier(seed);
    let job = execution_job(&delivery, &scope);
    let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane start");
    control_plane
        .commit_delivery_terminal_outcome(&scope, &message, &facts)
        .expect("initial terminal outcome");
    let mut changed = message.clone();
    changed.outcome.summary = "changed body under the same messageId".into();

    let error = control_plane
        .commit_delivery_terminal_outcome(&scope, &changed, &facts)
        .expect_err("same messageId cannot authorize another body");
    assert!(matches!(
        error,
        winwincode_control_plane::DeliveryTerminalOutcomeCommitError::Storage(ref source)
            if source.kind() == winwincode_control_plane::StorageErrorKind::RequestConflict
    ));
    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn a_new_message_cannot_resettle_an_already_terminal_stage_run() {
    let seed = 5;
    let root = temporary_directory("stale-new-message");
    let scope = repository_scope(seed);
    let (delivery, _candidate) = running_final_verifier(seed);
    let job = execution_job(&delivery, &scope);
    let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane start");
    control_plane
        .commit_delivery_terminal_outcome(&scope, &message, &facts)
        .expect("initial terminal outcome");
    let mut stale = message.clone();
    stale.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 100));

    control_plane
        .commit_delivery_terminal_outcome(&scope, &stale, &facts)
        .expect_err("a new message cannot settle the same StageRun twice");
    assert_eq!(durable_terminal_counts(&root, delivery.id()), (2, 2, 2, 4));
    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn concurrent_exact_message_returns_one_commit_and_only_durable_replays() {
    let seed = 4;
    let root = temporary_directory("concurrent-exact-message");
    let scope = repository_scope(seed);
    let (delivery, _candidate) = running_final_verifier(seed);
    let job = execution_job(&delivery, &scope);
    let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);

    let root = Arc::new(root);
    let barrier = Arc::new(Barrier::new(8));
    let handles = (0..8)
        .map(|_| {
            let root = Arc::clone(&root);
            let barrier = Arc::clone(&barrier);
            let scope = scope.clone();
            let message = message.clone();
            let facts = facts.clone();
            thread::spawn(move || {
                let mut control_plane = ControlPlane::start_local(
                    ControlPlaneConfig::local(root.as_path()),
                    Box::new(RecordingPublisher),
                )
                .expect("Control Plane start");
                barrier.wait();
                let receipt = control_plane
                    .commit_delivery_terminal_outcome(&scope, &message, &facts)
                    .expect("concurrent terminal outcome");
                let replayed = receipt.receipt().idempotent_replay;
                control_plane.shutdown().expect("shutdown");
                replayed
            })
        })
        .collect::<Vec<_>>();
    let replayed = handles
        .into_iter()
        .map(|handle| handle.join().expect("terminal outcome thread"))
        .collect::<Vec<_>>();

    assert_eq!(replayed.iter().filter(|value| !**value).count(), 1);
    assert_eq!(replayed.iter().filter(|value| **value).count(), 7);
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("durable count connection");
    let state_revision: i64 = connection
        .query_row(
            "SELECT revision FROM product_state WHERE stream_id = ?1",
            [format!("delivery:{}", delivery.id().0)],
            |row| row.get(0),
        )
        .expect("state revision");
    let journal_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM aggregate_journal_records WHERE aggregate_type = 'delivery' AND aggregate_id = ?1",
            [&delivery.id().0],
            |row| row.get(0),
        )
        .expect("journal count");
    let terminal_receipts: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM command_receipts WHERE stream_id = ?1 AND revision = 2",
            [format!("delivery:{}", delivery.id().0)],
            |row| row.get(0),
        )
        .expect("terminal receipt count");
    let terminal_events: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM outbox WHERE topic IN ('delivery.stage.terminal', 'delivery.changed.v1', 'runtime-projection.invalidated.v1')",
            [],
            |row| row.get(0),
        )
        .expect("terminal event count");
    assert_eq!(
        (
            state_revision,
            journal_count,
            terminal_receipts,
            terminal_events
        ),
        (2, 2, 1, 3)
    );
    connection.close().expect("durable count close");
    fs::remove_dir_all(root.as_path()).expect("database directory release");
}

#[test]
fn failed_infrastructure_and_cancelled_outcomes_settle_without_advancing_delivery() {
    for (offset, status, expected_run) in [
        (
            0_u64,
            ExecutionOutcomeStatus::Failed,
            StageRunStatus::Failed,
        ),
        (
            1,
            ExecutionOutcomeStatus::InfrastructureError,
            StageRunStatus::Failed,
        ),
        (
            2,
            ExecutionOutcomeStatus::Cancelled,
            StageRunStatus::Cancelled,
        ),
    ] {
        let seed = 10 + offset;
        let root = temporary_directory("unsuccessful-status");
        let scope = repository_scope(seed);
        let (delivery, _candidate) = running_final_verifier(seed);
        let job = execution_job(&delivery, &scope);
        let message = terminal_message(&job, &delivery, seed, status);
        let facts = outcome_facts(&delivery, &message);
        seed_delivery_and_job(&root, &delivery, &job);
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane start");

        control_plane
            .commit_delivery_terminal_outcome(&scope, &message, &facts)
            .expect("unsuccessful terminal outcome");
        let stored = control_plane
            .load_state(&format!("delivery:{}", delivery.id().0))
            .expect("state read")
            .expect("Delivery state");
        let settled = Delivery::decode_json(&stored.payload).expect("settled Delivery");
        let run = settled
            .snapshot()
            .stage_runs
            .iter()
            .find(|run| run.id == StageRunId(canonical_id("run", seed)))
            .expect("final verifier run");
        assert_eq!(run.status, expected_run);
        assert_eq!(settled.snapshot().status, DeliveryStatus::Verifying);
        assert!(
            settled
                .snapshot()
                .tasks
                .iter()
                .all(|task| task.status == DeliveryTaskStatus::Verifying)
        );
        control_plane.shutdown().expect("shutdown");
        fs::remove_dir_all(root).expect("database directory release");
    }
}

#[test]
fn failure_at_each_atomic_member_rolls_back_terminal_outcome() {
    for (offset, member) in ["state", "journal", "receipt", "outbox"]
        .into_iter()
        .enumerate()
    {
        let seed = 20 + u64::try_from(offset).expect("small atomic member index");
        let root = temporary_directory(member);
        let scope = repository_scope(seed);
        let (delivery, _candidate) = running_final_verifier(seed);
        let job = execution_job(&delivery, &scope);
        let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
        let facts = outcome_facts(&delivery, &message);
        seed_delivery_and_job(&root, &delivery, &job);
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane start");
        install_terminal_failure(&root, member);

        control_plane
            .commit_delivery_terminal_outcome(&scope, &message, &facts)
            .expect_err("injected atomic member failure");
        assert_eq!(
            durable_terminal_counts(&root, delivery.id()),
            (1, 1, 1, 1),
            "{member}"
        );
        control_plane.shutdown().expect("shutdown");
        fs::remove_dir_all(root).expect("database directory release");
    }
}

#[test]
fn publication_failure_keeps_terminal_commit_for_restart_and_receipt_replay() {
    let seed = 30;
    let root = temporary_directory("publication-restart");
    let scope = repository_scope(seed);
    let (delivery, _candidate) = running_final_verifier(seed);
    let job = execution_job(&delivery, &scope);
    let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);
    let mut failing =
        ControlPlane::start_local(ControlPlaneConfig::local(&root), Box::new(FailingPublisher))
            .expect("Control Plane start");

    let error = failing
        .commit_delivery_terminal_outcome(&scope, &message, &facts)
        .expect_err("publication must fail after commit");
    let committed = error
        .committed_receipt()
        .expect("publication error carries committed terminal receipt");
    assert_eq!(committed.receipt().revision, 2);
    assert_eq!(durable_terminal_counts(&root, delivery.id()), (2, 2, 2, 4));
    failing
        .shutdown()
        .expect_err("failing publisher leaves durable events pending");

    let mut restarted = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("restart publishes pending terminal events");
    let replay = restarted
        .commit_delivery_terminal_outcome(&scope, &message, &facts)
        .expect("receipt replay after restart");
    assert!(replay.receipt().idempotent_replay);
    assert_eq!(durable_terminal_counts(&root, delivery.id()), (2, 2, 2, 4));
    let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
        .expect("published event count connection");
    let pending: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM outbox WHERE published = 0",
            [],
            |row| row.get(0),
        )
        .expect("pending event count");
    assert_eq!(pending, 0);
    connection.close().expect("published event count close");
    restarted.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn stale_or_foreign_lease_binding_metadata_and_artifacts_fail_closed() {
    let seed = 40;
    let root = temporary_directory("foreign-terminal-facts");
    let scope = repository_scope(seed);
    let (delivery, _candidate) = running_final_verifier(seed);
    let job = execution_job(&delivery, &scope);
    let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    let facts = outcome_facts(&delivery, &message);
    seed_delivery_and_job(&root, &delivery, &job);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane start");

    let mut cases = Vec::new();
    let mut changed = message.clone();
    changed.lease.attempt = 2;
    cases.push(("attempt", changed));
    let mut changed = message.clone();
    changed.lease.lease_id = LeaseId(canonical_id("lse", seed + 1));
    cases.push(("lease", changed));
    let mut changed = message.clone();
    changed.lease.fencing_token = FencingToken((seed + 1).to_string());
    cases.push(("fence", changed));
    let mut changed = message.clone();
    changed.lease.worker_id = WorkerId(canonical_id("wrk", seed + 1));
    cases.push(("worker", changed));
    let mut changed = message.clone();
    changed.lease.worker_instance_id = WorkerInstanceId(canonical_id("wki", seed + 1));
    cases.push(("worker-instance", changed));
    let mut changed = message.clone();
    changed.worker_session_id = WorkerSessionId(canonical_id("wsn", seed + 1));
    cases.push(("worker-session", changed));
    let mut changed = message.clone();
    changed.lease.issued_at = Instant("2027-01-15T08:00:00.300Z".into());
    cases.push(("issued-at", changed));
    let mut changed = message.clone();
    changed.lease.expires_at = Instant("2027-01-15T08:06:00.000Z".into());
    cases.push(("expires-at", changed));
    let mut changed = message.clone();
    changed.outcome.codex_thread_id = Some(CodexThreadId(canonical_id("cdx", seed + 1)));
    cases.push(("codex-thread", changed));
    let mut changed = message.clone();
    changed.outcome.finished_at = Instant("2027-01-15T08:01:01.000Z".into());
    changed.sent_at = Instant("2027-01-15T08:01:01.100Z".into());
    cases.push(("finished-at", changed));
    let mut changed = message.clone();
    changed.outcome.last_event_sequence = ExecutionAckSequence(13);
    cases.push(("last-sequence", changed));
    let mut changed = message.clone();
    changed.outcome.artifacts[0].digest = Sha256Digest(format!("sha256:{}", "d".repeat(64)));
    cases.push(("artifact-digest", changed));
    let mut changed = message.clone();
    changed
        .outcome
        .artifacts
        .push(changed.outcome.artifacts[0].clone());
    cases.push(("duplicate-artifact", changed));
    let mut changed = message.clone();
    changed.sent_at = Instant("2027-01-15T08:06:00.000Z".into());
    cases.push(("sent-after-expiry", changed));

    for (name, changed) in cases {
        assert!(
            control_plane
                .commit_delivery_terminal_outcome(&scope, &changed, &facts)
                .is_err(),
            "foreign {name} must fail closed"
        );
        assert_eq!(
            durable_terminal_counts(&root, delivery.id()),
            (1, 1, 1, 1),
            "{name}"
        );
    }

    let foreign_stage_facts = outcome_facts_for_stage(
        &delivery,
        &message,
        StageRunId(canonical_id("run", seed + 1)),
    );
    control_plane
        .commit_delivery_terminal_outcome(&scope, &message, &foreign_stage_facts)
        .expect_err("foreign stage authority must fail closed");
    assert_eq!(durable_terminal_counts(&root, delivery.id()), (1, 1, 1, 1));
    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn missing_wrong_topic_corrupt_foreign_or_wrong_scope_execution_job_fails_closed() {
    for (offset, corruption) in [
        "missing",
        "wrong-topic",
        "unknown-field",
        "foreign-stage",
        "wrong-repository-scope",
    ]
    .into_iter()
    .enumerate()
    {
        let seed = 50 + u64::try_from(offset).expect("small corruption index");
        let root = temporary_directory(corruption);
        let scope = repository_scope(seed);
        let (delivery, _candidate) = running_final_verifier(seed);
        let job = execution_job(&delivery, &scope);
        let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
        let facts = outcome_facts(&delivery, &message);
        seed_delivery_and_job(&root, &delivery, &job);
        let event_id = format!("execution-job:{}", job.job_id.0);
        if corruption != "wrong-repository-scope" {
            let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
                .expect("ExecutionJob corruption injector");
            match corruption {
                "missing" => {
                    connection
                        .execute("DELETE FROM outbox WHERE event_id = ?1", [&event_id])
                        .expect("delete ExecutionJob");
                }
                "wrong-topic" => {
                    connection
                        .execute(
                            "UPDATE outbox SET topic = 'foreign.job' WHERE event_id = ?1",
                            [&event_id],
                        )
                        .expect("replace ExecutionJob topic");
                }
                "unknown-field" | "foreign-stage" => {
                    let mut value = serde_json::to_value(&job).expect("ExecutionJob value");
                    if corruption == "unknown-field" {
                        value
                            .as_object_mut()
                            .expect("ExecutionJob object")
                            .insert("unknownField".into(), serde_json::json!(true));
                    } else {
                        value["scope"]["stageRunId"] =
                            serde_json::json!(canonical_id("run", seed + 1));
                    }
                    connection
                        .execute(
                            "UPDATE outbox SET payload = ?1 WHERE event_id = ?2",
                            rusqlite::params![
                                serde_json::to_vec(&value).expect("ExecutionJob bytes"),
                                event_id
                            ],
                        )
                        .expect("replace ExecutionJob payload");
                }
                _ => unreachable!(),
            }
            connection
                .close()
                .expect("ExecutionJob corruption injector close");
        }
        let submitted_scope = if corruption == "wrong-repository-scope" {
            repository_scope(seed + 100)
        } else {
            scope
        };
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane start");

        control_plane
            .commit_delivery_terminal_outcome(&submitted_scope, &message, &facts)
            .expect_err("foreign durable ExecutionJob must fail closed");
        assert_eq!(
            control_plane
                .load_state(&format!("delivery:{}", delivery.id().0))
                .expect("state read")
                .expect("Delivery state")
                .revision,
            1,
            "{corruption}"
        );
        assert_eq!(
            terminal_receipt_count(&root, delivery.id()),
            0,
            "{corruption}"
        );
        control_plane.shutdown().expect("shutdown");
        fs::remove_dir_all(root).expect("database directory release");
    }
}

#[test]
fn successful_non_final_stage_outcome_is_rejected_without_writes() {
    let seed = 60;
    let root = temporary_directory("successful-non-final");
    let scope = repository_scope(seed);
    let delivery = running_non_final_executor(seed);
    let job = execution_job(&delivery, &scope);
    let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
    let facts = outcome_facts_for_stage(&delivery, &message, StageRunId(canonical_id("run", seed)));
    seed_delivery_and_job(&root, &delivery, &job);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane start");

    control_plane
        .commit_delivery_terminal_outcome(&scope, &message, &facts)
        .expect_err("successful executor settles only during atomic stage handoff");
    assert_eq!(durable_terminal_counts(&root, delivery.id()), (1, 1, 1, 1));
    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn generic_control_plane_commit_cannot_forge_terminal_delivery_state() {
    let seed = 61;
    let root = temporary_directory("generic-terminal-bypass");
    let scope = repository_scope(seed);
    let (delivery, candidate) = running_final_verifier(seed);
    let job = execution_job(&delivery, &scope);
    seed_delivery_and_job(&root, &delivery, &job);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
    )
    .expect("Control Plane start");
    let mut command = verdict_command(seed, &delivery, &candidate);
    command.command = CommandName::SessionCancel;
    command.request_id = RequestId(canonical_id("req", seed + 2_000));
    command.payload = serde_json::json!({});

    let error = control_plane
        .commit(
            &command,
            StateChange::new(
                format!("delivery:{}", delivery.id().0),
                b"forged-terminal-state".to_vec(),
                vec![NewOutboxEvent::internal(
                    "forged-terminal-event",
                    "delivery.stage.terminal",
                    b"forged".to_vec(),
                )],
            ),
        )
        .expect_err("generic commit must not write a reserved Delivery stream");
    assert!(matches!(
        error,
        CommitError::Storage(ref source)
            if source.kind() == winwincode_control_plane::StorageErrorKind::InvalidInput
    ));
    command.expected_revision = Revision(0);
    command.request_id = RequestId(canonical_id("req", seed + 2_001));
    let error = control_plane
        .commit(
            &command,
            StateChange::new(
                "worker:terminal-topic-bypass",
                b"unrelated-state".to_vec(),
                vec![NewOutboxEvent::internal(
                    "forged-terminal-topic",
                    "delivery.stage.terminal",
                    b"forged".to_vec(),
                )],
            ),
        )
        .expect_err("generic commit must not publish a reserved terminal topic");
    assert!(matches!(
        error,
        CommitError::Storage(ref source)
            if source.kind() == winwincode_control_plane::StorageErrorKind::InvalidInput
    ));
    assert!(
        control_plane
            .load_state("worker:terminal-topic-bypass")
            .expect("generic bypass state read")
            .is_none()
    );
    assert_eq!(durable_terminal_counts(&root, delivery.id()), (1, 1, 1, 1));
    control_plane.shutdown().expect("shutdown");
    fs::remove_dir_all(root).expect("database directory release");
}

#[test]
fn receipt_replay_rejects_changed_digest_event_membership_or_terminal_payload() {
    for (offset, corruption) in ["digest", "event-membership", "terminal-payload"]
        .into_iter()
        .enumerate()
    {
        let seed = 70 + u64::try_from(offset).expect("small corruption index");
        let root = temporary_directory(corruption);
        let scope = repository_scope(seed);
        let (delivery, _candidate) = running_final_verifier(seed);
        let job = execution_job(&delivery, &scope);
        let message = terminal_message(&job, &delivery, seed, ExecutionOutcomeStatus::Succeeded);
        let facts = outcome_facts(&delivery, &message);
        seed_delivery_and_job(&root, &delivery, &job);
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&root),
            Box::new(RecordingPublisher),
        )
        .expect("Control Plane start");
        control_plane
            .commit_delivery_terminal_outcome(&scope, &message, &facts)
            .expect("initial terminal outcome");
        let connection = rusqlite::Connection::open(root.join("control-plane.sqlite3"))
            .expect("receipt corruption injector");
        match corruption {
            "digest" => {
                connection
                    .execute(
                        "UPDATE command_receipts SET command_digest = ?1 WHERE stream_id = ?2 AND revision = 2",
                        rusqlite::params![
                            format!("sha256:{}", "f".repeat(64)),
                            format!("delivery:{}", delivery.id().0)
                        ],
                    )
                    .expect("replace terminal receipt digest");
            }
            "event-membership" => {
                connection
                    .execute(
                        "UPDATE outbox SET \
                           receipt_actor_key = (SELECT actor_key FROM command_receipts WHERE stream_id = ?1 AND revision = 1), \
                           receipt_scope_key = (SELECT scope_key FROM command_receipts WHERE stream_id = ?1 AND revision = 1), \
                           request_id = (SELECT request_id FROM command_receipts WHERE stream_id = ?1 AND revision = 1) \
                         WHERE topic = 'delivery.stage.terminal'",
                        [format!("delivery:{}", delivery.id().0)],
                    )
                    .expect("move terminal event to seed receipt");
            }
            "terminal-payload" => {
                connection
                    .execute(
                        "UPDATE outbox SET payload = X'7b7d' WHERE topic = 'delivery.stage.terminal'",
                        [],
                    )
                    .expect("replace terminal event payload");
            }
            _ => unreachable!(),
        }
        connection
            .close()
            .expect("receipt corruption injector close");

        control_plane
            .commit_delivery_terminal_outcome(&scope, &message, &facts)
            .expect_err("corrupt terminal receipt must fail closed");
        assert_eq!(
            control_plane
                .load_state(&format!("delivery:{}", delivery.id().0))
                .expect("state read")
                .expect("Delivery state")
                .revision,
            2,
            "{corruption}"
        );
        control_plane.shutdown().expect("shutdown");
        fs::remove_dir_all(root).expect("database directory release");
    }
}
