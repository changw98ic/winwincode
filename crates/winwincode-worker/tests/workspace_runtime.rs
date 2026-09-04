// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::HashMap,
    fmt::Write as _,
    path::PathBuf,
    process::Command,
    sync::{Arc, Mutex},
};

use sha2::{Digest, Sha256};
use winwincode_domain::{
    ArtifactId, CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionAckSequence, ExecutionJobId,
    FencingToken, Instant, LeaseId, ProductSessionId, RepositoryId, RequestId, SchemaVersion,
    SessionIdentity, Sha256Digest, StageRunId, WorkerId, WorkerInstanceId, WorkerSessionId,
    WorkspaceRevision,
};
use winwincode_execution_port::{
    change_batch_identity::derive_change_batch_id,
    generated::{
        AppliedFileOperation, AppliedFileSummary, ArtifactReference, ChangeBatchIdentity,
        ChangeBatchProgressEvent, ChangeBatchProgressState, ChangeBatchProposal,
        ChangeBatchProposalDisposition, ChangeBatchProposalEvent, ChangeBatchReceiptStatus,
        DeliveryStageAcceptanceCriterionInput, DeliveryStageExecutionScope,
        DeliveryStageExecutionScopeKind, DeliveryStageInput, DeliveryStageTaskInput, ExecutionJob,
        ExecutionJobReplacementAuthority, ExecutionLeaseStamp, ExecutionLimits, ExecutionScope,
        ExecutionWorkspace, ExecutionWorkspaceWriteMode, ValidationProfileName,
    },
};
use winwincode_worker::{
    ActiveJob, ActiveJobLifecycle, CodexRunKey,
    change_batch_journal::{ActiveBatchState, ChangeBatchJournal, ObservationGateResult},
    workspace::{WorkerWorkspace, WorkspaceCloseReason},
    workspace_runtime::{
        ChangeBatchExecutionRequest, ChangeBatchExecutionResult, ChangeBatchExecutor,
        ChangeBatchExecutorFuture, ChangeBatchWorkspaceRecovery, JobWorkspaceError,
        JobWorkspaceErrorCode, JobWorkspaceRuntime, ValidationArtifactError,
        ValidationArtifactPort, ValidationArtifactRequest, ValidationArtifactStream,
        WorkspaceTreeCompareFuture, WorkspaceTreeCompareResult, WorkspaceTreeFuture,
        WorkspaceTreePort, WorkspaceTreeRestoreFuture, WorkspaceTreeRestoreResult,
    },
};

#[cfg(feature = "test-support")]
use winwincode_worker::workspace::{
    WorkspaceCleanupInterruption, WorkspaceCreationInterruption, WorkspaceCreationRollbackFailure,
};

struct Fixture {
    root: PathBuf,
    sources: PathBuf,
    workspaces: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let unique = format!(
            "winwincode-workspace-runtime-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        );
        let root = std::env::temp_dir().join(unique);
        let sources = root.join("sources");
        let workspaces = root.join("workspaces");
        let repository = sources.join("repo_00000000000000000000000001");
        std::fs::create_dir_all(&repository).expect("create source repository");
        std::fs::create_dir_all(&workspaces).expect("create workspace root");
        git(&repository, &["init", "-q"]);
        git(&repository, &["config", "user.name", "WinWinCode Fixture"]);
        git(
            &repository,
            &["config", "user.email", "fixture@example.invalid"],
        );
        std::fs::write(repository.join("fixture.txt"), b"source\n").expect("write source");
        git(&repository, &["add", "fixture.txt"]);
        git(&repository, &["commit", "-qm", "source"]);
        Self {
            root,
            sources,
            workspaces,
        }
    }

    fn runtime(&self) -> JobWorkspaceRuntime {
        JobWorkspaceRuntime::open(&self.workspaces, &self.sources)
            .expect("open workspace runtime")
            .with_validation_artifact_port(FixtureValidationArtifacts::default())
    }

    fn repository(&self) -> PathBuf {
        self.sources.join("repo_00000000000000000000000001")
    }

    fn install_validation_config_text(&self, configuration: &str) {
        let repository = self.repository();
        std::fs::create_dir_all(repository.join(".winwincode"))
            .expect("validation config directory");
        std::fs::write(
            repository.join(".winwincode/validation.toml"),
            configuration,
        )
        .expect("validation config");
        git(&repository, &["add", ".winwincode/validation.toml"]);
        git(&repository, &["commit", "-qm", "validation config"]);
    }
}

#[derive(Debug, Default)]
struct FixtureValidationArtifacts {
    retained: HashMap<String, (Vec<u8>, ArtifactReference)>,
}

impl ValidationArtifactPort for FixtureValidationArtifacts {
    fn persist(
        &mut self,
        request: ValidationArtifactRequest<'_>,
    ) -> Result<ArtifactReference, ValidationArtifactError> {
        let stream = match request.stream {
            ValidationArtifactStream::Stdout => "stdout",
            ValidationArtifactStream::Stderr => "stderr",
        };
        let key = format!(
            "{}:{}:{}:{stream}",
            request.identity.batch_id.0, request.command_ordinal, request.command_id
        );
        if let Some((bytes, artifact)) = self.retained.get(&key) {
            return if bytes == request.bytes {
                Ok(artifact.clone())
            } else {
                Err(ValidationArtifactError)
            };
        }
        let digest = Sha256::digest(request.bytes);
        let key_digest = format!("{:x}", Sha256::digest(key.as_bytes()));
        let artifact = ArtifactReference {
            artifact_id: ArtifactId(format!("art_{}", &key_digest[..26])),
            digest: Sha256Digest(format!("sha256:{digest:x}")),
        };
        self.retained
            .insert(key, (request.bytes.to_vec(), artifact.clone()));
        Ok(artifact)
    }
}

const VALIDATION_CONFIG: &str = r#"schemaVersion = 1

[[commands]]
id = "python-format"
phase = "formatter"
language = "python"
allowedCompanionPaths = []
argv = ["/usr/bin/python3", "-B", "-c", 'from pathlib import Path; p=Path("delegated.txt"); p.write_text(p.read_text().replace("fixture", "formatted"))']
workingDirectory = "."
environment = []
network = false
timeoutMillis = 300000
outputLimitBytes = 1048576

[[commands]]
id = "python-check"
phase = "validation"
language = "python"
allowedCompanionPaths = []
argv = ["/usr/bin/python3", "-B", "-c", 'from pathlib import Path; assert Path("delegated.txt").read_text() == "formatted\n"']
workingDirectory = "."
environment = []
network = false
timeoutMillis = 300000
outputLimitBytes = 1048576

[[commands]]
id = "rust-check"
phase = "validation"
language = "rust"
allowedCompanionPaths = []
argv = ["/usr/bin/true"]
workingDirectory = "."
environment = []
network = false
timeoutMillis = 300000
outputLimitBytes = 1048576

[[commands]]
id = "typescript-check"
phase = "validation"
language = "typescript"
allowedCompanionPaths = []
argv = ["/usr/bin/true"]
workingDirectory = "."
environment = []
network = false
timeoutMillis = 300000
outputLimitBytes = 1048576

[[profiles]]
name = "changed"
commandIds = ["python-format", "python-check"]
[[profiles]]
name = "fast"
commandIds = ["rust-check"]
[[profiles]]
name = "affected"
commandIds = ["typescript-check"]
[[profiles]]
name = "final"
commandIds = ["rust-check", "typescript-check"]
"#;

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[derive(Debug)]
struct RecordingRecovery {
    observed: Arc<Mutex<Vec<(ExecutionJobId, String)>>>,
}

#[derive(Clone, Debug)]
struct ScriptedBatchExecutor {
    calls: Arc<Mutex<Vec<&'static str>>>,
    execute_result: ChangeBatchExecutionResult,
    recover_result: ChangeBatchExecutionResult,
    cancel_result: ChangeBatchExecutionResult,
}

#[derive(Debug)]
struct FixedWorkspaceTreePort;

#[derive(Debug)]
struct UncertainWorkspaceTreePort;

impl WorkspaceTreePort for FixedWorkspaceTreePort {
    fn compute_tree<'operation>(
        &'operation mut self,
        _checkout: &'operation std::path::Path,
        _state_root: &'operation std::path::Path,
        _base: &'operation WorkspaceRevision,
        _files: &'operation [AppliedFileSummary],
        _delta_digest: &'operation Sha256Digest,
    ) -> WorkspaceTreeFuture<'operation> {
        Box::pin(async { Ok(WorkspaceRevision(format!("git-tree:{}", "e".repeat(40)))) })
    }

    fn compare_tree<'operation>(
        &'operation mut self,
        _checkout: &'operation std::path::Path,
        _state_root: &'operation std::path::Path,
        _expected: &'operation WorkspaceRevision,
    ) -> WorkspaceTreeCompareFuture<'operation> {
        Box::pin(async { Ok(WorkspaceTreeCompareResult::Exact) })
    }

    fn restore_tree<'operation>(
        &'operation mut self,
        _workspace_id: &'operation str,
        _checkout: &'operation std::path::Path,
        _state_root: &'operation std::path::Path,
        _journal_root: &'operation std::path::Path,
        _expected_current: &'operation WorkspaceRevision,
        _target: &'operation WorkspaceRevision,
    ) -> WorkspaceTreeRestoreFuture<'operation> {
        Box::pin(async { Ok(WorkspaceTreeRestoreResult::AlreadyAtTarget) })
    }
}

impl WorkspaceTreePort for UncertainWorkspaceTreePort {
    fn compute_tree<'operation>(
        &'operation mut self,
        _checkout: &'operation std::path::Path,
        _state_root: &'operation std::path::Path,
        _base: &'operation WorkspaceRevision,
        _files: &'operation [AppliedFileSummary],
        _delta_digest: &'operation Sha256Digest,
    ) -> WorkspaceTreeFuture<'operation> {
        Box::pin(async { Ok(WorkspaceRevision(format!("git-tree:{}", "e".repeat(40)))) })
    }

    fn compare_tree<'operation>(
        &'operation mut self,
        _checkout: &'operation std::path::Path,
        _state_root: &'operation std::path::Path,
        _expected: &'operation WorkspaceRevision,
    ) -> WorkspaceTreeCompareFuture<'operation> {
        Box::pin(async { Ok(WorkspaceTreeCompareResult::Exact) })
    }

    fn restore_tree<'operation>(
        &'operation mut self,
        _workspace_id: &'operation str,
        _checkout: &'operation std::path::Path,
        _state_root: &'operation std::path::Path,
        _journal_root: &'operation std::path::Path,
        _expected_current: &'operation WorkspaceRevision,
        _target: &'operation WorkspaceRevision,
    ) -> WorkspaceTreeRestoreFuture<'operation> {
        Box::pin(async { Ok(WorkspaceTreeRestoreResult::StateUncertain) })
    }
}

impl ChangeBatchExecutor for ScriptedBatchExecutor {
    fn execute<'operation>(
        &'operation mut self,
        _request: ChangeBatchExecutionRequest<'operation>,
    ) -> ChangeBatchExecutorFuture<'operation> {
        self.calls.lock().expect("batch calls").push("execute");
        let result = self.execute_result.clone();
        Box::pin(async move { Ok(result) })
    }

    fn recover<'operation>(
        &'operation mut self,
        _request: ChangeBatchExecutionRequest<'operation>,
    ) -> ChangeBatchExecutorFuture<'operation> {
        self.calls.lock().expect("batch calls").push("recover");
        let result = self.recover_result.clone();
        Box::pin(async move { Ok(result) })
    }

    fn cancel<'operation>(
        &'operation mut self,
        _request: ChangeBatchExecutionRequest<'operation>,
    ) -> ChangeBatchExecutorFuture<'operation> {
        self.calls.lock().expect("batch calls").push("cancel");
        let result = self.cancel_result.clone();
        Box::pin(async move { Ok(result) })
    }
}

impl ChangeBatchWorkspaceRecovery for RecordingRecovery {
    fn recover(
        &mut self,
        active: &ActiveJob,
        workspace: &mut WorkerWorkspace,
    ) -> Result<(), JobWorkspaceError> {
        self.observed.lock().expect("recovery observer").push((
            active.job.job_id.clone(),
            workspace.resolved_source_commit().to_owned(),
        ));
        Ok(())
    }
}

fn git(repository: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .status()
        .expect("run Git");
    assert!(status.success(), "Git command failed: {args:?}");
}

fn git_output(repository: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .output()
        .expect("run Git");
    assert!(output.status.success(), "Git command failed: {args:?}");
    String::from_utf8(output.stdout)
        .expect("Git output is UTF-8")
        .trim()
        .to_owned()
}

fn active_job() -> ActiveJob {
    let worker_session_id = WorkerSessionId("wsn_00000000000000000000000001".to_owned());
    let codex_thread_id = CodexThreadId("cdx_00000000000000000000000001".to_owned());
    let task_id = DeliveryTaskId("dtk_00000000000000000000000001".to_owned());
    ActiveJob {
        job: ExecutionJob {
            attempt: 1,
            execution_profile: "executor".to_owned(),
            goal: "Implement fixture".to_owned(),
            job_id: ExecutionJobId("job_00000000000000000000000001".to_owned()),
            limits: ExecutionLimits {
                deadline_at: Instant("2026-08-28T01:00:00.000Z".to_owned()),
                max_artifact_bytes: 1_048_576,
                max_runtime_seconds: 300,
            },
            payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            scope: ExecutionScope::DeliveryStageExecutionScope(DeliveryStageExecutionScope {
                delivery_id: DeliveryId("dlv_00000000000000000000000001".to_owned()),
                delivery_task_id: Some(task_id.clone()),
                kind: DeliveryStageExecutionScopeKind::DeliveryStage,
                product_session_id: ProductSessionId("psn_00000000000000000000000001".to_owned()),
                rework_authorization: None,
                stage_run_id: StageRunId("run_00000000000000000000000001".to_owned()),
            }),
            stage_input: Some(DeliveryStageInput {
                acceptance_criteria: vec![DeliveryStageAcceptanceCriterionInput {
                    criterion_id: "criterion-fixture".to_owned(),
                    description: "The fixture change is present.".to_owned(),
                    required: true,
                    verification_method: Some("Inspect fixture.txt".to_owned()),
                }],
                candidate_ref: None,
                constraints: vec!["Keep repository isolation.".to_owned()],
                delivery_spec_id: "spec-fixture".to_owned(),
                delivery_spec_revision: 1,
                goal: "Implement fixture".to_owned(),
                out_of_scope: Vec::new(),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: vec!["fixture.txt".to_owned()],
                task: Some(DeliveryStageTaskInput {
                    acceptance_criterion_ids: vec!["criterion-fixture".to_owned()],
                    goal: "Implement fixture".to_owned(),
                    task_id,
                    title: "Implement fixture".to_owned(),
                }),
                title: "Fixture delivery".to_owned(),
            }),
            workspace: ExecutionWorkspace {
                checkout_revision: "HEAD".to_owned(),
                repository_id: RepositoryId("repo_00000000000000000000000001".to_owned()),
                write_mode: ExecutionWorkspaceWriteMode::Candidate,
            },
        },
        lease: ExecutionLeaseStamp {
            attempt: 1,
            expires_at: Instant("2026-08-28T01:00:00.000Z".to_owned()),
            fencing_token: FencingToken("1".to_owned()),
            issued_at: Instant("2026-08-28T00:00:00.000Z".to_owned()),
            job_id: ExecutionJobId("job_00000000000000000000000001".to_owned()),
            lease_id: LeaseId("lse_00000000000000000000000001".to_owned()),
            worker_id: WorkerId("wrk_00000000000000000000000001".to_owned()),
            worker_instance_id: WorkerInstanceId("wki_00000000000000000000000001".to_owned()),
        },
        worker_session_id: worker_session_id.clone(),
        session_identity: SessionIdentity {
            codex_thread_id: codex_thread_id.clone(),
            product_session_id: ProductSessionId("psn_00000000000000000000000001".to_owned()),
            stage_run_id: Some(StageRunId("run_00000000000000000000000001".to_owned())),
            worker_session_id,
        },
        codex_thread_id,
        lifecycle: ActiveJobLifecycle::Running,
        last_event_sequence: ExecutionAckSequence(0),
    }
}

fn source_revision(fixture: &Fixture) -> WorkspaceRevision {
    WorkspaceRevision(format!(
        "git-tree:{}",
        git_output(&fixture.repository(), &["rev-parse", "HEAD^{tree}"])
    ))
}

fn batch_proposal(
    active: &ActiveJob,
    workspace_revision: WorkspaceRevision,
) -> ChangeBatchProposalEvent {
    let patch = "*** Begin Patch\n*** Add File: delegated.txt\n+fixture\n*** End Patch\n";
    batch_proposal_with_patch(active, workspace_revision, patch, "turn-fixture")
}

fn batch_proposal_with_patch(
    active: &ActiveJob,
    workspace_revision: WorkspaceRevision,
    patch: &str,
    turn_id: &str,
) -> ChangeBatchProposalEvent {
    let patch_digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(patch.as_bytes())));
    let run_key = CodexRunKey {
        job_id: active.job.job_id.clone(),
        attempt: active.job.attempt,
        fencing_token: active.lease.fencing_token.clone(),
        payload_digest: active.job.payload_digest.clone(),
    }
    .canonical_digest()
    .expect("canonical batch run key")
    .0;
    ChangeBatchProposalEvent {
        identity: ChangeBatchIdentity {
            attempt: active.job.attempt,
            batch_id: derive_change_batch_id(&run_key, turn_id, None, &patch_digest)
                .expect("canonical batch id"),
            call_id: None,
            fencing_token: active.lease.fencing_token.clone(),
            job_id: active.job.job_id.clone(),
            lease_id: active.lease.lease_id.clone(),
            patch_digest,
            repository_id: active.job.workspace.repository_id.clone(),
            run_key,
            session_identity: active.session_identity.clone(),
            turn_id: turn_id.to_owned(),
            workspace_revision,
        },
        occurred_at: Instant("2026-08-28T00:00:01.000Z".to_owned()),
        proposal: ChangeBatchProposal {
            acceptance_criteria_ids: vec!["criterion-fixture".to_owned()],
            disposition: ChangeBatchProposalDisposition::Final,
            patch: patch.to_owned(),
            schema_version: 1,
            validation_profile: ValidationProfileName::Changed,
        },
    }
}

fn applied_result() -> ChangeBatchExecutionResult {
    ChangeBatchExecutionResult::Applied {
        files: vec![AppliedFileSummary {
            after_sha256: Some(Sha256Digest(format!(
                "sha256:{:x}",
                Sha256::digest(b"fixture\n")
            ))),
            before_sha256: None,
            bytes_after: 8,
            bytes_before: 0,
            mode_after: Some("644".to_owned()),
            mode_before: None,
            move_path: None,
            operation: AppliedFileOperation::Create,
            path: "delegated.txt".to_owned(),
        }],
        artifact_ref: None,
    }
}

fn scripted_executor(
    calls: &Arc<Mutex<Vec<&'static str>>>,
    result: ChangeBatchExecutionResult,
) -> ScriptedBatchExecutor {
    ScriptedBatchExecutor {
        calls: Arc::clone(calls),
        execute_result: result.clone(),
        recover_result: result.clone(),
        cancel_result: result,
    }
}

fn replacement_successor(predecessor: &ActiveJob) -> ActiveJob {
    let mut successor = predecessor.clone();
    successor.job.attempt = 2;
    successor.lease.attempt = 2;
    successor.lease.lease_id = LeaseId("lse_00000000000000000000000002".to_owned());
    successor.lease.fencing_token = FencingToken("2".to_owned());
    successor.lease.issued_at = Instant("2026-08-28T00:10:00.000Z".to_owned());
    successor.lease.expires_at = Instant("2026-08-28T01:10:00.000Z".to_owned());
    successor.lease.worker_instance_id =
        WorkerInstanceId("wki_00000000000000000000000002".to_owned());
    successor.worker_session_id = WorkerSessionId("wsn_00000000000000000000000002".to_owned());
    successor.codex_thread_id = CodexThreadId("cdx_00000000000000000000000002".to_owned());
    successor.session_identity.worker_session_id = successor.worker_session_id.clone();
    successor.session_identity.codex_thread_id = successor.codex_thread_id.clone();
    successor
}

fn second_replacement_successor(predecessor: &ActiveJob) -> ActiveJob {
    let mut successor = predecessor.clone();
    successor.job.attempt = 3;
    successor.lease.attempt = 3;
    successor.lease.lease_id = LeaseId("lse_00000000000000000000000003".to_owned());
    successor.lease.fencing_token = FencingToken("3".to_owned());
    successor.lease.issued_at = Instant("2026-08-28T00:20:00.000Z".to_owned());
    successor.lease.expires_at = Instant("2026-08-28T01:20:00.000Z".to_owned());
    successor.lease.worker_instance_id =
        WorkerInstanceId("wki_00000000000000000000000003".to_owned());
    successor.worker_session_id = WorkerSessionId("wsn_00000000000000000000000003".to_owned());
    successor.codex_thread_id = CodexThreadId("cdx_00000000000000000000000003".to_owned());
    successor.session_identity.worker_session_id = successor.worker_session_id.clone();
    successor.session_identity.codex_thread_id = successor.codex_thread_id.clone();
    successor
}

#[cfg(feature = "test-support")]
fn second_active_job() -> ActiveJob {
    let mut active = active_job();
    active.job.job_id = ExecutionJobId("job_00000000000000000000000002".to_owned());
    active.lease.job_id = active.job.job_id.clone();
    active.lease.lease_id = LeaseId("lse_00000000000000000000000012".to_owned());
    active.lease.fencing_token = FencingToken("12".to_owned());
    active.worker_session_id = WorkerSessionId("wsn_00000000000000000000000012".to_owned());
    active.codex_thread_id = CodexThreadId("cdx_00000000000000000000000012".to_owned());
    active.session_identity.worker_session_id = active.worker_session_id.clone();
    active.session_identity.codex_thread_id = active.codex_thread_id.clone();
    active
}

fn logical_job_digest(job: &ExecutionJob) -> Sha256Digest {
    let mut value = serde_json::to_value(job).expect("ExecutionJob value");
    value
        .as_object_mut()
        .expect("ExecutionJob object")
        .remove("attempt")
        .expect("ExecutionJob attempt");
    Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&value).expect("logical Job bytes"))
    ))
}

fn replacement_authority(
    predecessor: &ActiveJob,
    successor: &ActiveJob,
) -> ExecutionJobReplacementAuthority {
    ExecutionJobReplacementAuthority {
        created_at: Instant("2026-08-28T00:09:59.000Z".to_owned()),
        logical_job_digest: logical_job_digest(&successor.job),
        predecessor_lease: predecessor.lease.clone(),
        predecessor_session_identity: Some(predecessor.session_identity.clone()),
        receipt_digest: Sha256Digest(format!("sha256:{}", "f".repeat(64))),
        receipt_id: RequestId("req_00000000000000000000000009".to_owned()),
        scope: successor.job.scope.clone(),
        successor_lease: successor.lease.clone(),
    }
}

#[test]
fn crash_recovery_keeps_original_checkout_and_freezes_one_candidate() {
    let fixture = Fixture::new("recovery");
    let active = active_job();
    let mut first = fixture.runtime();
    let checkout = first
        .open_for_job(&active, None)
        .expect("create Job checkout");
    let original_source_commit = git_output(&fixture.repository(), &["rev-parse", "HEAD"]);
    std::fs::write(checkout.join("fixture.txt"), b"candidate\n").expect("write candidate change");

    // Dropping a non-terminal runtime preserves the checkout for exact restart.
    drop(first);
    std::fs::write(fixture.repository().join("upstream.txt"), b"new source\n")
        .expect("advance source bytes");
    git(&fixture.repository(), &["add", "upstream.txt"]);
    git(&fixture.repository(), &["commit", "-qm", "advance source"]);
    assert_ne!(
        git_output(&fixture.repository(), &["rev-parse", "HEAD"]),
        original_source_commit
    );
    let mut restarted = fixture.runtime();
    let recovered = restarted
        .open_for_job(&active, None)
        .expect("recover exact Job checkout");
    assert_eq!(recovered, checkout);
    assert_eq!(
        std::fs::read(recovered.join("fixture.txt")).expect("read recovered change"),
        b"candidate\n"
    );
    let prepared = restarted
        .prepare_candidate(&active)
        .expect("freeze recovered candidate");
    assert_ne!(
        prepared.snapshot().candidate_tree_id,
        prepared.snapshot().source_tree_id
    );
    assert_eq!(prepared.snapshot().source_commit_id, original_source_commit);
    let root = recovered.parent().expect("workspace root").to_path_buf();
    let report = restarted
        .close_job(&active.job.job_id, WorkspaceCloseReason::Completed)
        .expect("close Job workspace");
    assert_eq!(report.removed_root, root);
    assert!(!root.exists());
}

#[test]
fn frozen_candidate_restarts_with_the_same_commit_and_artifact_bytes() {
    let fixture = Fixture::new("frozen-recovery");
    let active = active_job();
    let mut first = fixture.runtime();
    let checkout = first
        .open_for_job(&active, None)
        .expect("create Job checkout");
    let source_commit = git_output(&checkout, &["rev-parse", "HEAD"]);
    std::fs::write(checkout.join("fixture.txt"), b"candidate\n").expect("write candidate change");

    let original = first
        .prepare_candidate(&active)
        .expect("freeze original candidate");
    assert_eq!(git_output(&checkout, &["rev-parse", "HEAD"]), source_commit);
    drop(first);

    let mut restarted = fixture.runtime();
    let recovered = restarted
        .open_for_job(&active, None)
        .expect("recover candidate checkout");
    assert_eq!(recovered, checkout);
    assert_eq!(
        git_output(&recovered, &["rev-parse", "HEAD"]),
        source_commit
    );
    let replayed = restarted
        .prepare_candidate(&active)
        .expect("freeze the same candidate after restart");
    assert_eq!(replayed, original);
    restarted
        .close_job(&active.job.job_id, WorkspaceCloseReason::Completed)
        .expect("close recovered candidate workspace");
}

#[test]
fn sealed_replacement_rotates_authority_and_preserves_the_predecessor_checkout() {
    let fixture = Fixture::new("replacement");
    let predecessor = active_job();
    let successor = replacement_successor(&predecessor);
    let receipt = replacement_authority(&predecessor, &successor);
    let mut first = fixture.runtime();
    let checkout = first
        .open_for_job(&predecessor, None)
        .expect("create predecessor checkout");
    std::fs::write(checkout.join("fixture.txt"), b"candidate\n").expect("write predecessor change");
    drop(first);

    let mut restarted = fixture.runtime();
    let recovered = restarted
        .open_for_job(&successor, Some(&receipt))
        .expect("rotate exact replacement authority");
    assert_eq!(recovered, checkout);
    assert_eq!(
        std::fs::read(recovered.join("fixture.txt")).expect("read predecessor change"),
        b"candidate\n"
    );
    let prepared = restarted
        .prepare_candidate(&successor)
        .expect("freeze candidate under successor authority");
    assert_eq!(
        prepared.snapshot().origin_provenance.worker_instance_id,
        predecessor.lease.worker_instance_id
    );
    assert_eq!(
        prepared.snapshot().provenance.worker_instance_id,
        successor.lease.worker_instance_id
    );
    let candidate_commit = prepared.snapshot().candidate_commit_id.clone();
    drop(restarted);

    let mut replayed = fixture.runtime();
    assert_eq!(
        replayed
            .open_for_job(&successor, Some(&receipt))
            .expect("replay exact replacement receipt"),
        checkout
    );
    assert_eq!(
        replayed
            .prepare_candidate(&successor)
            .expect("replay successor candidate")
            .snapshot()
            .candidate_commit_id,
        candidate_commit
    );
    replayed
        .close_job(&successor.job.job_id, WorkspaceCloseReason::Completed)
        .expect("close replacement checkout");
}

#[test]
fn terminal_cleanup_accepts_the_latest_receipt_after_multiple_replacements() {
    let fixture = Fixture::new("multiple-replacements");
    let first = active_job();
    let second = replacement_successor(&first);
    let third = second_replacement_successor(&second);
    let first_receipt = replacement_authority(&first, &second);
    let mut second_receipt = replacement_authority(&second, &third);
    second_receipt.receipt_id = RequestId("req_00000000000000000000000010".to_owned());
    second_receipt.receipt_digest = Sha256Digest(format!("sha256:{}", "e".repeat(64)));

    let mut first_runtime = fixture.runtime();
    let checkout = first_runtime
        .open_for_job(&first, None)
        .expect("create first attempt workspace");
    std::fs::write(
        checkout.join("fixture.txt"),
        b"candidate across replacements\n",
    )
    .expect("write first attempt change");
    drop(first_runtime);

    let mut second_runtime = fixture.runtime();
    second_runtime
        .open_for_job(&second, Some(&first_receipt))
        .expect("open second attempt workspace");
    drop(second_runtime);

    let mut third_runtime = fixture.runtime();
    assert_eq!(
        third_runtime
            .open_for_job(&third, Some(&second_receipt))
            .expect("open third attempt workspace"),
        checkout
    );
    third_runtime
        .close_job(&third.job.job_id, WorkspaceCloseReason::Completed)
        .expect("latest replacement receipt permits terminal cleanup");
    assert!(!checkout.parent().expect("workspace root").exists());
}

#[test]
fn replacement_rejects_missing_changed_or_foreign_predecessor_authority() {
    let fixture = Fixture::new("replacement-negative");
    let predecessor = active_job();
    let successor = replacement_successor(&predecessor);
    let receipt = replacement_authority(&predecessor, &successor);
    let mut first = fixture.runtime();
    first
        .open_for_job(&predecessor, None)
        .expect("create predecessor checkout");
    drop(first);

    let mut missing = fixture.runtime();
    assert_eq!(
        missing
            .open_for_job(&successor, None)
            .expect_err("successor without receipt must fail")
            .code(),
        JobWorkspaceErrorCode::Workspace
    );
    let mut foreign = receipt;
    foreign.scope = ExecutionScope::DeliveryStageExecutionScope(DeliveryStageExecutionScope {
        delivery_id: DeliveryId("dlv_00000000000000000000000009".to_owned()),
        delivery_task_id: None,
        kind: DeliveryStageExecutionScopeKind::DeliveryStage,
        product_session_id: ProductSessionId("psn_00000000000000000000000009".to_owned()),
        rework_authorization: None,
        stage_run_id: StageRunId("run_00000000000000000000000009".to_owned()),
    });
    assert_eq!(
        missing
            .open_for_job(&successor, Some(&foreign))
            .expect_err("foreign replacement scope must fail")
            .code(),
        JobWorkspaceErrorCode::Workspace
    );
    let mut original = fixture.runtime();
    original
        .open_for_job(&predecessor, None)
        .expect("predecessor remains recoverable")
        .parent()
        .expect("workspace root");
    original
        .close_job(&predecessor.job.job_id, WorkspaceCloseReason::Cancelled)
        .expect("close predecessor checkout");
}

#[test]
fn unbound_clean_predecessor_is_removed_before_successor_checkout_creation() {
    let fixture = Fixture::new("replacement-unbound-clean");
    let predecessor = active_job();
    let successor = replacement_successor(&predecessor);
    let mut receipt = replacement_authority(&predecessor, &successor);
    receipt.predecessor_session_identity = None;
    let mut first = fixture.runtime();
    let old_checkout = first
        .open_for_job(&predecessor, None)
        .expect("create unbound predecessor checkout");
    let old_root = old_checkout
        .parent()
        .expect("old workspace root")
        .to_path_buf();
    let old_source_commit = git_output(&old_checkout, &["rev-parse", "HEAD"]);
    drop(first);
    std::fs::write(fixture.repository().join("advanced.txt"), b"new source\n")
        .expect("advance symbolic source revision");
    git(&fixture.repository(), &["add", "advanced.txt"]);
    git(&fixture.repository(), &["commit", "-qm", "advance source"]);
    let advanced_source_commit = git_output(&fixture.repository(), &["rev-parse", "HEAD"]);
    assert_ne!(advanced_source_commit, old_source_commit);

    let mut restarted = fixture.runtime();
    let successor_checkout = restarted
        .open_for_job(&successor, Some(&receipt))
        .expect("replace clean unbound predecessor");
    assert_ne!(successor_checkout, old_checkout);
    assert!(!old_root.exists());
    assert_eq!(
        std::fs::read(successor_checkout.join("fixture.txt")).expect("read fresh source"),
        b"source\n"
    );
    assert_eq!(
        git_output(&successor_checkout, &["rev-parse", "HEAD"]),
        old_source_commit
    );
    assert!(!successor_checkout.join("advanced.txt").exists());
    assert_eq!(
        git_output(&fixture.repository(), &["rev-parse", "HEAD"]),
        advanced_source_commit
    );
    restarted
        .close_job(&successor.job.job_id, WorkspaceCloseReason::Completed)
        .expect("close successor checkout");
}

#[test]
fn unbound_dirty_predecessor_fails_closed_and_preserves_investigation_bytes() {
    let fixture = Fixture::new("replacement-unbound-dirty");
    let predecessor = active_job();
    let successor = replacement_successor(&predecessor);
    let mut receipt = replacement_authority(&predecessor, &successor);
    receipt.predecessor_session_identity = None;
    let mut first = fixture.runtime();
    let checkout = first
        .open_for_job(&predecessor, None)
        .expect("create unbound predecessor checkout");
    std::fs::write(checkout.join("fixture.txt"), b"unaccepted\n")
        .expect("write unaccepted predecessor bytes");
    drop(first);

    let mut restarted = fixture.runtime();
    assert_eq!(
        restarted
            .open_for_job(&successor, Some(&receipt))
            .expect_err("dirty unbound predecessor must fail closed")
            .code(),
        JobWorkspaceErrorCode::Workspace
    );
    assert_eq!(
        std::fs::read(checkout.join("fixture.txt")).expect("preserved investigation bytes"),
        b"unaccepted\n"
    );
    restarted
        .open_for_job(&predecessor, None)
        .expect("predecessor remains recoverable");
    restarted
        .close_job(&predecessor.job.job_id, WorkspaceCloseReason::Failed)
        .expect("explicitly close investigated predecessor");
}

#[test]
#[cfg(feature = "test-support")]
fn durable_creation_intent_recovers_every_pre_active_crash_point() {
    for (name, interruption) in [
        ("root", WorkspaceCreationInterruption::AfterRootCreated),
        (
            "manifest",
            WorkspaceCreationInterruption::AfterCreatingManifest,
        ),
        (
            "worktree",
            WorkspaceCreationInterruption::AfterWorktreeAdded,
        ),
    ] {
        let fixture = Fixture::new(&format!("creating-{name}"));
        let active = active_job();
        let resolved_source_commit = git_output(&fixture.repository(), &["rev-parse", "HEAD"]);
        let mut interrupted = fixture.runtime();
        assert_eq!(
            interrupted
                .interrupt_workspace_creation_for_test(&active, None, interruption)
                .expect_err("creation must stop at the selected durable phase")
                .code(),
            JobWorkspaceErrorCode::Workspace
        );
        drop(interrupted);
        std::fs::write(
            fixture.repository().join(format!("advanced-{name}.txt")),
            b"new source\n",
        )
        .expect("advance symbolic source after creation crash");
        git(&fixture.repository(), &["add", "."]);
        git(&fixture.repository(), &["commit", "-qm", "advance source"]);

        let mut restarted = fixture.runtime();
        let checkout = restarted
            .open_for_job(&active, None)
            .expect("restart reconciles the exact creation intent");
        assert_eq!(
            git_output(&checkout, &["rev-parse", "HEAD"]),
            resolved_source_commit
        );
        assert!(!checkout.join(format!("advanced-{name}.txt")).exists());
        assert_eq!(workspace_directory_count(&fixture.workspaces), 1);
        restarted
            .close_job(&active.job.job_id, WorkspaceCloseReason::Cancelled)
            .expect("close recovered workspace");
    }
}

#[test]
#[cfg(feature = "test-support")]
fn replacement_rotates_every_unfinished_predecessor_creation_without_source_drift() {
    for (name, interruption) in [
        ("root", WorkspaceCreationInterruption::AfterRootCreated),
        (
            "manifest",
            WorkspaceCreationInterruption::AfterCreatingManifest,
        ),
        (
            "worktree",
            WorkspaceCreationInterruption::AfterWorktreeAdded,
        ),
    ] {
        let fixture = Fixture::new(&format!("creating-replacement-{name}"));
        let predecessor = active_job();
        let successor = replacement_successor(&predecessor);
        let mut receipt = replacement_authority(&predecessor, &successor);
        receipt.predecessor_session_identity = None;
        let resolved_source_commit = git_output(&fixture.repository(), &["rev-parse", "HEAD"]);
        let mut interrupted = fixture.runtime();
        interrupted
            .interrupt_workspace_creation_for_test(&predecessor, None, interruption)
            .expect_err("predecessor creation stops before session acceptance");
        drop(interrupted);
        std::fs::write(
            fixture.repository().join(format!("advanced-{name}.txt")),
            b"new source\n",
        )
        .expect("advance source after predecessor crash");
        git(&fixture.repository(), &["add", "."]);
        git(&fixture.repository(), &["commit", "-qm", "advance source"]);

        let mut restarted = fixture.runtime();
        let checkout = restarted
            .open_for_job(&successor, Some(&receipt))
            .expect("sealed successor rotates the unfinished predecessor intent");
        assert_eq!(
            git_output(&checkout, &["rev-parse", "HEAD"]),
            resolved_source_commit
        );
        assert!(!checkout.join(format!("advanced-{name}.txt")).exists());
        assert_eq!(workspace_directory_count(&fixture.workspaces), 1);
        restarted
            .close_job(&successor.job.job_id, WorkspaceCloseReason::Completed)
            .expect("close successor workspace");
    }
}

#[test]
#[cfg(feature = "test-support")]
fn null_replacement_recovers_precreated_successor_before_removing_old_checkout() {
    let fixture = Fixture::new("replacement-precreated-crash");
    let predecessor = active_job();
    let successor = replacement_successor(&predecessor);
    let mut receipt = replacement_authority(&predecessor, &successor);
    receipt.predecessor_session_identity = None;
    let mut first = fixture.runtime();
    let old_checkout = first
        .open_for_job(&predecessor, None)
        .expect("create clean predecessor");
    let old_root = old_checkout.parent().expect("old root").to_path_buf();
    let old_source_commit = git_output(&old_checkout, &["rev-parse", "HEAD"]);
    drop(first);
    std::fs::write(fixture.repository().join("advanced.txt"), b"new source\n")
        .expect("advance source");
    git(&fixture.repository(), &["add", "advanced.txt"]);
    git(&fixture.repository(), &["commit", "-qm", "advance source"]);

    let mut interrupted = fixture.runtime();
    interrupted
        .interrupt_workspace_creation_for_test(
            &successor,
            Some(&receipt),
            WorkspaceCreationInterruption::AfterWorktreeAdded,
        )
        .expect_err("successor creation stops before Active");
    assert!(old_root.exists());
    assert_eq!(workspace_directory_count(&fixture.workspaces), 2);
    drop(interrupted);

    let mut restarted = fixture.runtime();
    let successor_checkout = restarted
        .open_for_job(&successor, Some(&receipt))
        .expect("restart completes successor then removes clean predecessor");
    assert!(!old_root.exists());
    assert_eq!(workspace_directory_count(&fixture.workspaces), 1);
    assert_eq!(
        git_output(&successor_checkout, &["rev-parse", "HEAD"]),
        old_source_commit
    );
    assert!(!successor_checkout.join("advanced.txt").exists());
    restarted
        .close_job(&successor.job.job_id, WorkspaceCloseReason::Completed)
        .expect("close successor");
}

#[test]
#[cfg(feature = "test-support")]
fn durable_cleaning_phase_finishes_every_partial_terminal_cleanup() {
    for (name, interruption) in [
        (
            "manifest",
            WorkspaceCleanupInterruption::AfterCleaningManifest,
        ),
        (
            "worktree",
            WorkspaceCleanupInterruption::AfterWorktreeRemoved,
        ),
        (
            "remove-failure",
            WorkspaceCleanupInterruption::FailWorktreeRemoval,
        ),
        ("prune-failure", WorkspaceCleanupInterruption::FailPrune),
        (
            "partial-root-failure",
            WorkspaceCleanupInterruption::FailRootRemovalAfterManifest,
        ),
        (
            "parent-sync-failure",
            WorkspaceCleanupInterruption::FailParentSync,
        ),
    ] {
        let fixture = Fixture::new(&format!("cleaning-{name}"));
        let active = active_job();
        let mut interrupted = fixture.runtime();
        let checkout = interrupted
            .open_for_job(&active, None)
            .expect("create active workspace");
        let workspace_root = checkout.parent().expect("workspace root").to_path_buf();
        interrupted
            .interrupt_workspace_cleanup_for_test(
                &active.job.job_id,
                WorkspaceCloseReason::Completed,
                interruption,
            )
            .expect_err("cleanup must stop at the selected durable phase");
        drop(interrupted);

        let restarted = fixture.runtime();
        assert!(!workspace_root.exists());
        assert_eq!(workspace_directory_count(&fixture.workspaces), 0);
        assert!(
            !git_output(&fixture.repository(), &["worktree", "list", "--porcelain"])
                .contains(checkout.to_string_lossy().as_ref())
        );
        drop(restarted);
    }
}

#[test]
#[cfg(feature = "test-support")]
fn startup_consumes_every_parallel_cleaning_intent_in_one_sorted_pass() {
    let fixture = Fixture::new("parallel-cleaning");
    let first = active_job();
    let second = second_active_job();
    let mut interrupted = fixture.runtime();
    interrupted
        .open_for_job(&first, None)
        .expect("create first workspace");
    interrupted
        .open_for_job(&second, None)
        .expect("create second workspace");
    for active in [&first, &second] {
        interrupted
            .interrupt_workspace_cleanup_for_test(
                &active.job.job_id,
                WorkspaceCloseReason::Completed,
                WorkspaceCleanupInterruption::AfterCleaningManifest,
            )
            .expect_err("leave durable Cleaning intent");
    }
    assert_eq!(workspace_directory_count(&fixture.workspaces), 2);
    drop(interrupted);

    let restarted = fixture.runtime();
    assert_eq!(workspace_directory_count(&fixture.workspaces), 0);
    assert_eq!(cleanup_intent_count(&fixture.workspaces), 0);
    drop(restarted);
}

#[test]
#[cfg(feature = "test-support")]
fn forged_cleanup_intent_never_deletes_an_active_workspace() {
    let fixture = Fixture::new("forged-cleanup");
    let active = active_job();
    let mut owner = fixture.runtime();
    let checkout = owner
        .open_for_job(&active, None)
        .expect("create active workspace");
    std::fs::write(checkout.join("fixture.txt"), b"active writer\n").expect("write active change");
    let workspace_root = checkout.parent().expect("workspace root").to_path_buf();
    let active_manifest = workspace_manifest_bytes(&workspace_root);
    let forged = manifest_with_phase(&active_manifest, "active", "cleaning");
    let cleanup_intent = workspace_intent_path(&fixture.workspaces, &workspace_root, "clean");
    std::fs::write(&cleanup_intent, forged).expect("write forged cleanup intent");
    drop(owner);

    let error = JobWorkspaceRuntime::open(&fixture.workspaces, &fixture.sources)
        .expect_err("forged cleanup intent must fail closed");
    assert_eq!(error.code(), JobWorkspaceErrorCode::Workspace);
    assert!(workspace_root.exists());
    assert_eq!(
        std::fs::read(checkout.join("fixture.txt")).expect("read active change"),
        b"active writer\n"
    );

    std::fs::remove_file(cleanup_intent).expect("remove forged cleanup intent");
    let mut recovered = fixture.runtime();
    recovered
        .open_for_job(&active, None)
        .expect("recover untouched active workspace");
    recovered
        .close_job(&active.job.job_id, WorkspaceCloseReason::Cancelled)
        .expect("close active workspace");
}

#[test]
#[cfg(feature = "test-support")]
fn tampered_parent_cleanup_authority_never_deletes_partial_workspace() {
    let fixture = Fixture::new("tampered-cleanup");
    let active = active_job();
    let mut owner = fixture.runtime();
    let checkout = owner
        .open_for_job(&active, None)
        .expect("create active workspace");
    let workspace_root = checkout.parent().expect("workspace root").to_path_buf();
    owner
        .interrupt_workspace_cleanup_for_test(
            &active.job.job_id,
            WorkspaceCloseReason::Completed,
            WorkspaceCleanupInterruption::AfterCleaningManifest,
        )
        .expect_err("leave durable cleanup authority");
    drop(owner);
    let cleanup_intent = workspace_intent_path(&fixture.workspaces, &workspace_root, "clean");
    let exact_intent = std::fs::read(&cleanup_intent).expect("read exact cleanup intent");
    std::fs::remove_file(workspace_root.join(".winwincode-workspace.json"))
        .expect("simulate partial root removal");
    std::fs::write(&cleanup_intent, replace_current_job_digest(&exact_intent))
        .expect("tamper current authority");

    let error = JobWorkspaceRuntime::open(&fixture.workspaces, &fixture.sources)
        .expect_err("tampered parent cleanup authority must fail closed");
    assert_eq!(error.code(), JobWorkspaceErrorCode::Workspace);
    assert!(workspace_root.exists());
    assert!(checkout.exists());

    std::fs::write(&cleanup_intent, exact_intent).expect("restore exact cleanup authority");
    let restarted = fixture.runtime();
    assert!(!workspace_root.exists());
    assert_eq!(cleanup_intent_count(&fixture.workspaces), 0);
    drop(restarted);
}

#[test]
#[cfg(feature = "test-support")]
fn live_workspace_owner_fences_replacement_and_recovery_reconciliation() {
    let fixture = Fixture::new("live-owner-fence");
    let predecessor = active_job();
    let successor = replacement_successor(&predecessor);
    let receipt = replacement_authority(&predecessor, &successor);
    let mut owner = fixture.runtime();
    let checkout = owner
        .open_for_job(&predecessor, None)
        .expect("create predecessor workspace");
    std::fs::write(checkout.join("fixture.txt"), b"live predecessor\n")
        .expect("write predecessor change");
    let workspace_root = checkout.parent().expect("workspace root").to_path_buf();
    let manifest_before = workspace_manifest_bytes(&workspace_root);

    let mut contender = fixture.runtime();
    assert_eq!(
        contender
            .open_for_job(&successor, Some(&receipt))
            .expect_err("live predecessor owner must fence successor")
            .code(),
        JobWorkspaceErrorCode::Workspace
    );
    assert_eq!(workspace_manifest_bytes(&workspace_root), manifest_before);
    assert_eq!(
        std::fs::read(checkout.join("fixture.txt")).expect("read fenced predecessor change"),
        b"live predecessor\n"
    );

    let creating_intent = workspace_intent_path(&fixture.workspaces, &workspace_root, "create");
    std::fs::write(
        &creating_intent,
        manifest_with_phase(&manifest_before, "active", "creating"),
    )
    .expect("write exact creating intent");
    assert_eq!(
        contender
            .open_for_job(&predecessor, None)
            .expect_err("live owner must fence creation reconciliation")
            .code(),
        JobWorkspaceErrorCode::Workspace
    );
    assert_eq!(workspace_manifest_bytes(&workspace_root), manifest_before);
    std::fs::remove_file(creating_intent).expect("remove test creation intent");

    drop(owner);
    let recovered = contender
        .open_for_job(&successor, Some(&receipt))
        .expect("successor acquires owner lock after predecessor exits");
    assert_eq!(recovered, checkout);
    assert_eq!(
        std::fs::read(recovered.join("fixture.txt")).expect("read handed off change"),
        b"live predecessor\n"
    );
    contender
        .close_job(&successor.job.job_id, WorkspaceCloseReason::Completed)
        .expect("close successor workspace");
}

#[test]
#[cfg(feature = "test-support")]
fn live_cleaning_owner_fences_startup_reconciliation() {
    let fixture = Fixture::new("live-cleaning-owner");
    let active = active_job();
    let mut owner = fixture.runtime();
    let checkout = owner
        .open_for_job(&active, None)
        .expect("create active workspace");
    let workspace_root = checkout.parent().expect("workspace root").to_path_buf();
    owner
        .interrupt_workspace_cleanup_for_test(
            &active.job.job_id,
            WorkspaceCloseReason::Completed,
            WorkspaceCleanupInterruption::AfterCleaningManifest,
        )
        .expect_err("leave live Cleaning owner");

    let error = JobWorkspaceRuntime::open(&fixture.workspaces, &fixture.sources)
        .expect_err("startup cannot remove a workspace whose owner is alive");
    assert_eq!(error.code(), JobWorkspaceErrorCode::Workspace);
    assert!(workspace_root.exists());
    assert!(checkout.exists());

    drop(owner);
    let restarted = fixture.runtime();
    assert!(!workspace_root.exists());
    drop(restarted);
}

#[test]
#[cfg(feature = "test-support")]
fn failed_creation_rollback_preserves_intent_until_exact_restart_converges() {
    for (name, failure) in [
        ("remove", WorkspaceCreationRollbackFailure::WorktreeRemoval),
        ("prune", WorkspaceCreationRollbackFailure::Prune),
        (
            "partial-root",
            WorkspaceCreationRollbackFailure::RootRemovalAfterManifest,
        ),
    ] {
        let fixture = Fixture::new(&format!("creation-rollback-{name}"));
        let active = active_job();
        let resolved_source_commit = git_output(&fixture.repository(), &["rev-parse", "HEAD"]);
        let mut failed = fixture.runtime();
        failed
            .fail_workspace_creation_rollback_for_test(&active, failure)
            .expect_err("normal creation error must expose incomplete rollback");
        assert!(creation_intent_count(&fixture.workspaces) > 0);
        drop(failed);
        std::fs::write(
            fixture.repository().join(format!("advanced-{name}.txt")),
            b"new source\n",
        )
        .expect("advance source after failed rollback");
        git(&fixture.repository(), &["add", "."]);
        git(&fixture.repository(), &["commit", "-qm", "advance source"]);

        let mut restarted = fixture.runtime();
        let checkout = restarted
            .open_for_job(&active, None)
            .expect("restart uses retained intent to converge");
        assert_eq!(creation_intent_count(&fixture.workspaces), 0);
        assert_eq!(workspace_directory_count(&fixture.workspaces), 1);
        assert_eq!(
            git_output(&checkout, &["rev-parse", "HEAD"]),
            resolved_source_commit
        );
        let worktrees = git_output(&fixture.repository(), &["worktree", "list", "--porcelain"]);
        assert_eq!(
            worktrees
                .matches(checkout.to_string_lossy().as_ref())
                .count(),
            1
        );
        restarted
            .close_job(&active.job.job_id, WorkspaceCloseReason::Cancelled)
            .expect("close converged workspace");
    }
}

#[cfg(feature = "test-support")]
fn workspace_directory_count(root: &std::path::Path) -> usize {
    std::fs::read_dir(root)
        .expect("read workspace root")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .count()
}

#[cfg(feature = "test-support")]
fn creation_intent_count(root: &std::path::Path) -> usize {
    std::fs::read_dir(root)
        .expect("read workspace root")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(".winwincode-workspace-create-"))
        })
        .count()
}

#[cfg(feature = "test-support")]
fn cleanup_intent_count(root: &std::path::Path) -> usize {
    std::fs::read_dir(root)
        .expect("read workspace root")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(".winwincode-workspace-clean-"))
        })
        .count()
}

#[cfg(feature = "test-support")]
fn workspace_manifest_bytes(workspace_root: &std::path::Path) -> Vec<u8> {
    std::fs::read(workspace_root.join(".winwincode-workspace.json"))
        .expect("read workspace manifest")
}

#[cfg(feature = "test-support")]
fn workspace_intent_path(
    manager_root: &std::path::Path,
    workspace_root: &std::path::Path,
    kind: &str,
) -> PathBuf {
    let workspace_id = workspace_root
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .expect("workspace identity");
    manager_root.join(format!(".winwincode-workspace-{kind}-{workspace_id}.json"))
}

#[cfg(feature = "test-support")]
fn manifest_with_phase(bytes: &[u8], from: &str, to: &str) -> Vec<u8> {
    let text = String::from_utf8(bytes.to_vec()).expect("manifest UTF-8");
    let from = format!("\"phase\":\"{from}\"");
    let to = format!("\"phase\":\"{to}\"");
    let replaced = text.replacen(&from, &to, 1);
    assert_ne!(replaced, text, "manifest phase must be present");
    replaced.into_bytes()
}

#[cfg(feature = "test-support")]
fn replace_current_job_digest(bytes: &[u8]) -> Vec<u8> {
    let mut text = String::from_utf8(bytes.to_vec()).expect("manifest UTF-8");
    let current = text
        .find("\"currentProvenance\":{")
        .expect("current provenance");
    let field = "\"executionJobDigest\":\"";
    let value_start =
        current + text[current..].find(field).expect("current Job digest") + field.len();
    let value_end = value_start
        + text[value_start..]
            .find('"')
            .expect("current Job digest end");
    text.replace_range(
        value_start..value_end,
        &format!("sha256:{}", "b".repeat(64)),
    );
    text.into_bytes()
}

#[test]
fn crash_recovery_rejects_a_changed_execution_job_body() {
    let fixture = Fixture::new("changed-job");
    let active = active_job();
    let mut first = fixture.runtime();
    let checkout = first
        .open_for_job(&active, None)
        .expect("create Job checkout");
    drop(first);

    let mut changed = active.clone();
    changed.job.goal = "Different sealed goal".to_owned();
    let mut restarted = fixture.runtime();
    let error = restarted
        .open_for_job(&changed, None)
        .expect_err("changed Job body must not recover the checkout");
    assert_eq!(error.code(), JobWorkspaceErrorCode::Workspace);
    assert_eq!(
        restarted
            .open_for_job(&active, None)
            .expect("original Job authority still recovers"),
        checkout
    );
    restarted
        .close_job(&active.job.job_id, WorkspaceCloseReason::Cancelled)
        .expect("remove original Job workspace");
}

#[test]
fn duplicate_authority_is_stable_and_foreign_authority_is_rejected() {
    let fixture = Fixture::new("authority");
    let active = active_job();
    let mut runtime = fixture.runtime();
    let first = runtime
        .open_for_job(&active, None)
        .expect("create Job checkout");
    let duplicate = runtime
        .open_for_job(&active, None)
        .expect("reuse exact Job checkout");
    assert_eq!(duplicate, first);

    let mut foreign = active.clone();
    foreign.lease.fencing_token = FencingToken("2".to_owned());
    let error = runtime
        .open_for_job(&foreign, None)
        .expect_err("foreign authority must not reuse checkout");
    assert_eq!(error.code(), JobWorkspaceErrorCode::AuthorityMismatch);
    runtime
        .close_job(&active.job.job_id, WorkspaceCloseReason::Cancelled)
        .expect("cancel Job workspace");
    assert!(!first.parent().expect("workspace root").exists());
}

#[test]
fn exact_checkout_runs_change_batch_recovery_before_becoming_active() {
    let fixture = Fixture::new("change-batch-recovery");
    let active = active_job();
    let source_commit = git_output(&fixture.repository(), &["rev-parse", "HEAD^{commit}"]);
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = fixture
        .runtime()
        .with_change_batch_recovery(RecordingRecovery {
            observed: Arc::clone(&observed),
        });
    runtime
        .open_for_job(&active, None)
        .expect("open checkout after recovery");
    assert_eq!(
        *observed.lock().expect("recovery observations"),
        vec![(active.job.job_id.clone(), source_commit)]
    );
    runtime
        .open_for_job(&active, None)
        .expect("duplicate open does not rerun recovery");
    assert_eq!(observed.lock().expect("recovery observations").len(), 1);
    runtime
        .close_job(&active.job.job_id, WorkspaceCloseReason::Cancelled)
        .expect("close recovered checkout");
}

#[tokio::test]
async fn production_change_batch_rejects_stale_lease_and_revision_before_any_write() {
    let fixture = Fixture::new("batch-real-stale-authority");
    let active = active_job();
    let proposal = batch_proposal(&active, source_revision(&fixture));
    let mut runtime = fixture.runtime();
    let checkout = runtime
        .open_for_job(&active, None)
        .expect("open stale authority workspace");
    let mut stale_lease = active.clone();
    stale_lease.lease.fencing_token = FencingToken("9".to_owned());
    assert_eq!(
        runtime
            .execute_change_batch(
                &stale_lease,
                &proposal,
                &Instant("2026-08-28T00:00:02.000Z".to_owned()),
            )
            .await
            .expect_err("stale lease rejected")
            .code(),
        JobWorkspaceErrorCode::AuthorityMismatch
    );
    let mut stale_revision = proposal.clone();
    stale_revision.identity.workspace_revision =
        WorkspaceRevision(format!("git-tree:{}", "f".repeat(40)));
    assert_eq!(
        runtime
            .execute_change_batch(
                &active,
                &stale_revision,
                &Instant("2026-08-28T00:00:03.000Z".to_owned()),
            )
            .await
            .expect_err("stale revision rejected")
            .code(),
        JobWorkspaceErrorCode::AuthorityMismatch
    );
    assert!(!checkout.join("delegated.txt").exists());
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn production_recovery_quarantines_other_state_and_preserves_its_bytes() {
    let fixture = Fixture::new("batch-real-other-recovery");
    let active = active_job();
    let proposal = batch_proposal(&active, source_revision(&fixture));
    let mut first = fixture.runtime();
    let checkout = first
        .open_for_job(&active, None)
        .expect("open other-state workspace");
    first
        .interrupt_change_batch_after_mutation_for_test(
            &active,
            &proposal,
            &Instant("2026-08-28T00:00:02.000Z".to_owned()),
        )
        .await
        .expect("interrupt after apply");
    std::fs::write(checkout.join("delegated.txt"), b"foreign\n").expect("write other state");
    drop(first);

    let mut restarted = fixture.runtime();
    assert_eq!(
        restarted
            .open_for_job_recovering(
                &active,
                None,
                &Instant("2026-08-28T00:00:03.000Z".to_owned()),
            )
            .await
            .expect_err("other state stays quarantined")
            .code(),
        JobWorkspaceErrorCode::ChangeBatch
    );
    assert_eq!(
        std::fs::read(checkout.join("delegated.txt")).unwrap(),
        b"foreign\n"
    );
    let journal = ChangeBatchJournal::open(fixture.root.join(".workspaces-change-batches"))
        .expect("open recovery journal");
    let record = journal
        .load(&proposal.identity.batch_id)
        .expect("load quarantined record")
        .expect("quarantined record exists");
    assert_eq!(
        record.receipt.map(|receipt| receipt.status),
        Some(ChangeBatchReceiptStatus::StateUncertain)
    );
    assert_eq!(
        journal
            .progress_events(&proposal.identity.batch_id)
            .expect("load recovery progress")
            .last()
            .map(|event| &event.state),
        Some(&ChangeBatchProgressState::InfrastructureFailed)
    );
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn production_recovery_terminalizes_full_expected_state_without_rewriting() {
    let fixture = Fixture::new("batch-real-full-recovery");
    let active = active_job();
    let proposal = batch_proposal(&active, source_revision(&fixture));
    let mut first = fixture.runtime();
    let checkout = first
        .open_for_job(&active, None)
        .expect("open interrupted workspace");
    let interrupted = first
        .interrupt_change_batch_after_mutation_for_test(
            &active,
            &proposal,
            &Instant("2026-08-28T00:00:02.000Z".to_owned()),
        )
        .await
        .expect("interrupt after exact apply");
    assert!(matches!(
        interrupted,
        ChangeBatchExecutionResult::Applied { .. }
    ));
    assert_eq!(
        std::fs::read(checkout.join("delegated.txt")).unwrap(),
        b"fixture\n"
    );
    drop(first);

    let mut restarted = fixture.runtime();
    restarted
        .open_for_job_recovering(
            &active,
            None,
            &Instant("2026-08-28T00:00:03.000Z".to_owned()),
        )
        .await
        .expect("reconcile full expected state");
    let replay = restarted
        .execute_change_batch(
            &active,
            &proposal,
            &Instant("2026-08-28T00:00:04.000Z".to_owned()),
        )
        .await
        .expect("replay recovered receipt");
    assert!(replay.replayed);
    assert_eq!(replay.receipt.status, ChangeBatchReceiptStatus::Applied);
    assert!(replay.receipt.result_revision.is_some());
    assert_eq!(
        std::fs::read(checkout.join("delegated.txt")).unwrap(),
        b"fixture\n"
    );
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn production_recovery_rolls_back_a_mixed_move_without_overwriting_other_state() {
    let fixture = Fixture::new("batch-real-move-recovery");
    std::fs::write(fixture.repository().join("move.txt"), b"move\n").expect("write move fixture");
    git(&fixture.repository(), &["add", "move.txt"]);
    git(&fixture.repository(), &["commit", "-qm", "move fixture"]);
    let active = active_job();
    let patch = "*** Begin Patch\n*** Update File: move.txt\n*** Move to: moved.txt\n@@\n-move\n+moved\n*** End Patch\n";
    let proposal =
        batch_proposal_with_patch(&active, source_revision(&fixture), patch, "turn-mixed-move");
    let mut first = fixture.runtime();
    let checkout = first
        .open_for_job(&active, None)
        .expect("open mixed move workspace");
    first
        .interrupt_change_batch_after_mutation_for_test(
            &active,
            &proposal,
            &Instant("2026-08-28T00:00:02.000Z".to_owned()),
        )
        .await
        .expect("interrupt after move");
    std::fs::write(checkout.join("move.txt"), b"move\n").expect("create mixed before state");
    drop(first);

    let mut restarted = fixture.runtime();
    restarted
        .open_for_job_recovering(
            &active,
            None,
            &Instant("2026-08-28T00:00:03.000Z".to_owned()),
        )
        .await
        .expect("rollback mixed move");
    let replay = restarted
        .execute_change_batch(
            &active,
            &proposal,
            &Instant("2026-08-28T00:00:04.000Z".to_owned()),
        )
        .await
        .expect("replay rollback receipt");
    assert_eq!(replay.receipt.status, ChangeBatchReceiptStatus::Rejected);
    assert_eq!(std::fs::read(checkout.join("move.txt")).unwrap(), b"move\n");
    assert!(!checkout.join("moved.txt").exists());
    assert_eq!(
        replay.progress.last().map(|event| &event.state),
        Some(&ChangeBatchProgressState::RepairRequired)
    );
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn production_cancel_after_apply_started_reports_the_proven_exact_state() {
    let fixture = Fixture::new("batch-real-cancel-recovery");
    let mut active = active_job();
    let proposal = batch_proposal(&active, source_revision(&fixture));
    let mut runtime = fixture.runtime();
    let checkout = runtime
        .open_for_job(&active, None)
        .expect("open cancellation recovery workspace");
    runtime
        .interrupt_change_batch_after_mutation_for_test(
            &active,
            &proposal,
            &Instant("2026-08-28T00:00:02.000Z".to_owned()),
        )
        .await
        .expect("interrupt after apply");
    active.lifecycle = ActiveJobLifecycle::Cancelling;

    let recovered = runtime
        .execute_change_batch(
            &active,
            &proposal,
            &Instant("2026-08-28T00:00:03.000Z".to_owned()),
        )
        .await
        .expect("cancel reconciles durable apply");

    assert_eq!(recovered.receipt.status, ChangeBatchReceiptStatus::Applied);
    assert!(recovered.receipt.delta_exact);
    assert_eq!(
        std::fs::read(checkout.join("delegated.txt")).unwrap(),
        b"fixture\n"
    );
}

#[tokio::test]
async fn production_change_batch_applies_add_update_delete_and_move_exactly() {
    let fixture = Fixture::new("batch-real-operations");
    std::fs::write(fixture.repository().join("delete.txt"), b"delete\n")
        .expect("write delete fixture");
    std::fs::write(fixture.repository().join("move.txt"), b"move\n").expect("write move fixture");
    git(&fixture.repository(), &["add", "delete.txt", "move.txt"]);
    git(
        &fixture.repository(),
        &["commit", "-qm", "operation fixtures"],
    );
    let base_revision = source_revision(&fixture);
    let active = active_job();
    let patch = "*** Begin Patch\n*** Add File: added.txt\n+added\n*** Update File: fixture.txt\n@@\n-source\n+updated\n*** Delete File: delete.txt\n*** Update File: move.txt\n*** Move to: moved.txt\n@@\n-move\n+moved\n*** End Patch\n";
    let proposal = batch_proposal_with_patch(
        &active,
        source_revision(&fixture),
        patch,
        "turn-real-operations",
    );
    let mut runtime = fixture.runtime();
    let checkout = runtime
        .open_for_job(&active, None)
        .expect("open real operation workspace");

    let executed = runtime
        .execute_change_batch(
            &active,
            &proposal,
            &Instant("2026-08-28T00:00:02.000Z".to_owned()),
        )
        .await
        .expect("execute real operation batch");

    assert_eq!(executed.receipt.status, ChangeBatchReceiptStatus::Applied);
    assert_eq!(executed.receipt.base_revision, base_revision);
    assert!(executed.receipt.result_revision.is_some());
    assert!(executed.receipt.delta_exact);
    assert!(executed.receipt.delta_digest.is_some());
    assert_eq!(executed.receipt.files.len(), 4);
    let receipt_json = serde_json::to_string(&executed.receipt).expect("encode exact receipt");
    assert!(!receipt_json.contains("source\\n"));
    assert!(!receipt_json.contains("updated\\n"));
    assert!(!receipt_json.contains("added\\n"));
    assert_eq!(
        std::fs::read(checkout.join("added.txt")).unwrap(),
        b"added\n"
    );
    assert_eq!(
        std::fs::read(checkout.join("fixture.txt")).unwrap(),
        b"updated\n"
    );
    assert!(!checkout.join("delete.txt").exists());
    assert!(!checkout.join("move.txt").exists());
    assert_eq!(
        std::fs::read(checkout.join("moved.txt")).unwrap(),
        b"moved\n"
    );
    assert!(runtime.contains(&active.job.job_id));
}

#[tokio::test]
async fn explicit_writer_and_validation_run_inside_one_revision_bound_barrier() {
    let fixture = Fixture::new("configured-writer-validation");
    let configuration = VALIDATION_CONFIG
        .replacen(
            "allowedCompanionPaths = []",
            "allowedCompanionPaths = [\"format-count.txt\"]",
            1,
        )
        .replace(
            "p.write_text(p.read_text().replace(\"fixture\", \"formatted\"))",
            "p.write_text(p.read_text().replace(\"fixture\", \"formatted\")); c=Path(\"format-count.txt\"); c.write_text(str(int(c.read_text())+1) if c.exists() else \"1\")",
        );
    fixture.install_validation_config_text(&configuration);
    let active = active_job();
    let mut runtime = fixture.runtime();
    let checkout = runtime
        .open_for_job(&active, None)
        .expect("open configured workspace");
    let proposal = batch_proposal(&active, source_revision(&fixture));
    let executed = runtime
        .execute_change_batch(
            &active,
            &proposal,
            &Instant("2026-08-28T00:00:02.000Z".to_owned()),
        )
        .await
        .expect("execute configured batch");

    assert_eq!(
        std::fs::read_to_string(checkout.join("delegated.txt")).expect("normalized file"),
        "formatted\n"
    );
    let normalizer = executed
        .receipt
        .normalizer
        .as_ref()
        .expect("normalizer receipt");
    assert_eq!(
        normalizer.status,
        winwincode_execution_port::generated::NormalizerReceiptStatus::Normalized
    );
    assert_ne!(
        normalizer.base_revision,
        executed.receipt.result_revision.clone().expect("result")
    );
    let validation = executed
        .receipt
        .validation
        .as_ref()
        .expect("validation receipt");
    assert_eq!(
        validation.status,
        winwincode_execution_port::generated::ValidationReceiptStatus::Passed
    );
    assert_eq!(validation.result_revision, executed.receipt.result_revision);
    assert_eq!(
        executed
            .progress
            .iter()
            .map(|event| &event.state)
            .collect::<Vec<_>>(),
        [
            &ChangeBatchProgressState::Proposed,
            &ChangeBatchProgressState::Authorized,
            &ChangeBatchProgressState::ApplyStarted,
            &ChangeBatchProgressState::Applied,
            &ChangeBatchProgressState::ValidationStarted,
            &ChangeBatchProgressState::ValidationCompleted,
            &ChangeBatchProgressState::ObservationRequested,
        ]
    );
    assert_eq!(
        std::fs::read_to_string(checkout.join("format-count.txt")).expect("formatter count"),
        "1"
    );
    let first_receipt = executed.receipt;
    drop(runtime);

    let mut restarted = fixture.runtime();
    restarted
        .open_for_job_recovering(
            &active,
            None,
            &Instant("2026-08-28T00:00:03.000Z".to_owned()),
        )
        .await
        .expect("reopen configured checkpoint");
    let replay = restarted
        .execute_change_batch(
            &active,
            &proposal,
            &Instant("2026-08-28T00:00:04.000Z".to_owned()),
        )
        .await
        .expect("replay configured checkpoint");
    assert!(replay.replayed);
    assert_eq!(replay.receipt, first_receipt);
    assert_eq!(
        std::fs::read_to_string(checkout.join("format-count.txt")).expect("replayed count"),
        "1"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn diagnostic_baseline_does_not_blame_history_then_routes_one_new_missing_module() {
    let fixture = Fixture::new("diagnostic-baseline-routing");
    let diagnostic_command = r"from pathlib import Path; module='new-module' if Path('second.txt').exists() else 'existing-module'; print(f'delegated.txt(1,1): error TS2307: Cannot find module {module!r}.'); raise SystemExit(1)";
    let configuration = VALIDATION_CONFIG.replace(
            "id = \"python-check\"\nphase = \"validation\"\nlanguage = \"python\"\nallowedCompanionPaths = []\nargv = [\"/usr/bin/python3\", \"-B\", \"-c\", 'from pathlib import Path; assert Path(\"delegated.txt\").read_text() == \"formatted\\n\"']",
            &format!(
                "id = \"python-check\"\nphase = \"validation\"\nlanguage = \"typescript\"\ndiagnosticParserVersion = \"typescript_v1\"\nallowedCompanionPaths = []\nargv = [\"/usr/bin/python3\", \"-B\", \"-c\", {diagnostic_command:?}]"
            ),
        );
    fixture.install_validation_config_text(&configuration);
    let active = active_job();
    let mut runtime = fixture.runtime();
    runtime
        .open_for_job(&active, None)
        .expect("open diagnostic workspace");
    let base = source_revision(&fixture);
    let first_proposal = batch_proposal(&active, base);
    let first = runtime
        .execute_change_batch(
            &active,
            &first_proposal,
            &Instant("2026-08-28T00:00:02.000Z".to_owned()),
        )
        .await
        .expect("validate first diagnostic baseline");
    assert_eq!(
        first.progress.last().map(|event| &event.state),
        Some(&ChangeBatchProgressState::ObservationRequested),
        "an absent baseline must not blame an existing diagnostic on this batch"
    );
    let validation = first
        .receipt
        .validation
        .as_ref()
        .expect("validation receipt");
    assert_eq!(validation.artifact_refs.len(), 2);
    let result_revision = first.receipt.result_revision.clone().expect("result tree");
    let delta_digest = first.receipt.delta_digest.clone().expect("exact delta");
    runtime
        .record_checkpoint_progress(
            &active,
            &ChangeBatchProgressEvent {
                artifact_refs: Vec::new(),
                identity: first_proposal.identity.clone(),
                occurred_at: Instant("2026-08-28T00:00:08.000Z".to_owned()),
                sequence: 8,
                state: ChangeBatchProgressState::ObservationCompleted,
                summary: "diagnostic baseline observation completed".to_owned(),
            },
        )
        .expect("complete first observation");
    let accepted = ChangeBatchProgressEvent {
        artifact_refs: Vec::new(),
        identity: first_proposal.identity,
        occurred_at: Instant("2026-08-28T00:00:09.000Z".to_owned()),
        sequence: 9,
        state: ChangeBatchProgressState::Accepted,
        summary: "diagnostic baseline accepted".to_owned(),
    };
    assert_eq!(
        runtime
            .accept_observed_checkpoint(&active, &accepted, &result_revision, &delta_digest)
            .expect("accept first diagnostic baseline"),
        ObservationGateResult::Accepted
    );
    let second = batch_proposal_with_patch(
        &active,
        result_revision,
        "*** Begin Patch\n*** Add File: second.txt\n+second\n*** End Patch\n",
        "turn-new-diagnostic",
    );
    let executed = runtime
        .execute_change_batch(
            &active,
            &second,
            &Instant("2026-08-28T00:00:10.000Z".to_owned()),
        )
        .await
        .expect("compare new missing-module diagnostic");
    assert_eq!(
        executed.progress.last().map(|event| &event.state),
        Some(&ChangeBatchProgressState::RepairRequired)
    );
}

#[tokio::test]
async fn failed_parser_command_does_not_skip_the_remaining_profile_snapshot() {
    let fixture = Fixture::new("diagnostic-complete-profile");
    let typescript = r"print('delegated.txt(1,1): error TS2307: Cannot find module \'existing\'.'); raise SystemExit(1)";
    let cargo = r#"print('{"reason":"build-finished","success":true}')"#;
    let configuration = VALIDATION_CONFIG
        .replace(
            "id = \"python-check\"\nphase = \"validation\"\nlanguage = \"python\"\nallowedCompanionPaths = []\nargv = [\"/usr/bin/python3\", \"-B\", \"-c\", 'from pathlib import Path; assert Path(\"delegated.txt\").read_text() == \"formatted\\n\"']",
            &format!(
                "id = \"python-check\"\nphase = \"validation\"\nlanguage = \"typescript\"\ndiagnosticParserVersion = \"typescript_v1\"\nallowedCompanionPaths = []\nargv = [\"/usr/bin/python3\", \"-B\", \"-c\", {typescript:?}]"
            ),
        )
        .replace(
            "id = \"rust-check\"\nphase = \"validation\"\nlanguage = \"rust\"\nallowedCompanionPaths = []\nargv = [\"/usr/bin/true\"]",
            &format!(
                "id = \"rust-check\"\nphase = \"validation\"\nlanguage = \"rust\"\ndiagnosticParserVersion = \"cargo_json_v1\"\nallowedCompanionPaths = []\nargv = [\"/usr/bin/python3\", \"-B\", \"-c\", {cargo:?}]"
            ),
        )
        .replace(
            "commandIds = [\"python-format\", \"python-check\"]",
            "commandIds = [\"python-format\", \"python-check\", \"rust-check\"]",
        );
    fixture.install_validation_config_text(&configuration);
    let active = active_job();
    let mut runtime = fixture.runtime();
    runtime
        .open_for_job(&active, None)
        .expect("open complete-profile workspace");
    let proposal = batch_proposal(&active, source_revision(&fixture));
    let executed = runtime
        .execute_change_batch(
            &active,
            &proposal,
            &Instant("2026-08-28T00:00:02.000Z".to_owned()),
        )
        .await
        .expect("retain every parser snapshot");
    let validation = executed.receipt.validation.expect("validation receipt");
    assert_eq!(validation.checks.len(), 2);
    assert_eq!(validation.artifact_refs.len(), 4);
    assert_eq!(
        executed.progress.last().map(|event| &event.state),
        Some(&ChangeBatchProgressState::ObservationRequested)
    );
}

#[tokio::test]
async fn timed_out_parser_profile_persists_infrastructure_instead_of_an_incomplete_snapshot() {
    let fixture = Fixture::new("diagnostic-timeout-profile");
    let configuration = VALIDATION_CONFIG
        .replace(
            "id = \"python-check\"\nphase = \"validation\"\nlanguage = \"python\"\nallowedCompanionPaths = []\nargv = [\"/usr/bin/python3\", \"-B\", \"-c\", 'from pathlib import Path; assert Path(\"delegated.txt\").read_text() == \"formatted\\n\"']\nworkingDirectory = \".\"\nenvironment = []\nnetwork = false\ntimeoutMillis = 300000",
            "id = \"python-check\"\nphase = \"validation\"\nlanguage = \"typescript\"\ndiagnosticParserVersion = \"typescript_v1\"\nallowedCompanionPaths = []\nargv = [\"/usr/bin/python3\", \"-B\", \"-c\", \"import time; time.sleep(1)\"]\nworkingDirectory = \".\"\nenvironment = []\nnetwork = false\ntimeoutMillis = 10",
        )
        .replace(
            "id = \"rust-check\"\nphase = \"validation\"\nlanguage = \"rust\"\nallowedCompanionPaths = []\nargv = [\"/usr/bin/true\"]",
            "id = \"rust-check\"\nphase = \"validation\"\nlanguage = \"rust\"\ndiagnosticParserVersion = \"cargo_json_v1\"\nallowedCompanionPaths = []\nargv = [\"/usr/bin/true\"]",
        )
        .replace(
            "commandIds = [\"python-format\", \"python-check\"]",
            "commandIds = [\"python-format\", \"python-check\", \"rust-check\"]",
        );
    fixture.install_validation_config_text(&configuration);
    let active = active_job();
    let mut runtime = fixture.runtime();
    runtime
        .open_for_job(&active, None)
        .expect("open timeout workspace");
    let proposal = batch_proposal(&active, source_revision(&fixture));
    let executed = runtime
        .execute_change_batch(
            &active,
            &proposal,
            &Instant("2026-08-28T00:00:02.000Z".to_owned()),
        )
        .await
        .expect("persist timeout diagnostic decision");
    let validation = executed.receipt.validation.expect("validation receipt");
    assert_eq!(
        validation.status,
        winwincode_execution_port::generated::ValidationReceiptStatus::InfrastructureError
    );
    assert_eq!(validation.checks.len(), 1);
    assert_eq!(validation.artifact_refs.len(), 2);
    assert_eq!(
        executed.progress.last().map(|event| &event.state),
        Some(&ChangeBatchProgressState::RepairRequired)
    );
}

#[tokio::test]
async fn writer_scope_violation_restores_the_exact_accepted_tree() {
    let fixture = Fixture::new("writer-scope-rollback");
    let configuration = VALIDATION_CONFIG.replace(
        "p.write_text(p.read_text().replace(\"fixture\", \"formatted\"))",
        "p.write_text(p.read_text().replace(\"fixture\", \"formatted\")); Path(\"foreign.txt\").write_text(\"foreign\")",
    );
    fixture.install_validation_config_text(&configuration);
    let active = active_job();
    let mut runtime = fixture.runtime();
    let checkout = runtime
        .open_for_job(&active, None)
        .expect("open scope fixture");
    let proposal = batch_proposal(&active, source_revision(&fixture));
    let executed = runtime
        .execute_change_batch(
            &active,
            &proposal,
            &Instant("2026-08-28T00:00:02.000Z".to_owned()),
        )
        .await
        .expect("scope violation is exact terminal fact");

    assert_eq!(executed.receipt.status, ChangeBatchReceiptStatus::Rejected);
    assert!(!checkout.join("delegated.txt").exists());
    assert!(!checkout.join("foreign.txt").exists());
    assert_eq!(
        executed.progress.last().map(|event| &event.state),
        Some(&ChangeBatchProgressState::RepairRequired)
    );
}

#[tokio::test]
async fn production_change_batch_applies_one_ten_and_twenty_files() {
    for count in [1_usize, 10, 20] {
        let fixture = Fixture::new(&format!("batch-real-{count}-files"));
        let active = active_job();
        let mut patch = String::from("*** Begin Patch\n");
        for index in 0..count {
            write!(
                patch,
                "*** Add File: generated-{index:02}.txt\n+fixture-{index:02}\n"
            )
            .expect("write bounded patch fixture");
        }
        patch.push_str("*** End Patch\n");
        let proposal = batch_proposal_with_patch(
            &active,
            source_revision(&fixture),
            &patch,
            &format!("turn-real-{count}-files"),
        );
        let mut runtime = fixture.runtime();
        let checkout = runtime
            .open_for_job(&active, None)
            .expect("open bounded real workspace");

        let executed = runtime
            .execute_change_batch(
                &active,
                &proposal,
                &Instant("2026-08-28T00:00:02.000Z".to_owned()),
            )
            .await
            .expect("execute bounded real batch");

        assert_eq!(executed.receipt.status, ChangeBatchReceiptStatus::Applied);
        assert_eq!(executed.receipt.files.len(), count);
        assert!(executed.receipt.result_revision.is_some());
        assert!(runtime.contains(&active.job.job_id));
        for index in 0..count {
            assert_eq!(
                std::fs::read(checkout.join(format!("generated-{index:02}.txt"))).unwrap(),
                format!("fixture-{index:02}\n").as_bytes()
            );
        }
    }
}

#[tokio::test]
async fn change_batch_success_replays_after_restart_without_second_executor_call() {
    let fixture = Fixture::new("batch-success-replay");
    let active = active_job();
    let proposal = batch_proposal(&active, source_revision(&fixture));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut first = fixture
        .runtime()
        .with_change_batch_executor(scripted_executor(&calls, applied_result()))
        .with_workspace_tree_port(FixedWorkspaceTreePort);
    first
        .open_for_job(&active, None)
        .expect("open batch workspace");
    let executed = first
        .execute_change_batch(
            &active,
            &proposal,
            &Instant("2026-08-28T00:00:02.000Z".to_owned()),
        )
        .await
        .expect("execute batch");
    assert!(!executed.replayed);
    assert_eq!(
        executed
            .progress
            .iter()
            .map(|event| event.state.clone())
            .collect::<Vec<_>>(),
        vec![
            ChangeBatchProgressState::Proposed,
            ChangeBatchProgressState::Authorized,
            ChangeBatchProgressState::ApplyStarted,
            ChangeBatchProgressState::Applied,
        ]
    );
    assert_eq!(executed.receipt.status, ChangeBatchReceiptStatus::Applied);
    assert!(executed.receipt.delta_exact);
    assert!(executed.receipt.delta_digest.is_some());
    assert_eq!(*calls.lock().expect("batch calls"), vec!["execute"]);
    drop(first);

    let mut restarted = fixture
        .runtime()
        .with_change_batch_executor(scripted_executor(&calls, applied_result()))
        .with_workspace_tree_port(FixedWorkspaceTreePort);
    restarted
        .open_for_job(&active, None)
        .expect("recover batch workspace");
    let replay = restarted
        .execute_change_batch(
            &active,
            &proposal,
            &Instant("2026-08-28T00:00:03.000Z".to_owned()),
        )
        .await
        .expect("replay exact batch");
    assert!(replay.replayed);
    assert_eq!(replay.progress, executed.progress);
    assert_eq!(replay.receipt, executed.receipt);
    assert_eq!(*calls.lock().expect("batch calls"), vec!["execute"]);
    restarted
        .prepare_close_job(
            &active.job.job_id,
            WorkspaceCloseReason::Completed,
            &Instant("2026-08-28T00:00:20.000Z".to_owned()),
        )
        .await
        .expect("close replayed workspace");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn checkpoint_barrier_rejects_a_second_batch_until_exact_observation_acceptance() {
    let fixture = Fixture::new("batch-checkpoint-barrier");
    let active = active_job();
    let base = source_revision(&fixture);
    let first_proposal = batch_proposal(&active, base.clone());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = fixture
        .runtime()
        .with_change_batch_executor(scripted_executor(&calls, applied_result()))
        .with_workspace_tree_port(FixedWorkspaceTreePort);
    runtime
        .open_for_job(&active, None)
        .expect("open checkpoint workspace");
    let first = runtime
        .execute_change_batch(
            &active,
            &first_proposal,
            &Instant("2026-08-28T00:00:02.000Z".to_owned()),
        )
        .await
        .expect("checkpoint first batch");
    let result_revision = first
        .receipt
        .result_revision
        .clone()
        .expect("exact checkpoint revision");
    let delta_digest = first
        .receipt
        .delta_digest
        .clone()
        .expect("exact checkpoint delta");
    let blocked = batch_proposal_with_patch(
        &active,
        base.clone(),
        "*** Begin Patch\n*** Add File: second.txt\n+second\n*** End Patch\n",
        "turn-second",
    );
    assert_eq!(
        runtime
            .execute_change_batch(
                &active,
                &blocked,
                &Instant("2026-08-28T00:00:03.000Z".to_owned()),
            )
            .await
            .expect_err("second batch is blocked")
            .code(),
        JobWorkspaceErrorCode::ChangeBatch
    );
    assert_eq!(*calls.lock().expect("batch calls"), vec!["execute"]);

    for (sequence, state) in (5_i64..).zip([
        ChangeBatchProgressState::ValidationStarted,
        ChangeBatchProgressState::ValidationCompleted,
        ChangeBatchProgressState::ObservationRequested,
        ChangeBatchProgressState::ObservationCompleted,
    ]) {
        runtime
            .record_checkpoint_progress(
                &active,
                &ChangeBatchProgressEvent {
                    artifact_refs: Vec::new(),
                    identity: first_proposal.identity.clone(),
                    occurred_at: Instant(format!("2026-08-28T00:00:0{sequence}.000Z")),
                    sequence,
                    state,
                    summary: "bounded post-apply progress".to_owned(),
                },
            )
            .expect("record post-apply progress");
    }
    let accepted = ChangeBatchProgressEvent {
        artifact_refs: Vec::new(),
        identity: first_proposal.identity.clone(),
        occurred_at: Instant("2026-08-28T00:00:09.000Z".to_owned()),
        sequence: 9,
        state: ChangeBatchProgressState::Accepted,
        summary: "exact observation accepted".to_owned(),
    };
    assert_eq!(
        runtime
            .accept_observed_checkpoint(&active, &accepted, &base, &delta_digest)
            .expect("stale observation is a typed result"),
        ObservationGateResult::Stale
    );
    assert_eq!(runtime.accepted_revision(&active.job.job_id).unwrap(), base);
    assert_eq!(
        runtime
            .accept_observed_checkpoint(&active, &accepted, &result_revision, &delta_digest,)
            .expect("accept exact observation"),
        ObservationGateResult::Accepted
    );
    assert_eq!(
        runtime.accepted_revision(&active.job.job_id).unwrap(),
        result_revision
    );
    let next = batch_proposal_with_patch(
        &active,
        result_revision,
        "*** Begin Patch\n*** Add File: second.txt\n+second\n*** End Patch\n",
        "turn-second",
    );
    runtime
        .execute_change_batch(
            &active,
            &next,
            &Instant("2026-08-28T00:00:10.000Z".to_owned()),
        )
        .await
        .expect("accepted checkpoint releases next batch");
    assert_eq!(
        *calls.lock().expect("batch calls"),
        vec!["execute", "execute"]
    );
}

#[tokio::test]
async fn prepare_close_restores_the_accepted_tree_before_removing_the_workspace() {
    let fixture = Fixture::new("batch-close-restore");
    let active = active_job();
    let proposal = batch_proposal(&active, source_revision(&fixture));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = fixture
        .runtime()
        .with_change_batch_executor(scripted_executor(&calls, applied_result()))
        .with_workspace_tree_port(FixedWorkspaceTreePort);
    runtime
        .open_for_job(&active, None)
        .expect("open close restore workspace");
    runtime
        .execute_change_batch(
            &active,
            &proposal,
            &Instant("2026-08-28T00:00:02.000Z".to_owned()),
        )
        .await
        .expect("checkpoint before close");
    runtime
        .prepare_close_job(
            &active.job.job_id,
            WorkspaceCloseReason::Completed,
            &Instant("2026-08-28T00:00:20.000Z".to_owned()),
        )
        .await
        .expect("restore accepted tree before close");
    assert!(!runtime.contains(&active.job.job_id));
    let journal = ChangeBatchJournal::open(fixture.root.join(".workspaces-change-batches"))
        .expect("open close journal");
    assert_eq!(
        journal
            .progress_events(&proposal.identity.batch_id)
            .expect("load close progress")
            .into_iter()
            .map(|event| event.state)
            .collect::<Vec<_>>(),
        vec![
            ChangeBatchProgressState::Proposed,
            ChangeBatchProgressState::Authorized,
            ChangeBatchProgressState::ApplyStarted,
            ChangeBatchProgressState::Applied,
            ChangeBatchProgressState::RollbackStarted,
            ChangeBatchProgressState::RolledBack,
            ChangeBatchProgressState::RepairRequired,
        ]
    );
}

#[tokio::test]
async fn prepare_close_quarantines_an_uncertain_restore_without_deleting_the_workspace() {
    let fixture = Fixture::new("batch-close-quarantine");
    let active = active_job();
    let proposal = batch_proposal(&active, source_revision(&fixture));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = fixture
        .runtime()
        .with_change_batch_executor(scripted_executor(&calls, applied_result()))
        .with_workspace_tree_port(UncertainWorkspaceTreePort);
    let checkout = runtime
        .open_for_job(&active, None)
        .expect("open uncertain close workspace");
    let workspace_id = checkout
        .parent()
        .and_then(std::path::Path::file_name)
        .and_then(std::ffi::OsStr::to_str)
        .expect("workspace id")
        .to_owned();
    runtime
        .execute_change_batch(
            &active,
            &proposal,
            &Instant("2026-08-28T00:00:02.000Z".to_owned()),
        )
        .await
        .expect("checkpoint before uncertain close");
    assert_eq!(
        runtime
            .prepare_close_job(
                &active.job.job_id,
                WorkspaceCloseReason::Cancelled,
                &Instant("2026-08-28T00:00:20.000Z".to_owned()),
            )
            .await
            .expect_err("uncertain restore stays quarantined")
            .code(),
        JobWorkspaceErrorCode::ChangeBatch
    );
    assert!(runtime.contains(&active.job.job_id));
    assert!(checkout.exists());
    let journal = ChangeBatchJournal::open(fixture.root.join(".workspaces-change-batches"))
        .expect("open quarantine journal");
    assert_eq!(
        journal
            .workspace_barrier(&workspace_id)
            .expect("read quarantine barrier")
            .expect("quarantine barrier")
            .state,
        ActiveBatchState::Quarantined
    );
    assert_eq!(
        journal
            .progress_events(&proposal.identity.batch_id)
            .expect("load quarantine progress")
            .last()
            .map(|event| &event.state),
        Some(&ChangeBatchProgressState::InfrastructureFailed)
    );
}

#[tokio::test]
async fn restart_finishes_a_durable_pending_accepted_tree_restore() {
    let fixture = Fixture::new("batch-restart-pending-restore");
    let active = active_job();
    let proposal = batch_proposal(&active, source_revision(&fixture));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut first = fixture
        .runtime()
        .with_change_batch_executor(scripted_executor(&calls, applied_result()))
        .with_workspace_tree_port(FixedWorkspaceTreePort);
    let checkout = first
        .open_for_job(&active, None)
        .expect("open pending restore workspace");
    let workspace_id = checkout
        .parent()
        .and_then(std::path::Path::file_name)
        .and_then(std::ffi::OsStr::to_str)
        .expect("workspace id")
        .to_owned();
    first
        .execute_change_batch(
            &active,
            &proposal,
            &Instant("2026-08-28T00:00:02.000Z".to_owned()),
        )
        .await
        .expect("checkpoint before restore interruption");
    let mut journal = ChangeBatchJournal::open(fixture.root.join(".workspaces-change-batches"))
        .expect("open interruption journal");
    journal
        .retain_workspace_progress(
            &workspace_id,
            &ChangeBatchProgressEvent {
                artifact_refs: Vec::new(),
                identity: proposal.identity.clone(),
                occurred_at: Instant("2026-08-28T00:00:03.000Z".to_owned()),
                sequence: 5,
                state: ChangeBatchProgressState::RollbackStarted,
                summary: "durable pending restore".to_owned(),
            },
            ActiveBatchState::Checkpointed,
            ActiveBatchState::RollbackPending,
        )
        .expect("persist restore-pending crash point");
    drop(journal);
    drop(first);

    let mut restarted = fixture
        .runtime()
        .with_workspace_tree_port(FixedWorkspaceTreePort);
    restarted
        .open_for_job_recovering(
            &active,
            None,
            &Instant("2026-08-28T00:00:04.000Z".to_owned()),
        )
        .await
        .expect("finish pending accepted-tree restore");
    let journal = ChangeBatchJournal::open(fixture.root.join(".workspaces-change-batches"))
        .expect("open recovered restore journal");
    assert_eq!(
        journal
            .workspace_barrier(&workspace_id)
            .expect("read recovered barrier")
            .expect("recovered barrier")
            .state,
        ActiveBatchState::RepairRequired
    );
    assert_eq!(
        journal
            .progress_events(&proposal.identity.batch_id)
            .expect("read recovered progress")
            .last()
            .map(|event| &event.state),
        Some(&ChangeBatchProgressState::RepairRequired)
    );
}

#[tokio::test]
async fn change_batch_rollback_uncertain_and_cancel_paths_are_ordered() {
    let rollback_fixture = Fixture::new("batch-rollback");
    let active = active_job();
    let proposal = batch_proposal(&active, source_revision(&rollback_fixture));
    let rollback_calls = Arc::new(Mutex::new(Vec::new()));
    let rollback = ChangeBatchExecutionResult::RolledBack { artifact_ref: None };
    let mut runtime = rollback_fixture
        .runtime()
        .with_change_batch_executor(scripted_executor(&rollback_calls, rollback))
        .with_workspace_tree_port(FixedWorkspaceTreePort);
    runtime
        .open_for_job(&active, None)
        .expect("open rollback workspace");
    let executed = runtime
        .execute_change_batch(
            &active,
            &proposal,
            &Instant("2026-08-28T00:00:02.000Z".to_owned()),
        )
        .await
        .expect("execute rollback");
    assert_eq!(
        executed
            .progress
            .iter()
            .map(|event| event.state.clone())
            .collect::<Vec<_>>(),
        vec![
            ChangeBatchProgressState::Proposed,
            ChangeBatchProgressState::Authorized,
            ChangeBatchProgressState::ApplyStarted,
            ChangeBatchProgressState::RollbackStarted,
            ChangeBatchProgressState::RolledBack,
            ChangeBatchProgressState::RepairRequired,
        ]
    );
    assert_eq!(executed.receipt.status, ChangeBatchReceiptStatus::Rejected);
    assert_eq!(
        *rollback_calls.lock().expect("rollback calls"),
        vec!["execute"]
    );

    let cancel_fixture = Fixture::new("batch-cancel");
    let mut cancelling = active_job();
    cancelling.lifecycle = ActiveJobLifecycle::Cancelling;
    let cancel_proposal = batch_proposal(&cancelling, source_revision(&cancel_fixture));
    let cancel_calls = Arc::new(Mutex::new(Vec::new()));
    let mut cancelled = cancel_fixture
        .runtime()
        .with_change_batch_executor(scripted_executor(
            &cancel_calls,
            ChangeBatchExecutionResult::RolledBack { artifact_ref: None },
        ))
        .with_workspace_tree_port(FixedWorkspaceTreePort);
    cancelled
        .open_for_job(&cancelling, None)
        .expect("open cancelling workspace");
    let cancelled_result = cancelled
        .execute_change_batch(
            &cancelling,
            &cancel_proposal,
            &Instant("2026-08-28T00:00:02.000Z".to_owned()),
        )
        .await
        .expect("cancel through rollback");
    assert_eq!(*cancel_calls.lock().expect("cancel calls"), vec!["cancel"]);
    assert_eq!(
        cancelled_result
            .progress
            .iter()
            .map(|event| event.state.clone())
            .collect::<Vec<_>>(),
        vec![
            ChangeBatchProgressState::Proposed,
            ChangeBatchProgressState::Authorized,
            ChangeBatchProgressState::ApplyStarted,
            ChangeBatchProgressState::RollbackStarted,
            ChangeBatchProgressState::RolledBack,
            ChangeBatchProgressState::RepairRequired,
        ]
    );
}

#[tokio::test]
async fn change_batch_uncertain_state_is_terminalized_without_an_exact_delta() {
    let fixture = Fixture::new("batch-uncertain");
    let active = active_job();
    let proposal = batch_proposal(&active, source_revision(&fixture));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = fixture
        .runtime()
        .with_change_batch_executor(scripted_executor(
            &calls,
            ChangeBatchExecutionResult::StateUncertain {
                files: Vec::new(),
                artifact_ref: None,
            },
        ))
        .with_workspace_tree_port(FixedWorkspaceTreePort);
    runtime
        .open_for_job(&active, None)
        .expect("open uncertain workspace");
    let failed = runtime
        .execute_change_batch(
            &active,
            &proposal,
            &Instant("2026-08-28T00:00:02.000Z".to_owned()),
        )
        .await
        .expect("executor reports uncertain state");
    assert_eq!(
        failed.progress.last().map(|event| &event.state),
        Some(&ChangeBatchProgressState::InfrastructureFailed)
    );
    assert_eq!(
        failed.receipt.status,
        ChangeBatchReceiptStatus::StateUncertain
    );
    assert!(!failed.receipt.delta_exact);
}

#[tokio::test]
async fn exact_partial_result_uses_the_actual_checkpoint_tree_and_quarantines() {
    let fixture = Fixture::new("batch-exact-partial");
    let active = active_job();
    let proposal = batch_proposal(&active, source_revision(&fixture));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let ChangeBatchExecutionResult::Applied { files, .. } = applied_result() else {
        unreachable!("applied fixture")
    };
    let mut runtime = fixture
        .runtime()
        .with_change_batch_executor(scripted_executor(
            &calls,
            ChangeBatchExecutionResult::PartiallyApplied {
                files,
                artifact_ref: None,
            },
        ))
        .with_workspace_tree_port(FixedWorkspaceTreePort);
    let checkout = runtime
        .open_for_job(&active, None)
        .expect("open exact partial workspace");
    let workspace_id = checkout
        .parent()
        .and_then(std::path::Path::file_name)
        .and_then(std::ffi::OsStr::to_str)
        .expect("workspace id")
        .to_owned();
    let executed = runtime
        .execute_change_batch(
            &active,
            &proposal,
            &Instant("2026-08-28T00:00:02.000Z".to_owned()),
        )
        .await
        .expect("retain exact partial result");
    assert_eq!(
        executed.receipt.status,
        ChangeBatchReceiptStatus::PartiallyApplied
    );
    assert_eq!(
        executed.receipt.result_revision,
        Some(WorkspaceRevision(format!("git-tree:{}", "e".repeat(40))))
    );
    assert!(executed.receipt.delta_exact);
    assert!(executed.receipt.delta_digest.is_some());
    let journal = ChangeBatchJournal::open(fixture.root.join(".workspaces-change-batches"))
        .expect("open partial journal");
    assert_eq!(
        journal
            .workspace_barrier(&workspace_id)
            .expect("read partial barrier")
            .expect("partial barrier")
            .state,
        ActiveBatchState::Quarantined
    );
}

#[tokio::test]
async fn change_batch_changed_intent_and_foreign_authority_never_reinvoke_executor() {
    let fixture = Fixture::new("batch-conflict");
    let active = active_job();
    let proposal = batch_proposal(&active, source_revision(&fixture));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = fixture
        .runtime()
        .with_change_batch_executor(scripted_executor(&calls, applied_result()))
        .with_workspace_tree_port(FixedWorkspaceTreePort);
    runtime
        .open_for_job(&active, None)
        .expect("open batch workspace");
    runtime
        .execute_change_batch(
            &active,
            &proposal,
            &Instant("2026-08-28T00:00:02.000Z".to_owned()),
        )
        .await
        .expect("execute exact intent");

    let mut changed = proposal.clone();
    changed.occurred_at = Instant("2026-08-28T00:00:09.000Z".to_owned());
    assert_eq!(
        runtime
            .execute_change_batch(
                &active,
                &changed,
                &Instant("2026-08-28T00:00:10.000Z".to_owned()),
            )
            .await
            .expect_err("same batch changed bytes conflict")
            .code(),
        JobWorkspaceErrorCode::ChangeBatch
    );
    let mut foreign = active.clone();
    foreign.lease.fencing_token = FencingToken("9".to_owned());
    assert_eq!(
        runtime
            .execute_change_batch(
                &foreign,
                &proposal,
                &Instant("2026-08-28T00:00:10.000Z".to_owned()),
            )
            .await
            .expect_err("foreign authority rejected")
            .code(),
        JobWorkspaceErrorCode::AuthorityMismatch
    );
    assert_eq!(*calls.lock().expect("batch calls"), vec!["execute"]);
}

#[test]
fn unchanged_or_cancelled_writer_produces_no_candidate() {
    let fixture = Fixture::new("fail-closed");
    let mut active = active_job();
    let mut runtime = fixture.runtime();
    runtime
        .open_for_job(&active, None)
        .expect("create unchanged Job checkout");
    let unchanged = runtime
        .prepare_candidate(&active)
        .expect_err("unchanged checkout has no candidate");
    assert_eq!(unchanged.code(), JobWorkspaceErrorCode::Candidate);
    active.lifecycle = ActiveJobLifecycle::Cancelling;
    let cancelling = runtime
        .prepare_candidate(&active)
        .expect_err("cancelling Job has no candidate");
    assert_eq!(cancelling.code(), JobWorkspaceErrorCode::Candidate);
    runtime
        .close_job(&active.job.job_id, WorkspaceCloseReason::Cancelled)
        .expect("remove cancelled workspace");
}
