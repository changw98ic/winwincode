// SPDX-License-Identifier: Apache-2.0

#![allow(
    clippy::doc_markdown,
    clippy::large_futures,
    clippy::manual_inspect,
    clippy::map_unwrap_or,
    clippy::match_wildcard_for_single_variants,
    clippy::too_many_lines,
    clippy::uninlined_format_args
)]

//! Scheduler-to-Worker-to-StrongFlow stage-product composition fixtures.
//!
//! These tests deliberately use the real repository scheduler, SQLite
//! registry/queue/slot stores, Worker lifecycle, detached Git workspaces, and
//! canonical stage-product validators.  The in-test adapter is only the
//! narrow Codex boundary needed to make the longitudinal assertions
//! deterministic; it emits the same semantic products as the production
//! adapter and retains the same Worker-facing outbox records.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use winwincode_codex::stage_product::{
    VerificationEvidenceKind, VerificationEvidenceStatus, prepare_planner_solution_activity,
    prepare_verification_command_evidence, prepare_verification_policy_attestation,
    prepare_verification_result_activity, stage_product_prompt,
};
use winwincode_control_plane::RepositoryExecutionScheduler;
use winwincode_domain::{
    ArtifactId, CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionEventId, ExecutionJobId,
    ExecutionMessageId, ExecutionSequence, Instant, OrganizationId, ProductSessionId, ProjectId,
    RepositoryId, RepositoryScope, RepositoryScopeKind, RequestId, SchemaVersion, SessionIdentity,
    Sha256Digest, StageRunId, UserId, WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_execution_port::generated::{
    ArtifactAckMessage, ArtifactReference, DeliveryStageAcceptanceCriterionInput,
    DeliveryStageExecutionScope, DeliveryStageExecutionScopeKind, DeliveryStageInput,
    DeliveryStageTaskInput, EncodedPayload, ExecutionEventCategory, ExecutionEventRecord,
    ExecutionJob, ExecutionLeaseStamp, ExecutionLimits, ExecutionOutcomeStatus,
    ExecutionPortMessage, ExecutionScope, ExecutionWorkspace, ExecutionWorkspaceWriteMode,
    JobCancelAckMessageStatus, JobDispatchResultMessage, JobDispatchResultMessageStatus,
    JobOutcomeMessage, RuntimeEventMessage, RuntimeEventMessageKind, SessionBindingMessage,
    WorkerCapabilityFeature, WorkerCapabilitySet, WorkerCapabilitySetPlatform,
    WorkerRegisterMessage, WorkerRegistrationResultMessage, WorkerRegistrationResultMessageKind,
    WorkerRegistrationResultMessageLeaseRecovery, WorkerRegistrationResultMessageStatus,
};
use winwincode_storage::{
    EXECUTION_PROTOCOL_VERSION, ExecutionAdmissionBoundary, ExecutionAdmissionLimits,
    ExecutionAdmissionPolicy, ExecutionJobState, ExecutionJobSubmission,
    ExecutionLeaseTerminalOutcome, ExecutionLeaseTerminalRequest, ExecutionQueueScope,
    ExecutionRepositoryAccess, ExecutionReservationRelease, ExecutionReservationReleaseReason,
    ExecutionReservationRequest, ExecutionReservationSettlement, ExecutionReservationStart,
    LeaseRecovery, RepositorySchedulerCancellationRequest, RepositorySchedulerClaimRequest,
    RepositorySchedulerRetryRequest, RepositorySchedulerScope, RepositorySchedulerTerminalRequest,
    SchedulerRetryPolicy, SqliteStorage, WorkerAuthenticationIdentity, WorkerHeartbeatRequest,
    WorkerPlatform, WorkerPoolId, WorkerRegistrationRequest, WorkerSlotAuthority,
    WorkerSlotCancellation, WorkerSlotCloseRequest, WorkerSlotOpenRequest,
    WorkerSlotResourceLimits, WorkerSlotResources, WorkerSlotState,
};
use winwincode_worker::{
    CandidateArtifactAckOutcome, CandidateArtifactAuthority, CandidateArtifactUpload,
    CodexCoreAdapter, CodexPoll, CodexThreadStart, CodexTurnCompletion, DurableExecutionDelivery,
    RetainedCandidateArtifact, WorkerConfig, WorkerExecutionPort, WorkerMain,
    secret_safe_runtime_summary, workspace_runtime::JobWorkspaceRuntime,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
static NEXT_FRAME: AtomicU64 = AtomicU64::new(10_000);

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn frame_id(prefix: &str) -> String {
    id(prefix, NEXT_FRAME.fetch_add(1, Ordering::Relaxed))
}

fn at(second: u64) -> Instant {
    Instant(format!("2038-01-01T00:00:{:02}.000Z", second % 60))
}

fn repository_scope() -> RepositoryScope {
    RepositoryScope {
        kind: RepositoryScopeKind::Repository,
        organization_id: OrganizationId(id("org", 1)),
        workspace_id: WorkspaceId(id("wsp", 2)),
        project_id: ProjectId(id("prj", 3)),
        repository_id: RepositoryId(id("rep", 4)),
    }
}

fn scheduler_scope() -> RepositorySchedulerScope {
    let scope = repository_scope();
    RepositorySchedulerScope {
        organization_id: scope.organization_id,
        workspace_id: scope.workspace_id,
        project_id: scope.project_id,
        repository_id: scope.repository_id,
    }
}

fn queue_scope() -> ExecutionQueueScope {
    let scope = repository_scope();
    ExecutionQueueScope {
        organization_id: scope.organization_id,
        workspace_id: scope.workspace_id,
        project_id: scope.project_id,
        repository_id: scope.repository_id,
        product_session_id: ProductSessionId(id("psn", 5)),
        delivery_id: Some(DeliveryId(id("dlv", 6))),
    }
}

#[derive(Debug)]
struct Fixture {
    root: PathBuf,
    revision: String,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "winwincode-scheduler-stage-product-{label}-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let sources = root.join("sources");
        let repository = sources.join(queue_scope().repository_id.0);
        std::fs::create_dir_all(&repository).expect("create source repository");
        git(&repository, &["init", "-q"]);
        git(&repository, &["config", "user.name", "WinWinCode Fixture"]);
        git(
            &repository,
            &["config", "user.email", "fixture@example.invalid"],
        );
        std::fs::write(repository.join("fixture.txt"), b"source\n").expect("write source fixture");
        git(&repository, &["add", "fixture.txt"]);
        git(&repository, &["commit", "-qm", "source"]);
        let revision = git_output(&repository, &["rev-parse", "HEAD"]);
        Self { root, revision }
    }

    fn data(&self) -> PathBuf {
        self.root.join("data")
    }

    fn sources(&self) -> PathBuf {
        self.root.join("sources")
    }

    fn workspaces(&self) -> PathBuf {
        self.root.join("workspaces")
    }

    fn workspace_runtime(&self) -> JobWorkspaceRuntime {
        JobWorkspaceRuntime::open(self.workspaces(), self.sources())
            .expect("open fixture workspace runtime")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn git(repository: &Path, arguments: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .status()
        .expect("run git fixture command");
    assert!(status.success(), "git command failed: {arguments:?}");
}

fn git_output(repository: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .output()
        .expect("run git fixture output command");
    assert!(
        output.status.success(),
        "git output command failed: {arguments:?}"
    );
    String::from_utf8(output.stdout)
        .expect("git output utf8")
        .trim()
        .to_owned()
}

#[derive(Clone, Default)]
struct RecordingPort {
    messages: Rc<RefCell<Vec<ExecutionPortMessage>>>,
}

impl RecordingPort {
    fn messages(&self) -> Vec<ExecutionPortMessage> {
        self.messages.borrow().clone()
    }

    fn len(&self) -> usize {
        self.messages.borrow().len()
    }

    fn since(&self, cursor: usize) -> Vec<ExecutionPortMessage> {
        self.messages.borrow()[cursor..].to_vec()
    }
}

impl WorkerExecutionPort for RecordingPort {
    type Error = ();

    fn send(
        &mut self,
        message: ExecutionPortMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        self.messages.borrow_mut().push(message);
        std::future::ready(Ok(()))
    }
}

#[derive(Clone, Debug)]
struct SemanticEvent {
    category: ExecutionEventCategory,
    media_type: String,
    bytes: Vec<u8>,
    digest: Sha256Digest,
    summary: String,
}

#[derive(Clone, Debug)]
struct Run {
    job: ExecutionJob,
    lease: ExecutionLeaseStamp,
    worker_session_id: WorkerSessionId,
    session_identity: SessionIdentity,
    thread_id: CodexThreadId,
    workspace: PathBuf,
    polls: VecDeque<Result<CodexPoll, ()>>,
    cancel_requested: bool,
}

#[derive(Clone, Debug)]
struct CandidateRecord {
    authority: CandidateArtifactAuthority,
    artifact: ArtifactReference,
}

#[derive(Default)]
struct AdapterState {
    runs: HashMap<String, Run>,
    deliveries: HashMap<String, DurableExecutionDelivery>,
    pending_delivery_ids: HashSet<String>,
    candidates: HashMap<String, CandidateRecord>,
}

#[derive(Clone, Default)]
struct ScriptedStageProductAdapter {
    state: Arc<Mutex<AdapterState>>,
}

impl ScriptedStageProductAdapter {
    fn candidate_ref(&self) -> Option<String> {
        self.state
            .lock()
            .expect("adapter state")
            .candidates
            .values()
            .next()
            .map(|record| format!("git-candidate:{}", record.artifact.digest.0))
    }
}

fn semantic_from_prepared(
    category: ExecutionEventCategory,
    media_type: &str,
    bytes: &[u8],
    summary: &str,
) -> SemanticEvent {
    SemanticEvent {
        category,
        media_type: media_type.to_owned(),
        bytes: bytes.to_vec(),
        digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))),
        summary: summary.to_owned(),
    }
}

const PLANNER_JSON: &str = concat!(
    "{\"schemaVersion\":1,",
    "\"protocol\":\"winwincode.planner-solution.v1\",",
    "\"solution\":{",
    "\"id\":\"solution:scheduler-vertical\",",
    "\"summary\":\"Run the scheduler stage-product composition.\",",
    "\"approach\":[\"Dispatch each role through the Worker and verify its product.\"],",
    "\"components\":[{",
    "\"id\":\"component:scheduler\",",
    "\"label\":\"Scheduler\",",
    "\"responsibility\":\"Own the durable attempt and lease.\",",
    "\"kind\":\"component\",",
    "\"trustBoundary\":\"control-plane\",",
    "\"unresolved\":false,",
    "\"repositoryPathPrefixes\":[\"crates\"]",
    "}],",
    "\"connections\":[{",
    "\"id\":\"connection:scheduler-worker\",",
    "\"from\":\"component:scheduler\",",
    "\"to\":\"component:worker\",",
    "\"label\":\"dispatches\"",
    "}]",
    "},",
    "\"architectureDiagram\":{",
    "\"id\":\"diagram:scheduler-architecture\",",
    "\"kind\":\"system-architecture\",",
    "\"title\":\"Scheduler stage composition\",",
    "\"nodes\":[{",
    "\"id\":\"diagram:architecture:stage\",",
    "\"label\":\"Delivery stage\",",
    "\"description\":\"Runs one canonical stage product.\",",
    "\"kind\":\"stage\",",
    "\"trustBoundary\":null,",
    "\"unresolved\":false",
    "}],",
    "\"edges\":[]",
    "},",
    "\"processDiagram\":{",
    "\"id\":\"diagram:scheduler-process\",",
    "\"kind\":\"process-flow\",",
    "\"title\":\"Scheduler stage process\",",
    "\"nodes\":[{",
    "\"id\":\"diagram:process:stage\",",
    "\"label\":\"Dispatch and verify\",",
    "\"description\":\"Dispatches and verifies one attempt.\",",
    "\"kind\":\"stage\",",
    "\"trustBoundary\":null,",
    "\"unresolved\":false",
    "}],",
    "\"edges\":[]",
    "},",
    "\"risks\":[\"A replacement must fence its predecessor.\"],",
    "\"unresolvedItems\":[],",
    "\"taskProposals\":[{",
    "\"id\":\"task_scheduler_vertical\",",
    "\"title\":\"Run the stage composition\",",
    "\"goal\":\"Exercise the scheduler and Worker boundary\",",
    "\"acceptanceCriterionIds\":[\"criterion-scheduler-vertical\"],",
    "\"blockedByTaskIds\":[]",
    "}]",
    "}"
);

fn verification_json(candidate_ref: &str, role: &str) -> Vec<u8> {
    format!(
        "{{\"protocol\":\"winwincode.independent-verification-result.v1\",\"delivery_spec_id\":\"spec-scheduler-vertical\",\"delivery_spec_revision\":1,\"candidate_ref\":\"{candidate_ref}\",\"findings\":[{{\"finding_id\":\"finding-{role}-vertical\",\"criterion_id\":\"criterion-scheduler-vertical\",\"verdict\":\"pass\",\"explanation\":\"The observed stage evidence completed successfully.\",\"evidence_sources\":[{{\"type\":\"command\",\"event_id\":\"evidence-{role}\"}}]}}]}}"
    )
    .into_bytes()
}

fn products_for_job(job: &ExecutionJob) -> Result<Vec<SemanticEvent>, ()> {
    match job.execution_profile.as_str() {
        "planner" => {
            let product =
                prepare_planner_solution_activity(job, PLANNER_JSON.as_bytes()).map_err(|_| ())?;
            Ok(vec![semantic_from_prepared(
                product.category().clone(),
                product.media_type(),
                product.bytes(),
                product.summary(),
            )])
        }
        "executor" => {
            let bytes = br#"{"changedFiles":["scheduler-stage-change.txt"]}"#;
            Ok(vec![semantic_from_prepared(
                ExecutionEventCategory::Diff,
                "application/json",
                bytes,
                "executor produced the candidate diff",
            )])
        }
        "reviewer" | "verifier" => {
            let candidate_ref = job
                .stage_input
                .as_ref()
                .and_then(|input| input.candidate_ref.as_deref())
                .ok_or(())?;
            let policy =
                prepare_verification_policy_attestation(job, candidate_ref).map_err(|_| ())?;
            let evidence = prepare_verification_command_evidence(
                job,
                if job.execution_profile == "reviewer" {
                    VerificationEvidenceKind::Command
                } else {
                    VerificationEvidenceKind::Test
                },
                VerificationEvidenceStatus::Completed,
                0,
                &format!("evidence-{}", job.execution_profile),
            )
            .map_err(|_| ())?;
            let result = prepare_verification_result_activity(
                job,
                &verification_json(candidate_ref, &job.execution_profile),
            )
            .map_err(|_| ())?;
            Ok(vec![
                semantic_from_prepared(
                    policy.category().clone(),
                    policy.media_type(),
                    policy.bytes(),
                    policy.summary(),
                ),
                semantic_from_prepared(
                    evidence.category().clone(),
                    evidence.media_type(),
                    evidence.bytes(),
                    evidence.summary(),
                ),
                semantic_from_prepared(
                    result.category().clone(),
                    result.media_type(),
                    result.bytes(),
                    result.summary(),
                ),
            ])
        }
        _ => Err(()),
    }
}

fn runtime_message(run: &Run, event: &SemanticEvent, sequence: i64) -> RuntimeEventMessage {
    RuntimeEventMessage {
        codex_thread_id: run.thread_id.clone(),
        event: ExecutionEventRecord {
            category: event.category.clone(),
            event_id: ExecutionEventId(frame_id("evt")),
            occurred_at: at(4),
            payload: Some(EncodedPayload {
                content_type: event.media_type.clone(),
                data_base64: STANDARD.encode(&event.bytes),
                payload_digest: event.digest.clone(),
            }),
            sequence: ExecutionSequence(sequence),
            summary: event.summary.clone(),
        },
        kind: RuntimeEventMessageKind::RuntimeEvent,
        lease: run.lease.clone(),
        message_id: ExecutionMessageId(frame_id("xmsg")),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: at(4),
        session_identity: run.session_identity.clone(),
        worker_session_id: run.worker_session_id.clone(),
    }
}

impl CodexCoreAdapter for ScriptedStageProductAdapter {
    type Error = ();

    fn ensure_thread(
        &mut self,
        start: CodexThreadStart<'_>,
    ) -> impl Future<Output = Result<CodexThreadId, Self::Error>> + Send {
        let result = start
            .run_key
            .canonical_thread_id()
            .map_err(|_| ())
            .map(|thread_id| {
                let mut state = self.state.lock().expect("adapter state");
                state.runs.insert(
                    thread_id.0.clone(),
                    Run {
                        job: start.job.clone(),
                        lease: start.lease.clone(),
                        worker_session_id: start.worker_session_id.clone(),
                        session_identity: SessionIdentity {
                            codex_thread_id: thread_id.clone(),
                            product_session_id: match &start.job.scope {
                                ExecutionScope::DeliveryStageExecutionScope(scope) => {
                                    scope.product_session_id.clone()
                                }
                                ExecutionScope::ProductSessionExecutionScope(scope) => {
                                    scope.product_session_id.clone()
                                }
                            },
                            stage_run_id: match &start.job.scope {
                                ExecutionScope::DeliveryStageExecutionScope(scope) => {
                                    Some(scope.stage_run_id.clone())
                                }
                                ExecutionScope::ProductSessionExecutionScope(_) => None,
                            },
                            worker_session_id: start.worker_session_id.clone(),
                        },
                        thread_id: thread_id.clone(),
                        workspace: start.workspace.to_path_buf(),
                        polls: VecDeque::new(),
                        cancel_requested: false,
                    },
                );
                thread_id
            });
        std::future::ready(result)
    }

    fn submit_turn(
        &mut self,
        thread_id: &CodexThreadId,
        goal: &str,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let result = (|| {
            let run = self
                .state
                .lock()
                .expect("adapter state")
                .runs
                .get(thread_id.0.as_str())
                .cloned()
                .ok_or(())?;
            if stage_product_prompt(&run.job).map_err(|_| ())? != goal {
                return Err(());
            }
            if run.job.execution_profile == "executor" {
                std::fs::write(
                    run.workspace.join("scheduler-stage-change.txt"),
                    b"candidate change\n",
                )
                .map_err(|_| ())?;
            }
            let products = products_for_job(&run.job)?;
            let mut polls: VecDeque<Result<CodexPoll, ()>> = products
                .iter()
                .enumerate()
                .map(|(index, product)| {
                    Ok(CodexPoll::RuntimeTrace(Box::new(runtime_message(
                        &run,
                        product,
                        i64::try_from(index + 1).expect("stage sequence"),
                    ))))
                })
                .collect();
            polls.push_back(Ok(CodexPoll::Completed(CodexTurnCompletion {
                summary: secret_safe_runtime_summary("scripted stage product completed")
                    .map_err(|_| ())?,
                artifacts: Vec::new(),
                usage: winwincode_execution_port::generated::ExecutionOutcomeUsage {
                    runtime_millis: 17,
                    tokens: 23,
                    cost_microunits: 29,
                },
            })));
            self.state
                .lock()
                .expect("adapter state")
                .runs
                .get_mut(thread_id.0.as_str())
                .ok_or(())?
                .polls = polls;
            Ok(())
        })();
        std::future::ready(result)
    }

    fn poll(
        &mut self,
        thread_id: &CodexThreadId,
        _now: &Instant,
    ) -> impl Future<Output = Result<CodexPoll, Self::Error>> + Send {
        let result = self
            .state
            .lock()
            .expect("adapter state")
            .runs
            .get_mut(thread_id.0.as_str())
            .map(|run| {
                if run.cancel_requested {
                    run.cancel_requested = false;
                    run.polls.clear();
                    Ok(CodexPoll::Cancelled(
                        secret_safe_runtime_summary("scripted stage product cancelled")
                            .expect("cancel summary"),
                    ))
                } else {
                    run.polls.pop_front().unwrap_or(Ok(CodexPoll::Pending))
                }
            })
            .unwrap_or(Err(()));
        std::future::ready(result)
    }

    fn accept_model_chunk(
        &mut self,
        _chunk: &winwincode_execution_port::generated::ModelChunkMessage,
        _received_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        std::future::ready(Ok(()))
    }

    fn accept_action_receipt(
        &mut self,
        _receipt: &winwincode_execution_port::generated::ActionEnforcementReceiptMessage,
        _received_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        std::future::ready(Ok(()))
    }

    fn accept_approval_decision(
        &mut self,
        _decision: &winwincode_execution_port::generated::ApprovalDecisionMessage,
        _received_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        std::future::ready(Ok(()))
    }

    fn accept_input_response(
        &mut self,
        _response: &winwincode_execution_port::generated::InputResponseMessage,
        _received_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        std::future::ready(Ok(()))
    }

    fn retain_execution_delivery(
        &mut self,
        message: &ExecutionPortMessage,
    ) -> Result<DurableExecutionDelivery, Self::Error> {
        let value = serde_json::to_value(message).map_err(|_| ())?;
        let Some(delivery_id) = value["messageId"].as_str() else {
            eprintln!("scripted adapter could not find messageId in {value:?}");
            return Err(());
        };
        let delivery_id = delivery_id.to_owned();
        let delivery = DurableExecutionDelivery {
            delivery_id: delivery_id.clone(),
            message: message.clone(),
        };
        let mut state = self.state.lock().expect("adapter state");
        if let Some(existing) = state.deliveries.get(&delivery_id).cloned() {
            if existing.message != delivery.message {
                return Err(());
            }
            state.pending_delivery_ids.insert(delivery_id);
            return Ok(existing);
        }
        state.pending_delivery_ids.insert(delivery_id.clone());
        state.deliveries.insert(delivery_id, delivery.clone());
        Ok(delivery)
    }

    fn pending_execution_deliveries(
        &mut self,
    ) -> Result<Vec<DurableExecutionDelivery>, Self::Error> {
        let state = self.state.lock().expect("adapter state");
        let mut deliveries = state
            .pending_delivery_ids
            .iter()
            .filter_map(|id| state.deliveries.get(id).cloned())
            .collect::<Vec<_>>();
        deliveries.sort_by(|left, right| left.delivery_id.cmp(&right.delivery_id));
        Ok(deliveries)
    }

    fn record_execution_delivery_sent(&mut self, delivery_id: &str) -> Result<(), Self::Error> {
        let mut state = self.state.lock().expect("adapter state");
        state.pending_delivery_ids.remove(delivery_id);
        let transport_only = state.deliveries.get(delivery_id).is_some_and(|delivery| {
            matches!(
                delivery.message,
                ExecutionPortMessage::WorkerRegisterMessage(_)
                    | ExecutionPortMessage::JobDispatchResultMessage(_)
                    | ExecutionPortMessage::SessionBindingMessage(_)
                    | ExecutionPortMessage::JobCancelAckMessage(_)
                    | ExecutionPortMessage::WorkerHeartbeatMessage(_)
            )
        });
        if transport_only {
            state.deliveries.remove(delivery_id);
        }
        Ok(())
    }

    fn accept_execution_delivery_ack(
        &mut self,
        _acknowledgement: &ExecutionPortMessage,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    fn retain_candidate_artifact(
        &mut self,
        upload: &CandidateArtifactUpload,
    ) -> Result<RetainedCandidateArtifact, Self::Error> {
        let mut state = self.state.lock().expect("adapter state");
        if let Some(existing) = state.candidates.get(&upload.lease.job_id.0) {
            if existing.authority != upload.authority() {
                return Err(());
            }
            return Ok(RetainedCandidateArtifact {
                artifact: existing.artifact.clone(),
                authority: existing.authority.clone(),
                deliveries: Vec::new(),
                already_accepted: true,
            });
        }
        let artifact = ArtifactReference {
            artifact_id: ArtifactId(frame_id("art")),
            digest: upload.digest.clone(),
        };
        state.candidates.insert(
            upload.lease.job_id.0.clone(),
            CandidateRecord {
                authority: upload.authority(),
                artifact: artifact.clone(),
            },
        );
        Ok(RetainedCandidateArtifact {
            artifact,
            authority: upload.authority(),
            deliveries: Vec::new(),
            already_accepted: true,
        })
    }

    fn accept_candidate_artifact_ack(
        &mut self,
        _acknowledgement: &ArtifactAckMessage,
    ) -> Result<CandidateArtifactAckOutcome, Self::Error> {
        Err(())
    }

    fn accepted_candidate_artifact(
        &mut self,
        authority: &CandidateArtifactAuthority,
    ) -> Result<Option<ArtifactReference>, Self::Error> {
        let state = self.state.lock().expect("adapter state");
        let candidate = state.candidates.get(&authority.lease.job_id.0);
        if candidate.is_some_and(|candidate| candidate.authority != *authority) {
            return Err(());
        }
        Ok(candidate.map(|candidate| candidate.artifact.clone()))
    }

    fn cancel_candidate_artifact(
        &mut self,
        authority: &CandidateArtifactAuthority,
    ) -> Result<(), Self::Error> {
        let mut state = self.state.lock().expect("adapter state");
        if let Some(candidate) = state.candidates.get(&authority.lease.job_id.0)
            && candidate.authority != *authority
        {
            return Err(());
        }
        state.candidates.remove(&authority.lease.job_id.0);
        Ok(())
    }

    fn replay_execution_deliveries(
        &mut self,
        request: &winwincode_execution_port::generated::RuntimeReplayRequestMessage,
    ) -> Result<Vec<DurableExecutionDelivery>, Self::Error> {
        let state = self.state.lock().expect("adapter state");
        let mut deliveries = state
            .deliveries
            .values()
            .filter(|delivery| {
                matches!(
                    &delivery.message,
                    ExecutionPortMessage::RuntimeEventMessage(message)
                        if message.lease == request.lease
                            && message.event.sequence.0 > request.after_sequence.0
                            && message.session_identity == request.session_identity
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        deliveries.sort_by_key(|delivery| match &delivery.message {
            ExecutionPortMessage::RuntimeEventMessage(message) => message.event.sequence.0,
            _ => 0,
        });
        Ok(deliveries)
    }

    fn retain_job_outcome(
        &mut self,
        _thread_id: &CodexThreadId,
        outcome: &JobOutcomeMessage,
    ) -> Result<DurableExecutionDelivery, Self::Error> {
        self.retain_execution_delivery(&ExecutionPortMessage::JobOutcomeMessage(outcome.clone()))
    }

    fn take_execution_messages(&mut self) -> Result<Vec<ExecutionPortMessage>, Self::Error> {
        Ok(Vec::new())
    }

    fn interrupt(
        &mut self,
        thread_id: &CodexThreadId,
        _interrupted_at: &Instant,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let result = self
            .state
            .lock()
            .expect("adapter state")
            .runs
            .get_mut(thread_id.0.as_str())
            .map(|run| {
                run.cancel_requested = true;
                run.polls.clear();
            })
            .ok_or(());
        std::future::ready(result)
    }

    fn close_thread(
        &mut self,
        _thread_id: &CodexThreadId,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        std::future::ready(Ok(()))
    }

    fn shutdown(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        std::future::ready(Ok(()))
    }
}

fn worker_config(
    worker_id: WorkerId,
    instance: WorkerInstanceId,
    started_at: Instant,
) -> WorkerConfig {
    WorkerConfig {
        worker_id,
        worker_instance_id: instance,
        started_at,
        capabilities: WorkerCapabilitySet {
            capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            features: vec![
                WorkerCapabilityFeature::ArtifactStream,
                WorkerCapabilityFeature::Mcp,
                WorkerCapabilityFeature::Sandbox,
                WorkerCapabilityFeature::Shell,
            ],
            max_concurrent_jobs: 1,
            platform: WorkerCapabilitySetPlatform::Aarch64AppleDarwin,
        },
    }
}

fn stage_job(role: &str, seed: u64, revision: &str, candidate_ref: Option<&str>) -> ExecutionJob {
    let job_id = ExecutionJobId(id("job", 100 + seed));
    let goal = format!("Run the scheduler stage-product vertical for {role}.");
    let task_id = DeliveryTaskId(id("dtk", 100 + seed));
    let has_task = matches!(role, "executor" | "reviewer" | "verifier");
    let scope = ExecutionScope::DeliveryStageExecutionScope(DeliveryStageExecutionScope {
        delivery_id: queue_scope().delivery_id.expect("delivery scope"),
        delivery_task_id: has_task.then(|| task_id.clone()),
        kind: DeliveryStageExecutionScopeKind::DeliveryStage,
        product_session_id: queue_scope().product_session_id,
        rework_authorization: None,
        stage_run_id: StageRunId(id("run", 100 + seed)),
    });
    let stage_input = DeliveryStageInput {
        acceptance_criteria: vec![DeliveryStageAcceptanceCriterionInput {
            criterion_id: "criterion-scheduler-vertical".to_owned(),
            description: "Every role emits its exact stage product.".to_owned(),
            required: true,
            verification_method: Some("Inspect the canonical runtime product.".to_owned()),
        }],
        candidate_ref: candidate_ref.map(str::to_owned),
        constraints: vec!["Keep the repository boundary exact.".to_owned()],
        delivery_spec_id: "spec-scheduler-vertical".to_owned(),
        delivery_spec_revision: 1,
        goal: goal.clone(),
        out_of_scope: Vec::new(),
        schema_version: SchemaVersion::WinwincodeV1,
        scope: vec!["scheduler-to-stage-product".to_owned()],
        task: has_task.then(|| DeliveryStageTaskInput {
            acceptance_criterion_ids: vec!["criterion-scheduler-vertical".to_owned()],
            goal: goal.clone(),
            task_id,
            title: format!("Run {role}"),
        }),
        title: "Scheduler stage-product vertical".to_owned(),
    };
    let payload = format!("scheduler-stage-product-{seed}-{role}");
    ExecutionJob {
        attempt: 1,
        execution_profile: role.to_owned(),
        goal,
        job_id,
        limits: ExecutionLimits {
            deadline_at: at(55),
            max_artifact_bytes: 1_048_576,
            max_runtime_seconds: 300,
        },
        payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(payload.as_bytes()))),
        scope,
        stage_input: Some(stage_input),
        workspace: ExecutionWorkspace {
            checkout_revision: revision.to_owned(),
            repository_id: queue_scope().repository_id,
            write_mode: if role == "executor" {
                ExecutionWorkspaceWriteMode::Candidate
            } else {
                ExecutionWorkspaceWriteMode::ReadOnly
            },
        },
    }
}

fn configure_admission(storage: &mut SqliteStorage) {
    let scope = queue_scope();
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 8,
        max_queued: 16,
        token_budget: 100_000,
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
        ExecutionAdmissionBoundary::Delivery {
            organization_id: scope.organization_id.clone(),
            delivery_id: scope.delivery_id.clone().expect("delivery boundary"),
        },
        ExecutionAdmissionBoundary::ProductSession {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            product_session_id: scope.product_session_id.clone(),
        },
        ExecutionAdmissionBoundary::WorkerPool {
            organization_id: scope.organization_id,
            worker_pool_id: WorkerPoolId(id("wpl", 7)),
        },
    ];
    let mut admission = storage.execution_admission().expect("admission");
    for boundary in boundaries {
        admission
            .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
            .expect("admission policy");
    }
}

fn reserve_and_start(storage: &mut SqliteStorage, job: &ExecutionJob, seed: u64) {
    let access = if job.execution_profile == "executor" {
        ExecutionRepositoryAccess::IsolatedWrite {
            worktree_key: job.job_id.0.clone(),
        }
    } else {
        ExecutionRepositoryAccess::ReadOnly
    };
    let scope = queue_scope();
    storage
        .execution_admission()
        .expect("admission")
        .reserve(&ExecutionReservationRequest {
            scope: scope.clone(),
            user_id: UserId(id("usr", 8)),
            worker_pool_id: WorkerPoolId(id("wpl", 7)),
            job_id: job.job_id.clone(),
            request_id: RequestId(id("req", 20_000 + seed)),
            repository_access: access,
            reserved_tokens: 1_000,
            reserved_cost_microunits: 1_000,
            runtime_limit_millis: 30_000,
            submitted_at: at(5 + seed),
        })
        .expect("reserve admission");
    storage
        .execution_admission()
        .expect("admission")
        .start(&ExecutionReservationStart {
            scope,
            worker_pool_id: WorkerPoolId(id("wpl", 7)),
            job_id: job.job_id.clone(),
            request_id: RequestId(id("req", 21_000 + seed)),
            expected_revision: 1,
            started_at: at(6 + seed),
        })
        .expect("start admission");
}

fn settle_admission(storage: &mut SqliteStorage, job: &ExecutionJob, seed: u64, cancelled: bool) {
    let scope = queue_scope();
    let current = storage
        .execution_admission()
        .expect("admission")
        .load_reservation_by_job(&job.job_id)
        .expect("load admission")
        .expect("admission reservation");
    if cancelled {
        storage
            .execution_admission()
            .expect("admission")
            .release(&ExecutionReservationRelease {
                scope,
                worker_pool_id: WorkerPoolId(id("wpl", 7)),
                job_id: job.job_id.clone(),
                request_id: RequestId(id("req", 22_000 + seed)),
                expected_revision: current.revision,
                reason: ExecutionReservationReleaseReason::Cancelled,
                released_at: at(40 + (seed % 10)),
            })
            .expect("release admission");
    } else {
        storage
            .execution_admission()
            .expect("admission")
            .settle(&ExecutionReservationSettlement {
                scope,
                worker_pool_id: WorkerPoolId(id("wpl", 7)),
                job_id: job.job_id.clone(),
                request_id: RequestId(id("req", 23_000 + seed)),
                expected_revision: current.revision,
                actual_tokens: 23,
                actual_cost_microunits: 29,
                actual_runtime_millis: 17,
                completed_at: at(40 + (seed % 10)),
            })
            .expect("settle admission");
    }
}

fn submit_job(storage: &mut SqliteStorage, job: &ExecutionJob, seed: u64) {
    storage
        .execution_queue()
        .expect("queue")
        .submit(&ExecutionJobSubmission {
            scope: queue_scope(),
            job_id: job.job_id.clone(),
            request_id: RequestId(id("req", 24_000 + seed)),
            payload_digest: job.payload_digest.clone(),
            dispatch_payload: serde_json::to_vec(job).expect("canonical job"),
            attempt: 1,
            dependencies: Vec::new(),
            stage_run_id: Some(match &job.scope {
                ExecutionScope::DeliveryStageExecutionScope(scope) => scope.stage_run_id.clone(),
                ExecutionScope::ProductSessionExecutionScope(_) => unreachable!("stage job"),
            }),
            submitted_at: at(5 + seed),
        })
        .expect("submit job");
}

fn register_storage(
    storage: &mut SqliteStorage,
    register: &WorkerRegisterMessage,
    started_at: Instant,
) -> LeaseRecovery {
    let receipt = storage
        .execution_registry()
        .expect("registry")
        .register_worker(&WorkerRegistrationRequest {
            authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
                control_plane_principal: "scheduler-stage-product-vertical".to_owned(),
            },
            protocol_version: EXECUTION_PROTOCOL_VERSION.to_owned(),
            platform: WorkerPlatform::Aarch64AppleDarwin,
            capabilities: vec!["codex".to_owned()],
            capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            security_zone: "local".to_owned(),
            max_slots: 1,
            message_id: register.message_id.clone(),
            request_id: register.request_id.clone(),
            sent_at: register.sent_at.clone(),
            started_at,
            worker_id: register.worker_id.clone(),
            worker_instance_id: register.worker_instance_id.clone(),
        })
        .expect("register worker")
        .lease_recovery;
    storage
        .execution_registry()
        .expect("registry")
        .record_heartbeat(&WorkerHeartbeatRequest {
            active_leases: Vec::new(),
            available_slots: 1,
            heartbeat_sequence: ExecutionSequence(1),
            max_slots: 1,
            running_slots: 0,
            message_id: ExecutionMessageId(frame_id("xmsg")),
            observed_at: register.sent_at.clone(),
            sent_at: register.sent_at.clone(),
            worker_id: register.worker_id.clone(),
            worker_instance_id: register.worker_instance_id.clone(),
        })
        .expect("worker heartbeat");
    receipt
}

fn register_result(
    register: &WorkerRegisterMessage,
    lease_recovery: WorkerRegistrationResultMessageLeaseRecovery,
) -> WorkerRegistrationResultMessage {
    WorkerRegistrationResultMessage {
        error: None,
        heartbeat_interval_ms: 1_000,
        kind: WorkerRegistrationResultMessageKind::WorkerRegistrationResult,
        lease_recovery,
        message_id: ExecutionMessageId(frame_id("xmsg")),
        request_id: register.request_id.clone(),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: register.sent_at.clone(),
        server_time: register.sent_at.clone(),
        status: WorkerRegistrationResultMessageStatus::Accepted,
        worker_id: register.worker_id.clone(),
        worker_instance_id: register.worker_instance_id.clone(),
    }
}

async fn start_worker(
    storage: &mut SqliteStorage,
    config: WorkerConfig,
    runtime: JobWorkspaceRuntime,
    adapter: ScriptedStageProductAdapter,
    recovery: WorkerRegistrationResultMessageLeaseRecovery,
) -> (
    WorkerMain<RecordingPort, ScriptedStageProductAdapter>,
    RecordingPort,
    WorkerRegisterMessage,
) {
    let port = RecordingPort::default();
    let port_view = port.clone();
    let instance = config.worker_instance_id.clone();
    let started_at = config.started_at.clone();
    let mut worker = WorkerMain::new(config, port, adapter, runtime)
        .with_registration_request_namespace(&instance, &started_at);
    worker
        .start(started_at.clone())
        .await
        .expect("start worker");
    let register = port_view
        .messages()
        .into_iter()
        .find_map(|message| match message {
            ExecutionPortMessage::WorkerRegisterMessage(register) => Some(register),
            _ => None,
        })
        .expect("worker register message");
    let stored_recovery = register_storage(storage, &register, started_at);
    let expected_recovery = match recovery {
        WorkerRegistrationResultMessageLeaseRecovery::NoActiveLeases => {
            LeaseRecovery::NoActiveLeases
        }
        WorkerRegistrationResultMessageLeaseRecovery::ReacquireRequired => {
            LeaseRecovery::ReacquireRequired
        }
    };
    assert_eq!(stored_recovery, expected_recovery);
    worker
        .accept_control(
            &ExecutionPortMessage::WorkerRegistrationResultMessage(register_result(
                &register, recovery,
            )),
            at(2),
        )
        .await
        .expect("accept worker registration");
    (worker, port_view, register)
}

fn dispatch_request(
    seed: u64,
    worker_id: WorkerId,
    instance: WorkerInstanceId,
) -> RepositorySchedulerClaimRequest {
    RepositorySchedulerClaimRequest {
        scope: scheduler_scope(),
        request_id: RequestId(id("req", 30_000 + seed)),
        scheduler_generation: format!("scheduler-stage-vertical-{seed}"),
        worker_id,
        worker_instance_id: instance,
        issued_at: at(8 + seed),
        expires_at: at(50),
    }
}

fn replacement_request(
    seed: u64,
    worker_id: WorkerId,
    instance: WorkerInstanceId,
) -> RepositorySchedulerClaimRequest {
    RepositorySchedulerClaimRequest {
        scope: scheduler_scope(),
        request_id: RequestId(id("req", 30_000 + seed)),
        scheduler_generation: format!("scheduler-stage-vertical-replacement-{seed}"),
        worker_id,
        worker_instance_id: instance,
        issued_at: at(50),
        expires_at: at(59),
    }
}

fn output_since<T>(
    messages: &[ExecutionPortMessage],
    select: impl Fn(&ExecutionPortMessage) -> Option<T>,
) -> Vec<T> {
    messages.iter().filter_map(select).collect()
}

fn binding_for(messages: &[ExecutionPortMessage]) -> SessionBindingMessage {
    output_since(messages, |message| match message {
        ExecutionPortMessage::SessionBindingMessage(binding) => Some(binding.clone()),
        _ => None,
    })
    .into_iter()
    .next()
    .expect("one session binding")
}

fn dispatch_result_for(messages: &[ExecutionPortMessage]) -> JobDispatchResultMessage {
    output_since(messages, |message| match message {
        ExecutionPortMessage::JobDispatchResultMessage(result) => Some(result.clone()),
        _ => None,
    })
    .into_iter()
    .next()
    .expect("one dispatch result")
}

fn outcome_for(messages: &[ExecutionPortMessage], job_id: &ExecutionJobId) -> JobOutcomeMessage {
    output_since(messages, |message| match message {
        ExecutionPortMessage::JobOutcomeMessage(outcome) if outcome.lease.job_id == *job_id => {
            Some(outcome.clone())
        }
        _ => None,
    })
    .into_iter()
    .last()
    .expect("one job outcome")
}

fn runtime_for(
    messages: &[ExecutionPortMessage],
    job_id: &ExecutionJobId,
) -> Vec<RuntimeEventMessage> {
    output_since(messages, |message| match message {
        ExecutionPortMessage::RuntimeEventMessage(runtime) if runtime.lease.job_id == *job_id => {
            Some(runtime.clone())
        }
        _ => None,
    })
}

fn slot_authority(binding: &SessionBindingMessage) -> WorkerSlotAuthority {
    WorkerSlotAuthority {
        worker_id: binding.worker_id.clone(),
        worker_instance_id: binding.lease.worker_instance_id.clone(),
        worker_session_id: binding.worker_session_id.clone(),
        codex_thread_id: binding.codex_thread_id.clone(),
        job_id: binding.lease.job_id.clone(),
        lease_id: binding.lease.lease_id.clone(),
        attempt: u64::try_from(binding.attempt).expect("slot attempt"),
        fencing_token: binding.fencing_token.clone(),
    }
}

fn open_slot(
    storage: &mut SqliteStorage,
    binding: &SessionBindingMessage,
    seed: u64,
) -> WorkerSlotAuthority {
    let authority = slot_authority(binding);
    let mut slots = storage.worker_session_slots().expect("slots");
    slots
        .configure_resources(
            &authority.worker_id,
            &authority.worker_instance_id,
            WorkerSlotResourceLimits {
                max_memory_bytes: 100,
                max_disk_bytes: 100,
                max_processes: 1,
            },
        )
        .expect("configure slot resources");
    slots
        .open(&WorkerSlotOpenRequest {
            authority: authority.clone(),
            resources: WorkerSlotResources {
                memory_bytes: 10,
                disk_bytes: 10,
                process_slots: 1,
            },
            request_id: RequestId(id("req", 31_000 + seed)),
            opened_at: at(if authority.attempt == 1 {
                10 + seed
            } else {
                51
            }),
        })
        .expect("open WorkerSession slot");
    authority
}

fn close_slot(
    storage: &mut SqliteStorage,
    authority: &WorkerSlotAuthority,
    seed: u64,
    cancelled: bool,
) {
    let mut slots = storage.worker_session_slots().expect("slots");
    let current = slots
        .load(&authority.worker_session_id)
        .expect("load slot")
        .expect("slot");
    slots
        .close(&WorkerSlotCloseRequest {
            authority: authority.clone(),
            request_id: RequestId(id("req", 32_000 + seed)),
            expected_revision: current.revision,
            outcome: if cancelled {
                WorkerSlotState::Cancelled
            } else {
                WorkerSlotState::Completed
            },
            closed_at: at(if authority.attempt == 1 {
                40 + seed
            } else {
                56
            }),
        })
        .expect("close WorkerSession slot");
}

fn durable_snapshot(
    storage: &mut SqliteStorage,
    job: &ExecutionJob,
    session: Option<&WorkerSessionId>,
) -> serde_json::Value {
    let queue = storage
        .execution_queue()
        .expect("queue")
        .load_job(&queue_scope(), &job.job_id)
        .expect("load queue job");
    let lease = storage
        .execution_registry()
        .expect("registry")
        .load_lease(&job.job_id)
        .expect("load lease");
    let slot = session.map(|session| {
        storage
            .worker_session_slots()
            .expect("slots")
            .load(session)
            .expect("load slot")
    });
    serde_json::json!({
        "queue": queue,
        "lease": lease,
        "slot": slot,
    })
}

async fn run_worker_until_outcome(
    worker: &mut WorkerMain<RecordingPort, ScriptedStageProductAdapter>,
    port: &RecordingPort,
    job_id: &ExecutionJobId,
    now: Instant,
) -> JobOutcomeMessage {
    for _ in 0..32 {
        worker
            .poll_codex(now.clone())
            .await
            .expect("poll scripted stage");
        let messages = port.messages();
        if messages.iter().any(|message| {
            matches!(message, ExecutionPortMessage::JobOutcomeMessage(outcome) if outcome.lease.job_id == *job_id)
        }) {
            return outcome_for(&messages, job_id);
        }
    }
    panic!(
        "stage Worker did not produce terminal outcome for {}",
        job_id.0
    );
}

fn terminal_request(
    _job: &ExecutionJob,
    outcome: &JobOutcomeMessage,
    seed: u64,
) -> RepositorySchedulerTerminalRequest {
    RepositorySchedulerTerminalRequest {
        scope: scheduler_scope(),
        terminal: ExecutionLeaseTerminalRequest {
            job_id: outcome.lease.job_id.clone(),
            lease_id: outcome.lease.lease_id.clone(),
            worker_id: outcome.lease.worker_id.clone(),
            worker_instance_id: outcome.lease.worker_instance_id.clone(),
            attempt: u64::try_from(outcome.lease.attempt).expect("terminal attempt"),
            fencing_token: outcome.lease.fencing_token.clone(),
            outcome: if outcome.outcome.status == ExecutionOutcomeStatus::Cancelled {
                ExecutionLeaseTerminalOutcome::Cancelled
            } else if outcome.outcome.status == ExecutionOutcomeStatus::Succeeded {
                ExecutionLeaseTerminalOutcome::Completed
            } else {
                ExecutionLeaseTerminalOutcome::Failed
            },
            terminal_at: outcome.outcome.finished_at.clone(),
            request_id: RequestId(id("req", 33_000 + seed)),
        },
    }
}

#[tokio::test(flavor = "current_thread")]
async fn scheduler_stage_product_roles_cancel_restart_and_old_attempt_are_exact() {
    let fixture = Fixture::new("roles");
    let mut storage = SqliteStorage::open(fixture.data()).expect("storage");
    configure_admission(&mut storage);
    let worker_id = WorkerId(id("wrk", 10));
    let instance = WorkerInstanceId(id("wki", 11));
    let adapter = ScriptedStageProductAdapter::default();
    let config = worker_config(worker_id.clone(), instance.clone(), at(0));
    let (mut worker, port, _) = start_worker(
        &mut storage,
        config,
        fixture.workspace_runtime(),
        adapter.clone(),
        WorkerRegistrationResultMessageLeaseRecovery::NoActiveLeases,
    )
    .await;

    let mut candidate_ref = None;
    for (role, seed) in [
        ("planner", 1),
        ("executor", 2),
        ("reviewer", 3),
        ("verifier", 4),
    ] {
        let job = stage_job(role, seed, &fixture.revision, candidate_ref.as_deref());
        submit_job(&mut storage, &job, seed);
        reserve_and_start(&mut storage, &job, seed);
        let dispatch = RepositoryExecutionScheduler::new(&mut storage)
            .claim_next(&dispatch_request(seed, worker_id.clone(), instance.clone()))
            .expect("claim stage")
            .expect("stage dispatch");
        let cursor = port.len();
        worker
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
                at(9 + seed),
            )
            .await
            .expect("accept stage dispatch");
        let messages = port.since(cursor);
        let result = dispatch_result_for(&messages);
        let binding = binding_for(&messages);
        assert_eq!(result.status, JobDispatchResultMessageStatus::Accepted);
        assert_eq!(result.job_id, job.job_id);
        assert_eq!(result.lease, dispatch.lease);
        assert_eq!(result.payload_digest, job.payload_digest);
        assert_eq!(
            result.worker_session_id,
            Some(binding.worker_session_id.clone())
        );
        assert_eq!(binding.attempt, dispatch.job.attempt);
        assert_eq!(binding.lease, dispatch.lease);
        assert_eq!(binding.product_session_id, queue_scope().product_session_id);
        assert_eq!(
            binding.stage_run_id,
            Some(match &job.scope {
                ExecutionScope::DeliveryStageExecutionScope(scope) => scope.stage_run_id.clone(),
                _ => unreachable!("delivery stage"),
            })
        );
        assert_eq!(
            binding.session_identity.codex_thread_id,
            binding.codex_thread_id
        );
        assert_eq!(
            binding.session_identity.worker_session_id,
            binding.worker_session_id
        );
        assert_eq!(binding.source_identity.worker_id, worker_id);
        assert_eq!(binding.source_identity.worker_instance_id, instance);
        let running = RepositoryExecutionScheduler::new(&mut storage)
            .record_dispatch_result(&repository_scope(), &result, &at(10 + seed))
            .expect("record accepted dispatch");
        assert!(running.accepted);
        assert_eq!(running.job.state, ExecutionJobState::Running);
        let authority = open_slot(&mut storage, &binding, seed);
        let outcome =
            run_worker_until_outcome(&mut worker, &port, &job.job_id, at(12 + seed)).await;
        assert_eq!(outcome.lease, dispatch.lease);
        assert_eq!(outcome.session_identity, binding.session_identity);
        assert_eq!(outcome.worker_session_id, binding.worker_session_id);
        assert_eq!(
            outcome.outcome.codex_thread_id,
            Some(binding.codex_thread_id.clone())
        );
        assert_eq!(outcome.outcome.status, ExecutionOutcomeStatus::Succeeded);
        let runtime = runtime_for(&port.messages(), &job.job_id);
        assert!(!runtime.is_empty(), "{role} emitted no runtime product");
        assert!(runtime.iter().all(|message| {
            message.lease == dispatch.lease
                && message.session_identity == binding.session_identity
                && message.worker_session_id == binding.worker_session_id
        }));
        match role {
            "planner" => assert_eq!(runtime[0].event.category, ExecutionEventCategory::Activity),
            "executor" => {
                assert_eq!(runtime[0].event.category, ExecutionEventCategory::Diff);
                assert_eq!(outcome.outcome.artifacts.len(), 1);
                candidate_ref = adapter.candidate_ref();
                assert!(
                    candidate_ref.is_some(),
                    "executor candidate was not retained"
                );
            }
            "reviewer" => {
                assert_eq!(runtime[0].event.category, ExecutionEventCategory::Lifecycle);
                assert_eq!(runtime[1].event.category, ExecutionEventCategory::Command);
                assert_eq!(runtime[2].event.category, ExecutionEventCategory::Activity);
            }
            "verifier" => {
                assert_eq!(runtime[0].event.category, ExecutionEventCategory::Lifecycle);
                assert_eq!(runtime[1].event.category, ExecutionEventCategory::Test);
                assert_eq!(runtime[2].event.category, ExecutionEventCategory::Activity);
            }
            _ => unreachable!("role fixture"),
        }
        close_slot(&mut storage, &authority, seed, false);
        let settled = RepositoryExecutionScheduler::new(&mut storage)
            .settle_terminal(&terminal_request(&job, &outcome, seed))
            .expect("settle stage");
        assert_eq!(settled.job.state, ExecutionJobState::Completed);
        let replay = RepositoryExecutionScheduler::new(&mut storage)
            .settle_terminal(&terminal_request(&job, &outcome, seed))
            .expect("replay stage settlement");
        assert!(replay.replayed);
        settle_admission(&mut storage, &job, seed, false);
        let duplicate_cursor = port.len();
        worker
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
                at(20 + seed),
            )
            .await
            .expect("replay dispatch");
        let duplicate_messages = port.since(duplicate_cursor);
        assert_eq!(
            output_since(&duplicate_messages, |message| match message {
                ExecutionPortMessage::JobDispatchResultMessage(result) => Some(result.clone()),
                _ => None,
            })
            .len(),
            1
        );
        assert!(
            output_since(&duplicate_messages, |message| match message {
                ExecutionPortMessage::SessionBindingMessage(_) => Some(()),
                _ => None,
            })
            .is_empty()
        );
    }

    let cancel_job = stage_job("planner", 5, &fixture.revision, None);
    submit_job(&mut storage, &cancel_job, 5);
    reserve_and_start(&mut storage, &cancel_job, 5);
    let cancel_dispatch = RepositoryExecutionScheduler::new(&mut storage)
        .claim_next(&dispatch_request(5, worker_id.clone(), instance.clone()))
        .expect("claim cancel")
        .expect("cancel dispatch");
    let cancel_cursor = port.len();
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(cancel_dispatch.clone()),
            at(14),
        )
        .await
        .expect("accept cancel dispatch");
    let cancel_messages = port.since(cancel_cursor);
    let cancel_result = dispatch_result_for(&cancel_messages);
    let cancel_binding = binding_for(&cancel_messages);
    RepositoryExecutionScheduler::new(&mut storage)
        .record_dispatch_result(&repository_scope(), &cancel_result, &at(15))
        .expect("record cancel dispatch");
    let cancel_authority = open_slot(&mut storage, &cancel_binding, 5);
    let current = storage
        .execution_queue()
        .expect("queue")
        .load_job(&queue_scope(), &cancel_job.job_id)
        .expect("load cancelling job")
        .expect("cancelling job");
    let cancel = RepositoryExecutionScheduler::new(&mut storage)
        .request_cancellation(&RepositorySchedulerCancellationRequest {
            scope: scheduler_scope(),
            job_id: cancel_job.job_id.clone(),
            request_id: RequestId(id("req", 34_005)),
            expected_revision: current.revision,
            requested_at: at(16),
        })
        .expect("request cancellation")
        .expect("typed cancellation");
    assert_eq!(cancel.worker_session_id, cancel_binding.worker_session_id);
    assert_eq!(
        cancel.session_identity.codex_thread_id,
        cancel_binding.codex_thread_id
    );
    storage
        .worker_session_slots()
        .expect("slots")
        .request_cancellation(&WorkerSlotCancellation {
            authority: cancel_authority.clone(),
            request_id: RequestId(id("req", 34_105)),
            expected_revision: 1,
            requested_at: at(16),
        })
        .expect("persist slot cancellation");
    let cancel_ack_cursor = port.len();
    worker
        .accept_control(
            &ExecutionPortMessage::JobCancelMessage(cancel.clone()),
            at(17),
        )
        .await
        .expect("accept cancellation");
    let cancel_ack_messages = port.since(cancel_ack_cursor);
    assert!(
        output_since(&cancel_ack_messages, |message| match message {
            ExecutionPortMessage::JobCancelAckMessage(ack)
                if ack.status == JobCancelAckMessageStatus::Accepted =>
                Some(()),
            _ => None,
        })
        .len()
            == 1
    );
    let cancelled = run_worker_until_outcome(&mut worker, &port, &cancel_job.job_id, at(18)).await;
    assert_eq!(cancelled.outcome.status, ExecutionOutcomeStatus::Cancelled);
    assert!(runtime_for(&port.messages(), &cancel_job.job_id).is_empty());
    close_slot(&mut storage, &cancel_authority, 5, true);
    let cancel_settled = RepositoryExecutionScheduler::new(&mut storage)
        .settle_terminal(&terminal_request(&cancel_job, &cancelled, 5))
        .expect("settle cancellation");
    assert_eq!(cancel_settled.job.state, ExecutionJobState::Failed);
    settle_admission(&mut storage, &cancel_job, 5, true);

    let restart_job = stage_job("planner", 6, &fixture.revision, None);
    submit_job(&mut storage, &restart_job, 6);
    reserve_and_start(&mut storage, &restart_job, 6);
    let old_dispatch = RepositoryExecutionScheduler::new(&mut storage)
        .claim_next(&dispatch_request(6, worker_id.clone(), instance.clone()))
        .expect("claim old restart")
        .expect("old restart dispatch");
    let old_cursor = port.len();
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(old_dispatch.clone()),
            at(20),
        )
        .await
        .expect("accept old restart dispatch");
    let old_messages = port.since(old_cursor);
    let old_result = dispatch_result_for(&old_messages);
    let old_binding = binding_for(&old_messages);
    RepositoryExecutionScheduler::new(&mut storage)
        .record_dispatch_result(&repository_scope(), &old_result, &at(21))
        .expect("record old restart dispatch");
    let old_authority = open_slot(&mut storage, &old_binding, 6);
    let before_replacement = durable_snapshot(
        &mut storage,
        &restart_job,
        Some(&old_binding.worker_session_id),
    );
    drop(worker);

    let replacement_instance = WorkerInstanceId(id("wki", 12));
    let replacement_config = worker_config(worker_id.clone(), replacement_instance.clone(), at(22));
    let (mut replacement_worker, replacement_port, _) = start_worker(
        &mut storage,
        replacement_config,
        fixture.workspace_runtime(),
        ScriptedStageProductAdapter::default(),
        WorkerRegistrationResultMessageLeaseRecovery::ReacquireRequired,
    )
    .await;
    let replacement_dispatch = RepositoryExecutionScheduler::new(&mut storage)
        .claim_next(&replacement_request(
            7,
            worker_id.clone(),
            replacement_instance.clone(),
        ))
        .expect("claim replacement")
        .expect("replacement dispatch");
    assert_eq!(replacement_dispatch.job.attempt, 2);
    assert_eq!(replacement_dispatch.lease.attempt, 2);
    let replacement_authority = replacement_dispatch
        .replacement_authority
        .as_ref()
        .expect("replacement authority");
    assert_eq!(replacement_authority.predecessor_lease, old_dispatch.lease);
    assert_eq!(
        replacement_authority.successor_lease,
        replacement_dispatch.lease
    );
    assert_eq!(
        replacement_authority.predecessor_session_identity.as_ref(),
        Some(&old_binding.session_identity)
    );
    let replacement_cursor = replacement_port.len();
    replacement_worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(replacement_dispatch.clone()),
            at(51),
        )
        .await
        .expect("accept replacement dispatch");
    let replacement_messages = replacement_port.since(replacement_cursor);
    let replacement_result = dispatch_result_for(&replacement_messages);
    let replacement_binding = binding_for(&replacement_messages);
    assert_eq!(
        replacement_result.status,
        JobDispatchResultMessageStatus::Accepted
    );
    assert_eq!(replacement_result.lease, replacement_dispatch.lease);
    assert_eq!(replacement_binding.lease, replacement_dispatch.lease);
    assert_ne!(
        replacement_binding.worker_session_id,
        old_binding.worker_session_id
    );
    assert_ne!(
        replacement_binding.codex_thread_id,
        old_binding.codex_thread_id
    );
    assert_eq!(
        replacement_binding.product_session_id,
        old_binding.product_session_id
    );
    assert_eq!(replacement_binding.stage_run_id, old_binding.stage_run_id);
    assert_eq!(
        replacement_binding.session_identity.worker_session_id,
        replacement_binding.worker_session_id
    );
    let replacement_running = RepositoryExecutionScheduler::new(&mut storage)
        .record_dispatch_result(&repository_scope(), &replacement_result, &at(52))
        .expect("record replacement dispatch");
    assert!(replacement_running.accepted);
    let replacement_slot = open_slot(&mut storage, &replacement_binding, 7);
    let replacement_outcome = run_worker_until_outcome(
        &mut replacement_worker,
        &replacement_port,
        &restart_job.job_id,
        at(53),
    )
    .await;
    assert_eq!(replacement_outcome.lease, replacement_dispatch.lease);
    assert_eq!(
        replacement_outcome.session_identity,
        replacement_binding.session_identity
    );
    assert_eq!(
        replacement_outcome.outcome.status,
        ExecutionOutcomeStatus::Succeeded
    );
    assert!(
        runtime_for(&replacement_port.messages(), &restart_job.job_id)
            .iter()
            .all(|runtime| runtime.lease == replacement_dispatch.lease)
    );
    close_slot(&mut storage, &replacement_slot, 7, false);
    RepositoryExecutionScheduler::new(&mut storage)
        .settle_terminal(&terminal_request(&restart_job, &replacement_outcome, 7))
        .expect("settle replacement");
    settle_admission(&mut storage, &restart_job, 6, false);

    let after_replacement = durable_snapshot(
        &mut storage,
        &restart_job,
        Some(&replacement_binding.worker_session_id),
    );
    assert_ne!(before_replacement, after_replacement);
    let stale_terminal = RepositorySchedulerTerminalRequest {
        scope: scheduler_scope(),
        terminal: ExecutionLeaseTerminalRequest {
            job_id: old_dispatch.lease.job_id.clone(),
            lease_id: old_dispatch.lease.lease_id.clone(),
            worker_id: old_dispatch.lease.worker_id.clone(),
            worker_instance_id: old_dispatch.lease.worker_instance_id.clone(),
            attempt: 1,
            fencing_token: old_dispatch.lease.fencing_token.clone(),
            outcome: ExecutionLeaseTerminalOutcome::Failed,
            terminal_at: at(27),
            request_id: RequestId(id("req", 35_006)),
        },
    };
    assert!(
        RepositoryExecutionScheduler::new(&mut storage)
            .settle_terminal(&stale_terminal)
            .is_err()
    );
    let after_stale_terminal = durable_snapshot(
        &mut storage,
        &restart_job,
        Some(&replacement_binding.worker_session_id),
    );
    assert_eq!(after_replacement, after_stale_terminal);
    assert!(
        storage
            .worker_session_slots()
            .expect("slots")
            .load(&old_authority.worker_session_id)
            .expect("old slot")
            .is_some_and(|slot| matches!(slot.state, WorkerSlotState::RecoveryFailed))
    );
    assert!(before_replacement["queue"] != serde_json::Value::Null);
}

#[tokio::test(flavor = "current_thread")]
async fn accepted_dispatch_without_predecessor_slot_recovers_as_fresh_attempt() {
    let fixture = Fixture::new("no-slot");
    let mut storage = SqliteStorage::open(fixture.data()).expect("storage");
    configure_admission(&mut storage);
    let worker_id = WorkerId(id("wrk", 40));
    let old_instance = WorkerInstanceId(id("wki", 41));
    let adapter = ScriptedStageProductAdapter::default();
    let (mut old_worker, old_port, _) = start_worker(
        &mut storage,
        worker_config(worker_id.clone(), old_instance.clone(), at(0)),
        fixture.workspace_runtime(),
        adapter.clone(),
        WorkerRegistrationResultMessageLeaseRecovery::NoActiveLeases,
    )
    .await;
    let job = stage_job("planner", 10, &fixture.revision, None);
    submit_job(&mut storage, &job, 10);
    reserve_and_start(&mut storage, &job, 10);
    let old_dispatch = RepositoryExecutionScheduler::new(&mut storage)
        .claim_next(&dispatch_request(
            10,
            worker_id.clone(),
            old_instance.clone(),
        ))
        .expect("claim no-slot old")
        .expect("no-slot old dispatch");
    let old_cursor = old_port.len();
    old_worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(old_dispatch.clone()),
            at(18),
        )
        .await
        .expect("accept no-slot old");
    let old_messages = old_port.since(old_cursor);
    let old_result = dispatch_result_for(&old_messages);
    let old_binding = binding_for(&old_messages);
    RepositoryExecutionScheduler::new(&mut storage)
        .record_dispatch_result(&repository_scope(), &old_result, &at(19))
        .expect("record no-slot old");
    drop(old_worker);

    let new_instance = WorkerInstanceId(id("wki", 42));
    let (mut new_worker, new_port, _) = start_worker(
        &mut storage,
        worker_config(worker_id.clone(), new_instance.clone(), at(20)),
        fixture.workspace_runtime(),
        adapter,
        WorkerRegistrationResultMessageLeaseRecovery::ReacquireRequired,
    )
    .await;
    let replacement = RepositoryExecutionScheduler::new(&mut storage)
        .claim_next(&replacement_request(
            11,
            worker_id.clone(),
            new_instance.clone(),
        ))
        .expect("claim no-slot replacement")
        .expect("no-slot replacement");
    let authority = replacement
        .replacement_authority
        .as_ref()
        .expect("replacement authority");
    assert!(authority.predecessor_session_identity.is_none());
    assert_eq!(replacement.job.attempt, 2);
    let cursor = new_port.len();
    new_worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(replacement.clone()),
            at(51),
        )
        .await
        .expect("accept no-slot replacement");
    let messages = new_port.since(cursor);
    let result = dispatch_result_for(&messages);
    let binding = binding_for(&messages);
    assert_eq!(result.status, JobDispatchResultMessageStatus::Accepted);
    assert_eq!(result.lease, replacement.lease);
    assert_ne!(binding.worker_session_id, old_binding.worker_session_id);
    assert_ne!(binding.codex_thread_id, old_binding.codex_thread_id);
    let replacement_running = RepositoryExecutionScheduler::new(&mut storage)
        .record_dispatch_result(&repository_scope(), &result, &at(52))
        .expect("record no-slot replacement");
    assert!(
        replacement_running.accepted,
        "replacement dispatch receipt: {:?}",
        replacement_running
    );
    assert_eq!(replacement_running.job.state, ExecutionJobState::Running);
    let outcome = run_worker_until_outcome(&mut new_worker, &new_port, &job.job_id, at(53)).await;
    assert_eq!(outcome.lease.attempt, 2);
    assert_eq!(outcome.outcome.status, ExecutionOutcomeStatus::Succeeded);
    assert!(
        runtime_for(&new_port.messages(), &job.job_id)
            .iter()
            .all(|runtime| runtime.lease.attempt == 2)
    );
    RepositoryExecutionScheduler::new(&mut storage)
        .settle_terminal(&terminal_request(&job, &outcome, 10))
        .expect("settle no-slot replacement");
    settle_admission(&mut storage, &job, 10, false);
}

#[tokio::test(flavor = "current_thread")]
async fn failed_stage_dispatch_retries_as_attempt_two_and_reaches_product() {
    let fixture = Fixture::new("retry");
    let mut storage = SqliteStorage::open(fixture.data()).expect("storage");
    configure_admission(&mut storage);
    let worker_id = WorkerId(id("wrk", 50));
    let instance = WorkerInstanceId(id("wki", 51));
    let adapter = ScriptedStageProductAdapter::default();
    let (failed_worker, _failed_port, _) = start_worker(
        &mut storage,
        worker_config(worker_id.clone(), instance.clone(), at(0)),
        fixture.workspace_runtime(),
        adapter.clone(),
        WorkerRegistrationResultMessageLeaseRecovery::NoActiveLeases,
    )
    .await;
    let job = stage_job("planner", 20, &fixture.revision, None);
    submit_job(&mut storage, &job, 20);
    reserve_and_start(&mut storage, &job, 20);
    let first = RepositoryExecutionScheduler::new(&mut storage)
        .claim_next(&dispatch_request(20, worker_id.clone(), instance.clone()))
        .expect("claim failed attempt")
        .expect("failed attempt dispatch");
    let failed = JobDispatchResultMessage {
        error: None,
        job_id: first.job.job_id.clone(),
        kind: winwincode_execution_port::generated::JobDispatchResultMessageKind::JobDispatchResult,
        lease: first.lease.clone(),
        message_id: ExecutionMessageId(frame_id("xmsg")),
        payload_digest: first.job.payload_digest.clone(),
        request_id: first.request_id.clone(),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: at(30),
        status: JobDispatchResultMessageStatus::RejectedCapability,
        worker_session_id: None,
    };
    let failed_receipt = RepositoryExecutionScheduler::new(&mut storage)
        .record_dispatch_result(&repository_scope(), &failed, &at(30))
        .expect("record failed dispatch");
    assert_eq!(failed_receipt.job.state, ExecutionJobState::Failed);
    let retry_instance = WorkerInstanceId(id("wki", 52));
    let (mut worker, port, _) = start_worker(
        &mut storage,
        worker_config(worker_id.clone(), retry_instance.clone(), at(31)),
        fixture.workspace_runtime(),
        adapter,
        WorkerRegistrationResultMessageLeaseRecovery::NoActiveLeases,
    )
    .await;
    drop(failed_worker);
    let retry_request = RepositorySchedulerRetryRequest {
        scope: scheduler_scope(),
        job_id: job.job_id.clone(),
        request_id: RequestId(id("req", 36_020)),
        scheduler_generation: "scheduler-stage-vertical-retry".to_owned(),
        worker_id: worker_id.clone(),
        worker_instance_id: retry_instance,
        retryable_failure: true,
        failed_at_tick: 100,
        now_tick: 105,
        policy: SchedulerRetryPolicy {
            max_attempts: 3,
            initial_backoff_ticks: 5,
            max_backoff_ticks: 20,
        },
        issued_at: at(31),
        expires_at: at(59),
    };
    let retry = RepositoryExecutionScheduler::new(&mut storage)
        .retry_failed(&retry_request)
        .expect("retry failed stage")
        .expect("retry dispatch");
    assert_eq!(retry.job.attempt, 2);
    assert_eq!(retry.lease.attempt, 2);
    assert!(
        retry
            .replacement_authority
            .as_ref()
            .is_some_and(|authority| {
                authority.predecessor_session_identity.is_none()
                    && authority.predecessor_lease == first.lease
                    && authority.successor_lease == retry.lease
            })
    );
    let cursor = port.len();
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(retry.clone()),
            at(32),
        )
        .await
        .expect("accept retry dispatch");
    let messages = port.since(cursor);
    let result = dispatch_result_for(&messages);
    let binding = binding_for(&messages);
    assert_eq!(result.status, JobDispatchResultMessageStatus::Accepted);
    assert_eq!(result.lease, retry.lease);
    assert_eq!(binding.attempt, 2);
    RepositoryExecutionScheduler::new(&mut storage)
        .record_dispatch_result(&repository_scope(), &result, &at(33))
        .expect("record retry dispatch");
    let authority = open_slot(&mut storage, &binding, 20);
    let outcome = run_worker_until_outcome(&mut worker, &port, &job.job_id, at(34)).await;
    assert_eq!(outcome.lease.attempt, 2);
    assert_eq!(outcome.outcome.status, ExecutionOutcomeStatus::Succeeded);
    assert_eq!(outcome.outcome.last_event_sequence.0, 1);
    close_slot(&mut storage, &authority, 20, false);
    RepositoryExecutionScheduler::new(&mut storage)
        .settle_terminal(&terminal_request(&job, &outcome, 20))
        .expect("settle retry");
    settle_admission(&mut storage, &job, 20, false);
    let replay = RepositoryExecutionScheduler::new(&mut storage)
        .retry_failed(&retry_request)
        .expect("replay retry")
        .expect("replayed retry dispatch");
    assert_eq!(replay, retry);
}
