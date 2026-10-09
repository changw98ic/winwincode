// SPDX-License-Identifier: Apache-2.0

//! Offline regressions with real Git, `SQLite` and the public precise-rework path.
//! Worker usage below is a deterministic fact fixture, not a model receipt.

use super::*;

#[path = "rework_execution_fixture.rs"]
mod rework_execution_fixture;
use rework_execution_fixture::{
    ReworkAttemptConfig, ReworkEscape, claim_running_profile, claim_running_rework,
    commit_rework_escape, initialize_rework_repository, successful_profile_terminal,
    successful_rework_terminal,
};
use winwincode_api::generated::{
    DeliveryResolveAttentionCommand, DeliveryResolveAttentionCommandCommand,
    DeliveryResolveAttentionPayload, WorkRunStartCommand, WorkRunStartCommandCommand,
    WorkRunStartPayload,
};
use winwincode_delivery::application::candidate_rejection::{
    CandidateRejectionReason, current_candidate_rejection,
};
use winwincode_domain::ExecutionEventId;
use winwincode_execution_port::generated::{
    ExecutionEventCategory, ExecutionEventRecord, LeaseWriteStatus, RuntimeEventMessage,
    RuntimeEventMessageKind,
};

#[test]
fn persisted_rework_unauthorized_path_is_rejected_without_rewriting_worker_success() {
    assert_real_rework_rejection(89, ReworkEscape::UnauthorizedPath);
}

#[test]
fn persisted_rework_same_path_different_hunk_is_rejected_without_expanding_scope() {
    assert_real_rework_rejection(90, ReworkEscape::SamePathDifferentHunk);
}

// This is an offline event fact accepted through the actual runtime ingress.
// The initial writer later becomes Settled when its fail Verdict is committed.
fn persist_initial_writer_runtime(
    control_plane: &mut ControlPlane,
    scope: &RepositoryScope,
    terminal: &JobOutcomeMessage,
    facts: &winwincode_delivery::application::workrun_execution::DeliveryTerminalOutcomeFacts,
    seed: u64,
) {
    let runtime = RuntimeEventMessage {
        codex_thread_id: terminal.session_identity.codex_thread_id.clone(),
        event: ExecutionEventRecord {
            category: ExecutionEventCategory::Lifecycle,
            event_id: ExecutionEventId(canonical_id("xevt", seed + 80_000)),
            occurred_at: Instant("2027-01-15T08:00:59.750Z".into()),
            payload: None,
            sequence: ExecutionSequence(1),
            summary: "initial writer offline event fact".into(),
        },
        kind: RuntimeEventMessageKind::RuntimeEvent,
        lease: terminal.lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 80_000)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:59.750Z".into()),
        session_identity: terminal.session_identity.clone(),
        worker_session_id: terminal.worker_session_id.clone(),
    };
    let accepted = control_plane
        .accept_runtime_event(scope, &runtime, facts.authority(), &runtime.sent_at)
        .expect("accept initial writer runtime through the public ingress");
    assert_eq!(accepted.status, LeaseWriteStatus::Accepted);
    assert_eq!(accepted.ack_sequence, ExecutionAckSequence(1));
    assert!(accepted.error.is_none());
}

struct RealReworkFixture {
    seed: u64,
    root: PathBuf,
    repository: PathBuf,
    base_commit: String,
    source_commit: String,
    scope: RepositoryScope,
    delivery: Delivery,
    actor: Actor,
    candidate: winwincode_delivery::domain::FrozenDeliveryCandidate,
}

struct PersistedReworkOutcome {
    job: ExecutionJob,
    authorization: winwincode_execution_port::generated::DeliveryReworkAuthorizationScope,
    terminal: JobOutcomeMessage,
    escaped_commit: String,
    authority_stream: String,
    original_terminal: StoredState,
    original_state: Delivery,
}

fn assert_real_rework_rejection(seed: u64, escape: ReworkEscape) {
    let (fixture, control_plane) = seed_real_rework_writer(seed);
    let mut control_plane = settle_independent_rework_roles(&fixture, control_plane);
    let (job, authorization) = authorize_precise_rework(&fixture, &mut control_plane);
    control_plane.shutdown().expect("rework command shutdown");
    let expected = persist_escaped_rework(&fixture, job, authorization, escape);
    assert_rework_restarts(&fixture, &expected);
    assert_rework_authorization_and_usage(&fixture, &expected);
    fs::remove_dir_all(fixture.root).expect("rework fixture cleanup");
}

fn seed_real_rework_writer(seed: u64) -> (RealReworkFixture, ControlPlane) {
    let root = temporary_directory("sealed-rework-refusal");
    let repository = root.join("repository");
    let (base_commit, source_commit) = initialize_rework_repository(&repository);
    let repository = fs::canonicalize(repository).expect("canonical rework repository");
    let scope = repository_scope(seed);
    let delivery = repository_executor_delivery(seed, &repository, &base_commit);
    let mut initial_job = execution_job(&delivery, &scope);
    initial_job
        .workspace
        .checkout_revision
        .clone_from(&base_commit);
    let mut initial_terminal = terminal_message(
        &initial_job,
        &delivery,
        seed,
        ExecutionOutcomeStatus::Succeeded,
    );
    initial_terminal.sent_at = initial_terminal.outcome.finished_at.clone();
    initial_terminal.outcome.last_event_sequence = ExecutionAckSequence(1);
    seed_delivery_and_job(&root, &delivery, &initial_job);
    seed_authenticated_worker_execution(&root, &scope, &initial_job, &initial_terminal, seed);
    let mut control_plane = ControlPlane::start_local_with_delivery_adapters(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
        LocalDeliveryAdapterConfig::new(&repository, scope.clone()),
    )
    .expect("initial writer Control Plane");
    upload_initial_candidate(
        &mut control_plane,
        &repository,
        &base_commit,
        &scope,
        &delivery,
        &mut initial_terminal,
        &source_commit,
        seed,
    );
    let initial_facts = outcome_facts(&delivery, &initial_terminal);
    persist_initial_writer_runtime(
        &mut control_plane,
        &scope,
        &initial_terminal,
        &initial_facts,
        seed,
    );
    control_plane
        .commit_delivery_terminal_outcome(
            &scope,
            &initial_terminal,
            &initial_facts,
            &initial_terminal.sent_at,
        )
        .expect("persist actual initial writer success");
    control_plane.shutdown().expect("initial writer shutdown");

    let control_plane = ControlPlane::start_local_with_delivery_adapters(
        ControlPlaneConfig::local(&root),
        Box::new(RecordingPublisher),
        LocalDeliveryAdapterConfig::new(&repository, scope.clone()),
    )
    .expect("precise rework Control Plane");
    let candidate = resolve_initial_rework_candidate(
        &control_plane,
        &scope,
        &delivery,
        &initial_terminal,
        &initial_facts,
        &source_commit,
    );
    let actor = Actor::UserActor(UserActor {
        id: UserId(canonical_id("usr", seed)),
        kind: winwincode_domain::UserActorKind::User,
    });
    (
        RealReworkFixture {
            seed,
            root,
            repository,
            base_commit,
            source_commit,
            scope,
            delivery,
            actor,
            candidate,
        },
        control_plane,
    )
}

fn resolve_initial_rework_candidate(
    control_plane: &ControlPlane,
    scope: &RepositoryScope,
    delivery: &Delivery,
    terminal: &JobOutcomeMessage,
    facts: &winwincode_delivery::application::workrun_execution::DeliveryTerminalOutcomeFacts,
    source_commit: &str,
) -> winwincode_delivery::domain::FrozenDeliveryCandidate {
    let artifact = &terminal.outcome.artifacts[0];
    let candidate = control_plane
        .resolve_delivery_candidate(
            scope,
            delivery.id(),
            &artifact.artifact_id,
            &artifact.digest,
            facts,
        )
        .expect("real Git initial Candidate");
    assert_eq!(candidate.candidate_commit_id(), source_commit);
    assert_eq!(candidate.changed_paths().len(), 1);
    candidate
}

fn settle_independent_rework_roles(
    fixture: &RealReworkFixture,
    mut control_plane: ControlPlane,
) -> ControlPlane {
    // Each verification role is dispatched and settled only after the writer's
    // success. No future role bindings are seeded into an earlier revision.
    for (offset, profile, clock) in [(0, "reviewer", 3), (1, "verifier", 4)] {
        let current = load_delivery(&control_plane, fixture.delivery.id());
        control_plane
            .workrun_start(&WorkRunStartCommand {
                actor: fixture.actor.clone(),
                scope: fixture.scope.clone(),
                schema_version: SchemaVersion::WinwincodeV1,
                command: WorkRunStartCommandCommand::WorkRunStart,
                expected_revision: Revision(i64::try_from(current.revision()).unwrap()),
                request_id: RequestId(canonical_id("req", fixture.seed + 40_000 + offset)),
                payload: WorkRunStartPayload {
                    delivery_id: fixture.delivery.id().clone(),
                    dispatch_profile: profile.into(),
                    rework: None,
                },
            })
            .expect("public independent verification role start");
        let role_job = latest_queued_job(&fixture.root);
        control_plane
            .shutdown()
            .expect("verification dispatch shutdown");
        let running_role = claim_running_profile(
            ReworkAttemptConfig {
                root: &fixture.root,
                scope: &fixture.scope,
                delivery_id: fixture.delivery.id(),
                job: &role_job,
                scope_seed: fixture.seed,
                identity_seed: fixture.seed + 200_000 + offset * 10_000,
                clock,
            },
            &fixture.repository,
        );
        let (role_terminal, role_facts) = successful_profile_terminal(
            &running_role,
            vec![],
            fixture.seed + 200_000 + offset * 10_000,
        );
        control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(&fixture.root),
            Box::new(RecordingPublisher),
        )
        .expect("independent role terminal Control Plane");
        control_plane
            .commit_delivery_terminal_outcome(
                &fixture.scope,
                &role_terminal,
                &role_facts,
                &role_terminal.sent_at,
            )
            .expect("independent role terminal persists before the next stage");
        control_plane
            .shutdown()
            .expect("independent role terminal shutdown");
        control_plane = ControlPlane::start_local_with_delivery_adapters(
            ControlPlaneConfig::local(&fixture.root),
            Box::new(RecordingPublisher),
            LocalDeliveryAdapterConfig::new(&fixture.repository, fixture.scope.clone()),
        )
        .expect("restart after independent role terminal");
    }
    control_plane
}

fn authorize_precise_rework(
    fixture: &RealReworkFixture,
    control_plane: &mut ControlPlane,
) -> (
    ExecutionJob,
    winwincode_execution_port::generated::DeliveryReworkAuthorizationScope,
) {
    let verified = load_delivery(control_plane, fixture.delivery.id());
    let facts = verdict_facts_fixture(&verified, &fixture.candidate, VerdictFixtureOutcome::Fail);
    control_plane
        .commit_delivery_verdict(
            &verdict_command(fixture.seed, &verified, &fixture.candidate),
            SubmitVerdictFacts {
                expected_revision: verified.revision(),
                candidate: &fixture.candidate,
                verification: facts.verification(),
                evidence: facts.evidence(),
                produced_at_millis: 1_800_000_210_100,
            },
        )
        .expect("persist failed Verdict for the real initial Candidate");
    let failed = load_delivery(control_plane, fixture.delivery.id());
    let attention = failed
        .snapshot()
        .attention_items
        .iter()
        .find(|item| {
            item.options
                .iter()
                .any(|option| option.id == "start-rework")
        })
        .expect("bounded rework Attention");
    control_plane
        .delivery_resolve_attention(&DeliveryResolveAttentionCommand {
            actor: fixture.actor.clone(),
            scope: fixture.scope.clone(),
            schema_version: SchemaVersion::WinwincodeV1,
            command: DeliveryResolveAttentionCommandCommand::DeliveryResolveAttention,
            expected_revision: Revision(i64::try_from(failed.revision()).unwrap()),
            request_id: RequestId(canonical_id("req", fixture.seed + 30_000)),
            payload: DeliveryResolveAttentionPayload {
                attention_item_id: attention.id.clone(),
                delivery_id: fixture.delivery.id().clone(),
                decision: "resolve".into(),
                remediation: None,
                resolution: "Repair the failed candidate within its sealed source hunks".into(),
            },
        })
        .expect("resolve the current bounded rework Attention through the public API");
    let reworking = load_delivery(control_plane, fixture.delivery.id());
    assert_eq!(reworking.snapshot().status, DeliveryStatus::Reworking);
    control_plane
        .workrun_start(&WorkRunStartCommand {
            actor: fixture.actor.clone(),
            scope: fixture.scope.clone(),
            schema_version: SchemaVersion::WinwincodeV1,
            command: WorkRunStartCommandCommand::WorkRunStart,
            expected_revision: Revision(i64::try_from(reworking.revision()).unwrap()),
            request_id: RequestId(canonical_id("req", fixture.seed + 30_001)),
            payload: WorkRunStartPayload {
                delivery_id: fixture.delivery.id().clone(),
                dispatch_profile: "remediator".into(),
                rework: None,
            },
        })
        .expect("the public API derives and persists exact hunk authorization");
    let job = latest_queued_job(&fixture.root);
    let ExecutionScope::WorkRunExecutionScope(job_scope) = &job.scope else {
        panic!("rework WorkRun scope");
    };
    let authorization = job_scope
        .rework_authorization
        .clone()
        .expect("sealed rework authorization");
    assert_eq!(authorization.targets.len(), 1);
    assert_eq!(authorization.targets[0].file_path, "src/lib.rs");
    assert_eq!(
        authorization.source_candidate_commit_id,
        fixture.source_commit
    );

    (job, authorization)
}

fn persist_escaped_rework(
    fixture: &RealReworkFixture,
    job: ExecutionJob,
    authorization: winwincode_execution_port::generated::DeliveryReworkAuthorizationScope,
    escape: ReworkEscape,
) -> PersistedReworkOutcome {
    let running = claim_running_rework(ReworkAttemptConfig {
        root: &fixture.root,
        scope: &fixture.scope,
        delivery_id: fixture.delivery.id(),
        job: &job,
        scope_seed: fixture.seed,
        identity_seed: fixture.seed + 100_000,
        clock: 5,
    });
    let escaped_commit = commit_rework_escape(&fixture.repository, &fixture.source_commit, escape);
    let artifact = ArtifactReference {
        artifact_id: ArtifactId(canonical_id("art", fixture.seed + 100_000)),
        digest: Sha256Digest(format!("sha256:{}", "0".repeat(64))),
    };
    let (mut terminal, _) = successful_rework_terminal(&running, artifact, fixture.seed + 100_000);
    seed_timed_rework_artifact(
        &fixture.root,
        &fixture.repository,
        &fixture.base_commit,
        &fixture.scope,
        &running.delivery,
        &mut terminal,
        &escaped_commit,
        fixture.seed + 100_000,
        1_800_000_269_000,
    );
    let (terminal, terminal_facts) = successful_rework_terminal(
        &running,
        terminal.outcome.artifacts[0].clone(),
        fixture.seed + 100_000,
    );
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&fixture.root),
        Box::new(RecordingPublisher),
    )
    .expect("historical successful remediator Control Plane");
    control_plane
        .commit_delivery_terminal_outcome(
            &fixture.scope,
            &terminal,
            &terminal_facts,
            &terminal.sent_at,
        )
        .expect("historical remediator success is persisted before CP source acceptance");
    let authority_stream = format!("delivery-terminal-authority:{}", job.job_id.0);
    let original_terminal = control_plane
        .load_state(&authority_stream)
        .expect("terminal read")
        .expect("original successful terminal");
    let original_state = load_delivery(&control_plane, fixture.delivery.id());
    assert_eq!(
        original_state
            .snapshot()
            .work_run_aggregate
            .runs
            .iter()
            .find(|run| run.execution_job_id == job.job_id)
            .unwrap()
            .state,
        winwincode_domain::WorkRunState::CandidateReady
    );
    control_plane.shutdown().expect("durable success shutdown");

    PersistedReworkOutcome {
        job,
        authorization,
        terminal,
        escaped_commit,
        authority_stream,
        original_terminal,
        original_state,
    }
}

fn assert_rework_restarts(fixture: &RealReworkFixture, expected: &PersistedReworkOutcome) {
    let mut exact_rejected_state = None;
    for _ in 0..2 {
        let mut control_plane = ControlPlane::start_local_with_delivery_adapters(
            ControlPlaneConfig::local(&fixture.root),
            Box::new(RecordingPublisher),
            LocalDeliveryAdapterConfig::new(&fixture.repository, fixture.scope.clone()),
        )
        .expect("post-success remediator restart");
        let mut storage = SqliteStorage::open(&fixture.root).expect("terminal replay storage");
        for _ in 0..2 {
            let response = DurableExecutionPortIngress::new(
                &mut control_plane,
                &mut storage,
                &fixture.scope,
                expected.terminal.sent_at.clone(),
            )
            .expect("actual remediator ingress")
            .handle(&ExecutionPortMessage::JobOutcomeMessage(
                expected.terminal.clone(),
            ))
            .expect("an unauthorized candidate must become a readable product refusal");
            let [ExecutionPortMessage::JobOutcomeAckMessage(ack)] = response.as_slice() else {
                panic!("terminal Ack");
            };
            assert_eq!(ack.status, JobOutcomeAckMessageStatus::Duplicate);
        }
        let state = assert_rework_outcome_preserved(fixture, expected, &control_plane);
        assert_rework_detail_history(fixture, &control_plane);
        if let Some(previous) = &exact_rejected_state {
            assert_eq!(previous, &state);
        } else {
            exact_rejected_state = Some(state);
        }
        Box::new(storage).close().expect("replay storage close");
        control_plane.shutdown().expect("refusal shutdown");
    }
}

fn assert_rework_outcome_preserved(
    fixture: &RealReworkFixture,
    expected: &PersistedReworkOutcome,
    control_plane: &ControlPlane,
) -> StoredState {
    let state = control_plane
        .load_state(&format!("delivery:{}", fixture.delivery.id().0))
        .expect("rejected Delivery state")
        .expect("rejected state retained");
    let rejected = Delivery::decode_json(&state.payload).expect("rejected Delivery readable");
    let refusal = current_candidate_rejection(&rejected).expect("exact typed refusal");
    assert_eq!(
        refusal.reason,
        CandidateRejectionReason::ReworkSourceOutsideAuthorization
    );
    assert_eq!(
        refusal.authorization_digest.as_ref(),
        Some(&expected.authorization.authorization_digest)
    );
    assert_eq!(refusal.candidate_commit_id, expected.escaped_commit);
    assert_eq!(
        refusal.artifact_id,
        expected.terminal.outcome.artifacts[0].artifact_id
    );
    assert_eq!(
        refusal.artifact_digest,
        expected.terminal.outcome.artifacts[0].digest
    );
    assert_eq!(refusal.job_id, expected.job.job_id);
    assert_eq!(refusal.attempt, 1);
    assert_eq!(rejected.snapshot().status, DeliveryStatus::NeedsAttention);
    assert_eq!(
        rejected.snapshot().work_run_aggregate.runs.len(),
        expected
            .original_state
            .snapshot()
            .work_run_aggregate
            .runs
            .len(),
        "CP refusal must not dispatch a Reviewer or append a new run"
    );
    assert_eq!(
        rejected.snapshot().session_bindings,
        expected.original_state.snapshot().session_bindings
    );
    assert_eq!(
        rejected.snapshot().work_run_aggregate.runs,
        expected.original_state.snapshot().work_run_aggregate.runs
    );
    assert_eq!(
        rejected.snapshot().verdict,
        expected.original_state.snapshot().verdict
    );
    assert_eq!(
        rejected.snapshot().evidence,
        expected.original_state.snapshot().evidence
    );
    assert_eq!(
        control_plane
            .load_state(&expected.authority_stream)
            .expect("terminal preserved")
            .expect("original successful terminal preserved")
            .payload,
        expected.original_terminal.payload
    );
    state
}

fn assert_rework_detail_history(fixture: &RealReworkFixture, control_plane: &ControlPlane) {
    let query = winwincode_api::generated::DeliveryGetQuery {
        actor: fixture.actor.clone(),
        scope: fixture.scope.clone(),
        schema_version: SchemaVersion::WinwincodeV1,
        request_id: RequestId(canonical_id("req", fixture.seed + 30_002)),
        query: winwincode_api::generated::DeliveryGetQueryQuery::DeliveryGet,
        page: winwincode_api::generated::PageRequest {
            cursor: None,
            limit: 20,
        },
        parameters: winwincode_api::generated::DeliveryGetParameters {
            at_cursor: None,
            delivery_id: fixture.delivery.id().clone(),
        },
    };
    let response = winwincode_control_plane::strongflow_projection::StrongFlowProjectionQueryPort::delivery_get(control_plane, &query)
        .expect("real rework refusal remains queryable");
    let winwincode_api::generated::QueryResultResponse::DeliveryGetResultResponse(response) =
        response
    else {
        panic!("Delivery detail");
    };
    assert!(response.result.current_candidate.is_none());
    assert!(
        response.result.verdict.is_none(),
        "old failed Verdict is not the current refused candidate's Verdict"
    );
    assert!(
        response.result.evidence.is_empty(),
        "old Evidence is not promoted onto a refused candidate"
    );
    let history = winwincode_control_plane::strongflow_projection::StrongFlowProjectionQueryPort::candidate_history_list(
        control_plane, &winwincode_api::generated::CandidateHistoryListQuery {
            actor: fixture.actor.clone(), scope: fixture.scope.clone(), schema_version: SchemaVersion::WinwincodeV1,
            request_id: query.request_id.clone(), page: query.page.clone(),
            query: winwincode_api::generated::CandidateHistoryListQueryQuery::CandidateList,
            parameters: winwincode_api::generated::CandidateHistoryListParameters {
                at_cursor: response.result.read_cursor, delivery_id: fixture.delivery.id().clone(), read_page_limit: 20,
            },
        },
    ).expect("history retains the real authorized source Candidate while omitting only its refused replacement");
    let winwincode_api::generated::QueryResultResponse::CandidateHistoryListResultResponse(history) =
        history
    else {
        panic!("Candidate history");
    };
    assert_eq!(
        history.result.items.len(),
        1,
        "the legal source Candidate must not be hidden"
    );
    assert_eq!(
        history.result.items[0].candidate.candidate_ref,
        fixture.candidate.candidate_ref()
    );
}

fn assert_rework_authorization_and_usage(
    fixture: &RealReworkFixture,
    expected: &PersistedReworkOutcome,
) {
    assert_eq!(
        changed_terminal_replay_status(&fixture.root, &fixture.scope, &expected.terminal),
        JobOutcomeAckMessageStatus::RejectedConflict
    );
    let retained_job = latest_queued_job(&fixture.root);
    let ExecutionScope::WorkRunExecutionScope(retained_scope) = retained_job.scope else {
        panic!("retained scope");
    };
    assert_eq!(
        retained_scope.rework_authorization.unwrap().targets,
        expected.authorization.targets,
        "CP refusal cannot expand the sealed hunk grant"
    );
    assert_eq!(
        worker_terminal_state(&fixture.root, &expected.job.job_id),
        ("settled".into(), 1)
    );
    let connection = rusqlite::Connection::open(fixture.root.join("control-plane.sqlite3"))
        .expect("rework usage audit");
    let usage: (i64, i64) = connection.query_row(
        "SELECT actual_tokens, actual_cost_microunits FROM execution_admission_reservations WHERE job_id = ?1",
        [&expected.job.job_id.0], |row| Ok((row.get(0)?, row.get(1)?)),
    ).expect("original successful rework usage");
    assert_eq!(usage, (40, 400));
    connection.close().expect("rework audit close");
}

fn load_delivery(control_plane: &ControlPlane, id: &DeliveryId) -> Delivery {
    let state = control_plane
        .load_state(&format!("delivery:{}", id.0))
        .expect("load Delivery")
        .expect("Delivery exists");
    Delivery::decode_json(&state.payload).expect("Delivery JSON")
}

fn latest_queued_job(root: &Path) -> ExecutionJob {
    let connection =
        rusqlite::Connection::open(root.join("control-plane.sqlite3")).expect("queue inspection");
    let payload: Vec<u8> = connection
        .query_row(
            "SELECT dispatch_payload FROM scheduler_execution_jobs ORDER BY rowid DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("queued remediator payload");
    connection.close().expect("queue inspection close");
    serde_json::from_slice(&payload).expect("queued remediator JSON")
}

#[allow(clippy::too_many_arguments)]
fn seed_timed_rework_artifact(
    root: &Path,
    repository: &Path,
    base: &str,
    scope: &RepositoryScope,
    delivery: &Delivery,
    message: &mut JobOutcomeMessage,
    candidate: &str,
    seed: u64,
    opened_at_millis: u64,
) {
    let bytes =
        GitCandidateArtifactManifest::new(candidate, candidate_bundle(repository, base, candidate))
            .expect("rework Git manifest")
            .encode()
            .expect("rework manifest bytes");
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes)));
    let artifact = message.outcome.artifacts.first_mut().unwrap();
    artifact.digest = digest.clone();
    let provenance = ArtifactProvenance::execution_job(
        message.lease.job_id.clone(),
        1,
        message.lease.lease_id.clone(),
        message.lease.fencing_token.clone(),
        message.lease.worker_id.clone(),
        message.lease.worker_instance_id.clone(),
        message.worker_session_id.clone(),
    )
    .expect("exact rework provenance");
    let objects = LocalArtifactObjectStore::open(root.join("artifacts")).expect("rework objects");
    let mut artifacts = ArtifactStore::open(root.join("artifact-catalog"), Box::new(objects))
        .expect("rework catalog");
    let scope_key = repository_receipt_scope(scope);
    artifacts
        .open_artifact(ArtifactOpen::new(
            scope_key.clone(),
            ExecutionMessageId(canonical_id("xmsg", seed + 70_000)),
            RequestId(canonical_id("req", seed + 70_000)),
            artifact.artifact_id.clone(),
            "candidate",
            "application/vnd.winwincode.git-candidate+json",
            digest.clone(),
            u64::try_from(bytes.len()).unwrap(),
            Some("candidate.json".into()),
            provenance.clone(),
            ArtifactMeteringAttribution {
                organization_id: scope.organization_id.clone(),
                workspace_id: scope.workspace_id.clone(),
                project_id: scope.project_id.clone(),
                repository_id: scope.repository_id.clone(),
                delivery_id: Some(delivery.id().clone()),
                product_session_id: Some(message.session_identity.product_session_id.clone()),
                user_id: UserId(canonical_id("usr", seed)),
            },
            ArtifactRetention::Indefinite,
            opened_at_millis,
        ))
        .expect("rework Artifact open after the actual lease");
    artifacts
        .append_chunk(&ArtifactChunk::new(
            scope_key,
            ExecutionMessageId(canonical_id("xmsg", seed + 70_001)),
            artifact.artifact_id.clone(),
            provenance,
            opened_at_millis + 500,
            1,
            "application/octet-stream",
            digest,
            bytes,
            true,
        ))
        .expect("rework Artifact completed before the successful terminal");
    artifacts.close().expect("rework catalog close");
}

#[allow(clippy::too_many_arguments)]
fn upload_initial_candidate(
    control_plane: &mut ControlPlane,
    repository: &Path,
    base: &str,
    scope: &RepositoryScope,
    delivery: &Delivery,
    terminal: &mut JobOutcomeMessage,
    commit: &str,
    seed: u64,
) {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use winwincode_execution_port::generated::{
        ArtifactChunkMessage, ArtifactChunkMessageKind, ArtifactDescriptor, ArtifactKind,
        ArtifactOpenMessage, ArtifactOpenMessageKind, EncodedPayload, LeaseWriteStatus,
    };
    let bytes =
        GitCandidateArtifactManifest::new(commit, candidate_bundle(repository, base, commit))
            .expect("initial candidate manifest")
            .encode()
            .expect("initial candidate manifest bytes");
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes)));
    terminal.outcome.artifacts[0].digest = digest.clone();
    let facts = outcome_facts(delivery, terminal);
    let open = ArtifactOpenMessage {
        replaces_artifact_id: None,
        kind: ArtifactOpenMessageKind::ArtifactOpen,
        artifact: ArtifactDescriptor {
            artifact_id: terminal.outcome.artifacts[0].artifact_id.clone(),
            digest: digest.clone(),
            file_name: Some("candidate.json".into()),
            kind: ArtifactKind::Candidate,
            media_type: "application/vnd.winwincode.git-candidate+json".into(),
            size_bytes: i64::try_from(bytes.len()).unwrap(),
        },
        lease: terminal.lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 75_000)),
        request_id: RequestId(canonical_id("req", seed + 75_000)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:59.000Z".into()),
        session_identity: terminal.session_identity.clone(),
        snapshot_id: None,
        worker_session_id: terminal.worker_session_id.clone(),
    };
    let opened = control_plane
        .accept_artifact_open(scope, &open, facts.authority())
        .expect("initial candidate public artifact.open");
    assert_eq!(opened.status, LeaseWriteStatus::Accepted);
    let chunk = ArtifactChunkMessage {
        artifact_id: open.artifact.artifact_id,
        is_final: true,
        kind: ArtifactChunkMessageKind::ArtifactChunk,
        lease: terminal.lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 75_001)),
        payload: EncodedPayload {
            content_type: "application/vnd.winwincode.git-candidate+json".into(),
            data_base64: STANDARD.encode(bytes),
            payload_digest: digest,
        },
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:59.500Z".into()),
        sequence: ExecutionSequence(1),
        session_identity: terminal.session_identity.clone(),
        snapshot_id: None,
        worker_session_id: terminal.worker_session_id.clone(),
    };
    let acknowledged = control_plane
        .accept_artifact_chunk(scope, &chunk, facts.authority())
        .expect("initial candidate final chunk accepted and its actual Git source pinned");
    assert_eq!(acknowledged.status, LeaseWriteStatus::Accepted);
    assert!(
        control_plane
            .pin_candidate_git_after_final_artifact_ack(
                scope,
                &chunk,
                &acknowledged,
                facts.authority()
            )
            .expect("exact final acknowledgement pin replay")
            .is_some()
    );
}
