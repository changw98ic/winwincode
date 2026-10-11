// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use rusqlite::{Connection, params};
use serde_json::{Value, from_value};
use sha2::{Digest as _, Sha256};
use winwincode_control_plane::{
    ExecutionPortService,
    debug_hypothesis_ledger::{
        DebugHypothesisLedgerService, DebugHypothesisLedgerTransactionErrorKind,
        RecoveredDebugHypothesisLedger,
    },
};
use winwincode_domain::{
    ArtifactId, CodexThreadId, DebugHypothesisId, DebugSessionId, DeliveryId, Instant, ProbeId,
    ProbeRoundId, RequestId, SessionIdentity, Sha256Digest, WorkspaceRevision,
};
use winwincode_execution_port::{
    debug_hypothesis_ledger::{
        DebugHypothesisLedgerReducer, canonical_probe_round_receipt_bytes,
        derive_debug_unresolved_question_digest, seal_debug_hypothesis_round_evidence,
    },
    debug_probe_contract::{
        ValidatedDebugProbePlan, derive_debug_probe_plan_digest, derive_probe_budget_digest,
        derive_probe_command_arg_bytes, derive_probe_definition_digest, seal_debug_probe_plan,
        seal_probe_execution_intent,
    },
    debug_probe_delta_context::{
        ContextSafetyScanError, DebugContextSafetyScanner, DebugProbeDeltaContextInput,
        ValidatedDebugProbeDeltaContext, prepare_debug_probe_delta_context,
    },
    generated::{
        ArtifactReference, DebugContextSafetyProfile, DebugContextSafetyScannerVersion,
        DebugHypothesis, DebugHypothesisLedgerSeed, DebugHypothesisLedgerUpdate,
        DebugHypothesisStatus, DebugProbeKind, DebugProbePlan, DebugProbeRoundAuthority,
        DebugSessionStatus, DebugUnresolvedQuestion, ExecutionJob, ExecutionPortMessage,
        ExecutionWorkspaceWriteMode, JobDispatchMessage, JobDispatchResultMessage,
        JobDispatchResultMessageStatus, ProbeCommandSpec, ProbeCompletionRule,
        ProbeCompletionRuleKind, ProbeExecutionIntent, ProbeExecutionReceipt, ProbeNetworkAccess,
        ProbeNormalizerProfile, ProbeNormalizerVersion, ProbeRawStream, ProbeReceiptStatus,
        ProbeResourceClaim, ProbeRoundBudget, ProbeRoundBudgetUsage, ProbeRoundCompletionReason,
        ProbeRoundReceipt, ProbeRoundReceiptStatus, ProbeSideEffectClass, ProbeSpec,
        ProbeWorkspaceAccess, WorkerRegisterMessage,
    },
    probe_result_normalizer::{
        ProbeEvidenceProjection, ProbeRawStreamInput, canonical_probe_evidence_bundle_bytes,
        derive_probe_normalizer_profile_digest, normalize_probe_evidence,
        probe_baseline_not_applicable, project_probe_evidence, seal_probe_normalizer_profile,
    },
};
use winwincode_storage::{
    AggregateJournalKey, ExecutionAuthorityCommitGuard, ExecutionJobState, ExecutionJobSubmission,
    ExecutionJobTransitionRequest, ExecutionLeaseClaim, ExecutionQueueScope, NewOutboxEvent,
    ProductStateStorage, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey, SqliteStorage,
    StateCommit,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct AcceptingScanner(DebugContextSafetyProfile);

impl DebugContextSafetyScanner for AcceptingScanner {
    fn profile(&self) -> &DebugContextSafetyProfile {
        &self.0
    }

    fn validate(&self, _text: &str) -> Result<(), ContextSafetyScanError> {
        Ok(())
    }
}

struct RoundFixture {
    plan: ValidatedDebugProbePlan,
    evidence:
        winwincode_execution_port::debug_hypothesis_ledger::ValidatedDebugHypothesisRoundEvidence,
    alternate_projection: ProbeEvidenceProjection,
}

fn fixture_message<T: serde::de::DeserializeOwned>(kind: &str) -> T {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .expect("canonical ExecutionPort fixture");
    fixture["messages"]
        .as_array()
        .expect("fixture messages")
        .iter()
        .find(|message| message["kind"] == kind)
        .cloned()
        .map(from_value)
        .expect("fixture kind")
        .unwrap_or_else(|error| panic!("{kind} fixture must decode: {error}"))
}

fn digest(character: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", character.to_string().repeat(64)))
}

fn artifact(character: char, bytes: &[u8]) -> ArtifactReference {
    ArtifactReference {
        artifact_id: ArtifactId(format!("art_{}", character.to_string().repeat(26))),
        digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))),
    }
}

fn temporary_directory() -> std::path::PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-debug-hypothesis-ledger-{}-{suffix}",
        std::process::id()
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DurableCounts {
    states: i64,
    receipts: i64,
    outbox: i64,
    journal_records: i64,
}

fn durable_counts(root: &Path) -> DurableCounts {
    let connection =
        Connection::open(root.join("control-plane.sqlite3")).expect("inspect database");
    let count = |table: &str| {
        connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap_or_else(|error| panic!("count {table}: {error}"))
    };
    DurableCounts {
        states: count("product_state"),
        receipts: count("command_receipts"),
        outbox: count("outbox"),
        journal_records: count("aggregate_journal_records"),
    }
}

fn replacement_worker_session(value: &str) -> String {
    let mut replacement = value.to_owned();
    let current = replacement.pop().expect("non-empty Worker session ID");
    replacement.push(if current == '9' { '8' } else { '9' });
    replacement
}

fn test_journal_record_digest(payload: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.debug-hypothesis-ledger.journal-record.v1\0");
    digest.update((payload.len() as u64).to_be_bytes());
    digest.update(payload);
    format!("sha256:{:x}", digest.finalize())
}

#[derive(Clone, Copy)]
enum AuthorityTamper {
    StateOnly,
    JournalAndState,
}

fn tamper_prepared_authority(
    root: &Path,
    debug_session_id: &DebugSessionId,
    tamper: AuthorityTamper,
) {
    let connection = Connection::open(root.join("control-plane.sqlite3")).expect("tamper database");
    let stream_id = format!("debug-hypothesis-ledger:{}", debug_session_id.0);
    let state_bytes: Vec<u8> = connection
        .query_row(
            "SELECT payload FROM product_state WHERE stream_id = ?1",
            [&stream_id],
            |row| row.get(0),
        )
        .expect("Prepared state bytes");
    let mut state: Value = serde_json::from_slice(&state_bytes).expect("Prepared state JSON");
    let original = state["authority"]["worker_session_id"]
        .as_str()
        .expect("state Worker session ID");
    let replacement = replacement_worker_session(original);
    state["authority"]["worker_session_id"] = Value::String(replacement.clone());

    if matches!(tamper, AuthorityTamper::JournalAndState) {
        let record_bytes: Vec<u8> = connection
            .query_row(
                "SELECT payload FROM aggregate_journal_records \
                 WHERE aggregate_type = 'debug-hypothesis-ledger' \
                 AND aggregate_id = ?1 AND sequence = 1",
                [&debug_session_id.0],
                |row| row.get(0),
            )
            .expect("Prepared journal bytes");
        let mut record: Value =
            serde_json::from_slice(&record_bytes).expect("Prepared journal JSON");
        record["authority"]["worker_session_id"] = Value::String(replacement);
        let changed_record = serde_json::to_vec(&record).expect("changed journal bytes");
        let changed_digest = test_journal_record_digest(&changed_record);
        connection
            .execute(
                "UPDATE aggregate_journal_records SET digest = ?1, payload = ?2 \
                 WHERE aggregate_type = 'debug-hypothesis-ledger' \
                 AND aggregate_id = ?3 AND sequence = 1",
                params![changed_digest, changed_record, debug_session_id.0],
            )
            .expect("tamper Prepared journal");
        state["journal_tail_digest"] = Value::String(changed_digest);
    }

    connection
        .execute(
            "UPDATE product_state SET payload = ?1 WHERE stream_id = ?2",
            params![
                serde_json::to_vec(&state).expect("changed state bytes"),
                stream_id
            ],
        )
        .expect("tamper Prepared state");
}

fn tamper_prepared_evidence(
    root: &Path,
    debug_session_id: &DebugSessionId,
    replacement: &winwincode_execution_port::debug_hypothesis_ledger::ValidatedDebugHypothesisRoundEvidence,
) {
    let connection = Connection::open(root.join("control-plane.sqlite3")).expect("tamper database");
    let record_bytes: Vec<u8> = connection
        .query_row(
            "SELECT payload FROM aggregate_journal_records \
             WHERE aggregate_type = 'debug-hypothesis-ledger' \
             AND aggregate_id = ?1 AND sequence = 1",
            [&debug_session_id.0],
            |row| row.get(0),
        )
        .expect("Prepared journal bytes");
    let mut record: Value = serde_json::from_slice(&record_bytes).expect("Prepared journal JSON");
    record["evidence"] = serde_json::to_value(replacement.cut()).expect("replacement evidence");
    let changed_record = serde_json::to_vec(&record).expect("changed journal bytes");
    let changed_digest = test_journal_record_digest(&changed_record);
    connection
        .execute(
            "UPDATE aggregate_journal_records SET digest = ?1, payload = ?2 \
             WHERE aggregate_type = 'debug-hypothesis-ledger' \
             AND aggregate_id = ?3 AND sequence = 1",
            params![changed_digest, changed_record, debug_session_id.0],
        )
        .expect("tamper Prepared journal evidence");

    let stream_id = format!("debug-hypothesis-ledger:{}", debug_session_id.0);
    let state_bytes: Vec<u8> = connection
        .query_row(
            "SELECT payload FROM product_state WHERE stream_id = ?1",
            [&stream_id],
            |row| row.get(0),
        )
        .expect("Prepared state bytes");
    let mut state: Value = serde_json::from_slice(&state_bytes).expect("Prepared state JSON");
    state["evidence_cut_digest"] =
        serde_json::to_value(&replacement.cut().evidence_cut_digest).expect("evidence digest");
    state["journal_tail_digest"] = Value::String(changed_digest);
    connection
        .execute(
            "UPDATE product_state SET payload = ?1 WHERE stream_id = ?2",
            params![
                serde_json::to_vec(&state).expect("changed state bytes"),
                stream_id
            ],
        )
        .expect("tamper Prepared state evidence");
}

fn claim_from_dispatch(dispatch: &JobDispatchMessage) -> ExecutionLeaseClaim {
    ExecutionLeaseClaim {
        expires_at: dispatch.lease.expires_at.clone(),
        fencing_token: dispatch.lease.fencing_token.clone(),
        issued_at: dispatch.lease.issued_at.clone(),
        job_id: dispatch.lease.job_id.clone(),
        lease_id: dispatch.lease.lease_id.clone(),
        message_id: dispatch.message_id.clone(),
        payload_digest: dispatch.job.payload_digest.clone(),
        request_id: dispatch.request_id.clone(),
        worker_id: dispatch.lease.worker_id.clone(),
        worker_instance_id: dispatch.lease.worker_instance_id.clone(),
        attempt: u64::try_from(dispatch.lease.attempt).expect("positive attempt"),
    }
}

fn commit_dispatch_intent(storage: &mut SqliteStorage, job: &ExecutionJob) {
    let identity = ReceiptIdentity::new(
        ReceiptActorKey::from_encoded(b"debug-ledger-test-actor".to_vec()).expect("actor"),
        ReceiptScopeKey::from_encoded(b"debug-ledger-test-scope".to_vec()).expect("scope"),
        RequestId("req_debug-ledger-dispatch".to_owned()),
    )
    .expect("identity");
    storage
        .commit(&StateCommit::new(
            identity,
            digest('d'),
            format!("delivery-execution-intent:{}", job.job_id.0),
            0,
            b"{}".to_vec(),
            vec![NewOutboxEvent::internal(
                format!("execution-job:{}", job.job_id.0),
                "execution.job.dispatch",
                serde_json::to_vec(job).expect("job bytes"),
            )],
        ))
        .expect("dispatch intent");
}

fn queue_scope(job: &ExecutionJob) -> (ExecutionQueueScope, Option<winwincode_domain::WorkRunId>) {
    match &job.scope {
        winwincode_execution_port::generated::ExecutionScope::WorkRunExecutionScope(scope) => (
            ExecutionQueueScope {
                organization_id: winwincode_domain::OrganizationId(
                    "org_00000000000000000000000001".to_owned(),
                ),
                workspace_id: winwincode_domain::WorkspaceId(
                    "wsp_00000000000000000000000001".to_owned(),
                ),
                project_id: winwincode_domain::ProjectId(
                    "prj_00000000000000000000000001".to_owned(),
                ),
                repository_id: job.workspace.repository_id.clone(),
                product_session_id: scope.product_session_id.clone(),
                delivery_id: Some(DeliveryId("dlv_00000000000000000000000001".to_owned())),
            },
            Some(scope.work_run_id.clone()),
        ),
        winwincode_execution_port::generated::ExecutionScope::ProductSessionExecutionScope(
            scope,
        ) => (
            ExecutionQueueScope {
                organization_id: winwincode_domain::OrganizationId(
                    "org_00000000000000000000000001".to_owned(),
                ),
                workspace_id: winwincode_domain::WorkspaceId(
                    "wsp_00000000000000000000000001".to_owned(),
                ),
                project_id: winwincode_domain::ProjectId(
                    "prj_00000000000000000000000001".to_owned(),
                ),
                repository_id: job.workspace.repository_id.clone(),
                product_session_id: scope.product_session_id.clone(),
                delivery_id: None,
            },
            None,
        ),
    }
}

fn seed_running_authority(
    storage: &mut SqliteStorage,
) -> (ExecutionAuthorityCommitGuard, ExecutionJob) {
    seed_authority_with_write_mode(storage, ExecutionWorkspaceWriteMode::ReadOnly)
}

fn seed_authority_with_write_mode(
    storage: &mut SqliteStorage,
    write_mode: ExecutionWorkspaceWriteMode,
) -> (ExecutionAuthorityCommitGuard, ExecutionJob) {
    let mut dispatch: JobDispatchMessage = fixture_message("job.dispatch");
    dispatch.job.workspace.checkout_revision = "0".repeat(40);
    dispatch.job.workspace.write_mode = write_mode;
    commit_dispatch_intent(storage, &dispatch.job);
    let claim = claim_from_dispatch(&dispatch);
    let (scope, work_run_id) = queue_scope(&dispatch.job);
    let submitted = storage
        .execution_queue()
        .expect("queue")
        .submit(&ExecutionJobSubmission {
            scope: scope.clone(),
            job_id: dispatch.job.job_id.clone(),
            request_id: RequestId("req_00000000000000000000000020".to_owned()),
            payload_digest: dispatch.job.payload_digest.clone(),
            dispatch_payload: serde_json::to_vec(&dispatch.job).expect("job payload"),
            attempt: 1,
            dependencies: Vec::new(),
            work_run_id,
            submitted_at: claim.issued_at.clone(),
        })
        .expect("queue submit");
    storage
        .execution_queue()
        .expect("queue")
        .transition(&ExecutionJobTransitionRequest {
            scope,
            job_id: dispatch.job.job_id.clone(),
            request_id: RequestId("req_00000000000000000000000021".to_owned()),
            expected_revision: submitted.job.revision,
            from: ExecutionJobState::Queued,
            to: ExecutionJobState::Leased,
            occurred_at: claim.issued_at.clone(),
        })
        .expect("queue lease");
    let register: WorkerRegisterMessage = fixture_message("worker.register");
    let mut service = ExecutionPortService::new(storage, register.sent_at.clone());
    service
        .handle(ExecutionPortMessage::WorkerRegisterMessage(register))
        .expect("worker registration");
    service
        .claim_execution_job(dispatch.job.clone(), claim)
        .expect("job claim");
    let result: JobDispatchResultMessage = fixture_message("job.dispatch_result");
    let response = service
        .accept_dispatch_result(result)
        .expect("dispatch result");
    assert_eq!(response.status, JobDispatchResultMessageStatus::Accepted);
    drop(service);
    let job = storage
        .load_execution_job_record(&dispatch.job.job_id)
        .expect("job read")
        .expect("job record");
    let authority = storage
        .execution_registry()
        .expect("registry")
        .load_dispatch_authority(&dispatch.job.job_id)
        .expect("dispatch read")
        .expect("dispatch authority");
    (
        ExecutionAuthorityCommitGuard::terminal_round(job, authority).expect("guard"),
        dispatch.job,
    )
}

fn round_authority(
    guard: &ExecutionAuthorityCommitGuard,
    job: &ExecutionJob,
    debug_session: u64,
) -> DebugProbeRoundAuthority {
    let work_run_id = guard.expected_job().work_run_id.clone();
    DebugProbeRoundAuthority {
        attempt: job.attempt,
        debug_session_id: DebugSessionId(format!("dbg_{debug_session:026}")),
        environment_digest: digest('1'),
        fencing_token: guard.expected_dispatch().lease().fencing_token.clone(),
        job_id: job.job_id.clone(),
        lease_id: guard.expected_dispatch().lease().lease_id.clone(),
        repository_id: job.workspace.repository_id.clone(),
        round_id: ProbeRoundId(format!("prn_{debug_session:026}")),
        session_identity: SessionIdentity {
            codex_thread_id: CodexThreadId(format!("cdx_{debug_session:026}")),
            product_session_id: guard.expected_job().scope.product_session_id.clone(),
            work_run_id,
            worker_session_id: guard.expected_dispatch().worker_session_id().clone(),
        },
        workspace_revision: WorkspaceRevision(format!(
            "git-tree:{}",
            job.workspace.checkout_revision
        )),
    }
}

#[allow(clippy::too_many_lines)]
fn sealed_round(authority: DebugProbeRoundAuthority) -> RoundFixture {
    let mut plan = DebugProbePlan {
        authority,
        budget: ProbeRoundBudget {
            budget_digest: digest('2'),
            parallel_probe_limit: 1,
            peak_memory_limit_bytes: 134_217_728,
            probe_limit: 1,
            total_command_arg_limit_bytes: 262_144,
            total_cpu_limit_millis: 10_000,
            total_output_limit_bytes: 1_024,
            wall_time_limit_millis: 300_000,
        },
        completion_rule: ProbeCompletionRule {
            kind: ProbeCompletionRuleKind::AllTerminal,
            minimum_completed_probes: 1,
            minimum_successful_probes: 1,
            stop_on_required_probe_failure: true,
        },
        created_at: Instant("2026-09-07T08:00:00.000Z".to_owned()),
        plan_digest: digest('3'),
        probes: vec![ProbeSpec {
            command: ProbeCommandSpec {
                argv: vec!["fixture-probe".to_owned()],
                command_arg_bytes: 1,
                working_directory: ".".to_owned(),
            },
            kind: DebugProbeKind::StaticAnalysis,
            output_limit_bytes: 1_024,
            probe_definition_digest: digest('4'),
            probe_id: ProbeId("prb_00000000000000000000000000".to_owned()),
            required: true,
            resources: ProbeResourceClaim {
                cpu_limit_millis: 10_000,
                database_keys: Vec::new(),
                exclusive_keys: Vec::new(),
                memory_limit_bytes: 134_217_728,
                network_access: ProbeNetworkAccess::None,
                paths: vec!["src".to_owned()],
                port_numbers: Vec::new(),
                service_keys: Vec::new(),
                side_effect_class: ProbeSideEffectClass::PureRead,
                workspace_access: ProbeWorkspaceAccess::ReadOnly,
            },
            target_hypothesis_ids: vec![DebugHypothesisId(
                "hyp_00000000000000000000000000".to_owned(),
            )],
            timeout_millis: 300_000,
        }],
        schema_version: 1,
    };
    plan.probes[0].command.command_arg_bytes =
        derive_probe_command_arg_bytes(&plan.probes[0].command.argv).expect("command bytes");
    plan.probes[0].probe_definition_digest =
        derive_probe_definition_digest(&plan.probes[0]).expect("probe digest");
    plan.budget.budget_digest = derive_probe_budget_digest(&plan.budget).expect("budget digest");
    plan.plan_digest = derive_debug_probe_plan_digest(&plan).expect("plan digest");
    let plan = seal_debug_probe_plan(plan.clone(), &plan.authority).expect("plan seal");
    let intent = seal_probe_execution_intent(
        ProbeExecutionIntent {
            created_at: Instant("2026-09-07T08:00:01.000Z".to_owned()),
            identity: plan
                .probe_identity(&plan.probes()[0].spec().probe_id)
                .expect("probe identity"),
            plan_digest: plan.plan().plan_digest.clone(),
            schema_version: 1,
            spec: plan.probes()[0].spec().clone(),
        },
        &plan,
    )
    .expect("intent seal");
    let output = b"ok";
    let raw_ref = artifact('A', output);
    let probe_receipt = ProbeExecutionReceipt {
        artifact_refs: vec![raw_ref.clone()],
        duration_millis: 10,
        error: None,
        exit_code: Some(0),
        finished_at: Instant("2026-09-07T08:00:03.000Z".to_owned()),
        identity: intent.intent().identity.clone(),
        output_bytes: 2,
        output_truncated: false,
        plan_digest: plan.plan().plan_digest.clone(),
        schema_version: 1,
        signal: None,
        started_at: Instant("2026-09-07T08:00:02.000Z".to_owned()),
        status: ProbeReceiptStatus::Succeeded,
        timed_out: false,
    };
    let mut profile = ProbeNormalizerProfile {
        diagnostic_parser_version: None,
        normalizer_version: ProbeNormalizerVersion::L0L1V1,
        profile_digest: digest('5'),
        stack_parser_version: None,
    };
    profile.profile_digest =
        derive_probe_normalizer_profile_digest(&profile).expect("normalizer digest");
    let profile = seal_probe_normalizer_profile(profile).expect("normalizer profile");
    let raw = [ProbeRawStreamInput::new(
        ProbeRawStream::Stdout,
        raw_ref,
        output,
    )];
    let bundle = normalize_probe_evidence(
        &intent,
        &probe_receipt,
        &profile,
        &raw,
        &probe_baseline_not_applicable(),
        Path::new("/workspace"),
    )
    .expect("normalized evidence");
    let bundle_bytes = canonical_probe_evidence_bundle_bytes(&bundle).expect("bundle bytes");
    let projection =
        project_probe_evidence(&bundle, artifact('B', &bundle_bytes)).expect("projection");
    let alternate_projection = project_probe_evidence(&bundle, artifact('C', &bundle_bytes))
        .expect("alternate projection");
    let receipt = ProbeRoundReceipt {
        authority: plan.plan().authority.clone(),
        completion_reason: ProbeRoundCompletionReason::AllProbesTerminal,
        error: None,
        finished_at: Instant("2026-09-07T08:00:04.000Z".to_owned()),
        plan_digest: plan.plan().plan_digest.clone(),
        probe_receipts: vec![probe_receipt],
        schema_version: 1,
        started_at: Instant("2026-09-07T08:00:01.000Z".to_owned()),
        status: ProbeRoundReceiptStatus::Completed,
        reducer: None,
        usage: ProbeRoundBudgetUsage {
            budget_digest: plan.plan().budget.budget_digest.clone(),
            elapsed_millis: 3_000,
            peak_memory_bytes: 100,
            peak_parallel_probes: 1,
            probe_count: 1,
            total_command_arg_bytes: intent.probe().command_arg_bytes(),
            total_cpu_millis: 10,
            total_output_bytes: 2,
        },
    };
    let receipt_bytes = canonical_probe_round_receipt_bytes(&receipt).expect("receipt bytes");
    let evidence = seal_debug_hypothesis_round_evidence(
        &plan,
        &receipt,
        artifact('R', &receipt_bytes),
        &[projection],
    )
    .expect("evidence seal");
    RoundFixture {
        plan,
        evidence,
        alternate_projection,
    }
}

fn alternate_evidence(
    round: &RoundFixture,
) -> winwincode_execution_port::debug_hypothesis_ledger::ValidatedDebugHypothesisRoundEvidence {
    seal_debug_hypothesis_round_evidence(
        &round.plan,
        round.evidence.receipt(),
        round
            .evidence
            .receipt_reference()
            .receipt_artifact_ref
            .clone(),
        std::slice::from_ref(&round.alternate_projection),
    )
    .expect("alternate evidence seal")
}

fn seed(authority: &DebugProbeRoundAuthority) -> DebugHypothesisLedgerSeed {
    DebugHypothesisLedgerSeed {
        authority: authority.clone(),
        created_at: Instant("2026-09-07T07:59:59.000Z".to_owned()),
        hypotheses: vec![DebugHypothesis {
            confidence_bps: 0,
            contradicting_evidence: Vec::new(),
            created_round_id: authority.round_id.clone(),
            hypothesis_id: DebugHypothesisId("hyp_00000000000000000000000000".to_owned()),
            last_updated_round_id: authority.round_id.clone(),
            status: DebugHypothesisStatus::Active,
            summary: "revision validation rejects a valid snapshot".to_owned(),
            supporting_evidence: Vec::new(),
        }],
        unresolved_questions: Vec::new(),
    }
}

fn scanner() -> AcceptingScanner {
    AcceptingScanner(DebugContextSafetyProfile {
        scanner_policy_digest: digest('a'),
        scanner_version: DebugContextSafetyScannerVersion::WorkspaceSecretScanV1,
    })
}

fn prepared_inputs(
    authority: DebugProbeRoundAuthority,
) -> (
    DebugHypothesisLedgerSeed,
    RoundFixture,
    ValidatedDebugProbeDeltaContext,
) {
    let seed = seed(&authority);
    let reducer = DebugHypothesisLedgerReducer::initialize(seed.clone()).expect("seed reducer");
    let round = sealed_round(authority);
    let context = prepare_debug_probe_delta_context(
        &DebugProbeDeltaContextInput {
            current_ledger: reducer.ledger(),
            previous_context: None,
            previous_ledger: None,
            round_evidence: &round.evidence,
            snippets: &[],
        },
        &scanner(),
    )
    .expect("delta context");
    (seed, round, context)
}

fn update_for(
    reducer: &DebugHypothesisLedgerReducer,
    round: &RoundFixture,
    context: &ValidatedDebugProbeDeltaContext,
) -> DebugHypothesisLedgerUpdate {
    let mut question = DebugUnresolvedQuestion {
        last_updated_round_id: round
            .evidence
            .receipt_reference()
            .authority
            .round_id
            .clone(),
        opened_round_id: round
            .evidence
            .receipt_reference()
            .authority
            .round_id
            .clone(),
        question_digest: digest('0'),
        summary: "which validation branch rejected the snapshot".to_owned(),
    };
    question.question_digest =
        derive_debug_unresolved_question_digest(&question).expect("question digest");
    DebugHypothesisLedgerUpdate {
        confirmed_facts: Vec::new(),
        mutations: Vec::new(),
        occurred_at: Instant("2026-09-07T08:00:05.000Z".to_owned()),
        opened_questions: vec![question],
        previous_ledger_digest: reducer.ledger().ledger().ledger_digest.clone(),
        reproduction_recipe_update: None,
        resolved_question_digests: Vec::new(),
        session_status: DebugSessionStatus::Active,
        source_context_digest: context.context().context_digest.clone(),
        source_request_digest: context.context().source_request_digest.clone(),
        source_round_receipt: round.evidence.receipt_reference().clone(),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn prepared_commit_restart_replay_conflict_and_stale_first_write_are_exact() {
    let root = temporary_directory();
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let (guard, job) = seed_running_authority(&mut storage);
    let authority = round_authority(&guard, &job, 1);
    let (seed, round, context) = prepared_inputs(authority.clone());
    let reducer = DebugHypothesisLedgerReducer::initialize(seed.clone()).expect("reducer");
    let update = update_for(&reducer, &round, &context);

    let mismatched_evidence = alternate_evidence(&round);
    let before_mismatched_evidence = durable_counts(&root);
    assert_eq!(
        DebugHypothesisLedgerService::new(&mut storage)
            .prepare_round(
                Some(seed.clone()),
                &round.plan,
                &mismatched_evidence,
                &context,
                &guard,
            )
            .expect_err("context from another evidence cut must not prepare")
            .kind(),
        DebugHypothesisLedgerTransactionErrorKind::InvalidInput
    );
    assert_eq!(durable_counts(&root), before_mismatched_evidence);

    let prepared = DebugHypothesisLedgerService::new(&mut storage)
        .prepare_round(
            Some(seed.clone()),
            &round.plan,
            &round.evidence,
            &context,
            &guard,
        )
        .expect("first prepare");
    assert_eq!(prepared.context_bytes(), context.canonical_bytes());
    assert!(!prepared.receipt().idempotent_replay);
    drop(storage);

    let mut storage = SqliteStorage::open(&root).expect("reopen after prepare");
    let recovered = DebugHypothesisLedgerService::new(&mut storage)
        .recover(&authority.debug_session_id)
        .expect("recover Prepared")
        .expect("Prepared state");
    let RecoveredDebugHypothesisLedger::Prepared { context_bytes, .. } = recovered else {
        panic!("restart must preserve Prepared");
    };
    assert_eq!(context_bytes, context.canonical_bytes());
    let replay = DebugHypothesisLedgerService::new(&mut storage)
        .prepare_round(Some(seed), &round.plan, &round.evidence, &context, &guard)
        .expect("prepare replay");
    assert!(replay.receipt().idempotent_replay);

    let late_authority = round_authority(&guard, &job, 2);
    let (late_seed, late_round, late_context) = prepared_inputs(late_authority.clone());
    let late_reducer =
        DebugHypothesisLedgerReducer::initialize(late_seed.clone()).expect("late reducer");
    let late_update = update_for(&late_reducer, &late_round, &late_context);
    DebugHypothesisLedgerService::new(&mut storage)
        .prepare_round(
            Some(late_seed),
            &late_round.plan,
            &late_round.evidence,
            &late_context,
            &guard,
        )
        .expect("prepare late-response round");

    let mut wrong_request = update.clone();
    wrong_request.source_request_digest = digest('9');
    let before_wrong_request = durable_counts(&root);
    assert_eq!(
        DebugHypothesisLedgerService::new(&mut storage)
            .commit_round(wrong_request, &guard)
            .expect_err("changed request digest must not commit")
            .kind(),
        DebugHypothesisLedgerTransactionErrorKind::InvalidInput
    );
    assert_eq!(durable_counts(&root), before_wrong_request);
    let committed = DebugHypothesisLedgerService::new(&mut storage)
        .commit_round(update.clone(), &guard)
        .expect("commit");
    assert_eq!(committed.ledger().event_sequence.0, 2);
    assert!(!committed.receipt().idempotent_replay);
    drop(storage);

    let mut storage = SqliteStorage::open(&root).expect("reopen after commit");
    let recovered = DebugHypothesisLedgerService::new(&mut storage)
        .recover(&authority.debug_session_id)
        .expect("recover Committed")
        .expect("Committed state");
    assert!(matches!(
        recovered,
        RecoveredDebugHypothesisLedger::Committed { .. }
    ));

    let running = storage
        .load_execution_job_record(&job.job_id)
        .expect("job read")
        .expect("running job");
    storage
        .execution_queue()
        .expect("queue")
        .transition(&ExecutionJobTransitionRequest {
            scope: running.scope.clone(),
            job_id: running.job_id.clone(),
            request_id: RequestId("req_00000000000000000000000022".to_owned()),
            expected_revision: running.revision,
            from: ExecutionJobState::Running,
            to: ExecutionJobState::Completed,
            occurred_at: Instant("2026-09-07T08:00:06.000Z".to_owned()),
        })
        .expect("advance authority");

    let before_late_commit = durable_counts(&root);
    assert_eq!(
        DebugHypothesisLedgerService::new(&mut storage)
            .commit_round(late_update, &guard)
            .expect_err("late model result must not commit")
            .kind(),
        DebugHypothesisLedgerTransactionErrorKind::StaleAuthority
    );
    assert_eq!(durable_counts(&root), before_late_commit);
    let late_recovered = DebugHypothesisLedgerService::new(&mut storage)
        .recover(&late_authority.debug_session_id)
        .expect("recover late Prepared")
        .expect("late Prepared state");
    assert!(matches!(
        late_recovered,
        RecoveredDebugHypothesisLedger::Prepared { .. }
    ));
    let late_journal = storage
        .load_journal(
            &AggregateJournalKey::new(
                "debug-hypothesis-ledger",
                late_authority.debug_session_id.0.clone(),
            )
            .expect("late journal key"),
        )
        .expect("late journal read")
        .expect("late journal");
    assert_eq!(late_journal.records.len(), 1);

    let replay = DebugHypothesisLedgerService::new(&mut storage)
        .commit_round(update.clone(), &guard)
        .expect("receipt-first commit replay after authority advance");
    assert!(replay.receipt().idempotent_replay);
    assert_eq!(replay.event(), committed.event());
    assert_eq!(replay.cursor(), committed.cursor());

    let mut changed = update;
    changed.opened_questions[0].summary = "changed replay body".to_owned();
    assert_eq!(
        DebugHypothesisLedgerService::new(&mut storage)
            .commit_round(changed, &guard)
            .expect_err("changed commit replay must conflict")
            .kind(),
        DebugHypothesisLedgerTransactionErrorKind::RequestConflict
    );

    let stale_authority = round_authority(&guard, &job, 3);
    let (stale_seed, stale_round, stale_context) = prepared_inputs(stale_authority.clone());
    let pending_before = storage.pending_events().expect("pending before").len();
    assert_eq!(
        DebugHypothesisLedgerService::new(&mut storage)
            .prepare_round(
                Some(stale_seed),
                &stale_round.plan,
                &stale_round.evidence,
                &stale_context,
                &guard,
            )
            .expect_err("stale first write must fail")
            .kind(),
        DebugHypothesisLedgerTransactionErrorKind::StaleAuthority
    );
    assert_eq!(
        storage.pending_events().expect("pending after").len(),
        pending_before
    );
    assert!(
        DebugHypothesisLedgerService::new(&mut storage)
            .recover(&stale_authority.debug_session_id)
            .expect("stale recovery")
            .is_none()
    );
    assert!(
        storage
            .load_journal(
                &AggregateJournalKey::new(
                    "debug-hypothesis-ledger",
                    stale_authority.debug_session_id.0,
                )
                .expect("journal key"),
            )
            .expect("stale journal read")
            .is_none()
    );

    drop(storage);
    fs::remove_dir_all(root).expect("remove fixture");
}

#[test]
fn prepared_recovery_rejects_state_or_journal_authority_substitution() {
    for (index, tamper) in [AuthorityTamper::StateOnly, AuthorityTamper::JournalAndState]
        .into_iter()
        .enumerate()
    {
        let root = temporary_directory();
        let mut storage = SqliteStorage::open(&root).expect("storage");
        let (guard, job) = seed_running_authority(&mut storage);
        let authority = round_authority(
            &guard,
            &job,
            u64::try_from(index + 10).expect("small fixture index"),
        );
        let (seed, round, context) = prepared_inputs(authority.clone());
        DebugHypothesisLedgerService::new(&mut storage)
            .prepare_round(Some(seed), &round.plan, &round.evidence, &context, &guard)
            .expect("prepare authority fixture");
        drop(storage);

        tamper_prepared_authority(&root, &authority.debug_session_id, tamper);
        let mut storage = SqliteStorage::open(&root).expect("reopen tampered storage");
        assert_eq!(
            DebugHypothesisLedgerService::new(&mut storage)
                .recover(&authority.debug_session_id)
                .expect_err("authority substitution must fail closed")
                .kind(),
            DebugHypothesisLedgerTransactionErrorKind::CorruptJournal
        );
        drop(storage);
        fs::remove_dir_all(root).expect("remove tamper fixture");
    }
}

#[test]
fn prepared_recovery_rejects_another_evidence_cut_for_the_same_receipt() {
    let root = temporary_directory();
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let (guard, job) = seed_running_authority(&mut storage);
    let authority = round_authority(&guard, &job, 20);
    let (seed, round, context) = prepared_inputs(authority.clone());
    let replacement = alternate_evidence(&round);
    DebugHypothesisLedgerService::new(&mut storage)
        .prepare_round(Some(seed), &round.plan, &round.evidence, &context, &guard)
        .expect("prepare evidence fixture");
    drop(storage);

    tamper_prepared_evidence(&root, &authority.debug_session_id, &replacement);
    let mut storage = SqliteStorage::open(&root).expect("reopen tampered storage");
    assert_eq!(
        DebugHypothesisLedgerService::new(&mut storage)
            .recover(&authority.debug_session_id)
            .expect_err("evidence-cut substitution must fail closed")
            .kind(),
        DebugHypothesisLedgerTransactionErrorKind::CorruptJournal
    );
    drop(storage);
    fs::remove_dir_all(root).expect("remove evidence fixture");
}

#[test]
fn candidate_workspace_authority_cannot_prepare_a_debug_ledger() {
    let root = temporary_directory();
    let mut storage = SqliteStorage::open(&root).expect("storage");
    let (guard, job) =
        seed_authority_with_write_mode(&mut storage, ExecutionWorkspaceWriteMode::Candidate);
    let authority = round_authority(&guard, &job, 30);
    let (seed, round, context) = prepared_inputs(authority.clone());
    let before = durable_counts(&root);

    assert_eq!(
        DebugHypothesisLedgerService::new(&mut storage)
            .prepare_round(Some(seed), &round.plan, &round.evidence, &context, &guard)
            .expect_err("DebugProbe requires a read-only workspace")
            .kind(),
        DebugHypothesisLedgerTransactionErrorKind::StaleAuthority
    );
    assert_eq!(durable_counts(&root), before);
    assert!(
        DebugHypothesisLedgerService::new(&mut storage)
            .recover(&authority.debug_session_id)
            .expect("candidate recovery")
            .is_none()
    );

    drop(storage);
    fs::remove_dir_all(root).expect("remove Candidate fixture");
}
