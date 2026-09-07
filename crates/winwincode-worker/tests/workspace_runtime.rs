// SPDX-License-Identifier: Apache-2.0

use std::{collections::HashMap, path::PathBuf, process::Command};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use winwincode_domain::{
    ArtifactId, CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionAckSequence, ExecutionJobId,
    ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId, ProductSessionId,
    RepositoryId, RequestId, SchemaVersion, SessionIdentity, Sha256Digest, StageRunId, WorkerId,
    WorkerInstanceId, WorkerSessionId, WorkspaceRevision,
};
use winwincode_execution_port::{
    change_batch_identity::derive_change_batch_id,
    generated::{
        AppliedFileOperation, AppliedFileSummary, ArtifactReference, ChangeBatchIdentity,
        ChangeBatchProgressEvent, ChangeBatchProgressState, ChangeBatchProposal,
        ChangeBatchProposalDisposition, ChangeBatchProposalEvent,
        DeliveryStageAcceptanceCriterionInput, DeliveryStageExecutionScope,
        DeliveryStageExecutionScopeKind, DeliveryStageInput, DeliveryStageTaskInput,
        EncodedPayload, ExecutionJob, ExecutionJobReplacementAuthority, ExecutionLeaseStamp,
        ExecutionLimits, ExecutionOutcomeUsage, ExecutionScope, ExecutionWorkspace,
        ExecutionWorkspaceWriteMode, ModelChunkMessage, ModelChunkMessageKind, ModelGatewayRoute,
        ModelOpenMessage, ObservationReceipt, ObservationSource, ValidationProfileName,
    },
    observation_contract::{derive_observation_output_digest, parse_observation_response_strict},
};
use winwincode_worker::{
    ActiveJob, ActiveJobLifecycle, CodexRunKey,
    change_batch_store::{
        BatchState, ChangeBatchStore, ObservationChunkRetention, ObservationModelFrame,
    },
    workspace::WorkspaceCloseReason,
    workspace_runtime::{
        ChangeBatchExecutionRequest, ChangeBatchExecutionResult, ChangeBatchExecutor,
        ChangeBatchExecutorFuture, JobWorkspaceErrorCode, JobWorkspaceRuntime,
        ObservationModelConfiguration, ValidationArtifactError, ValidationArtifactPort,
        ValidationArtifactRequest, ValidationArtifactStream,
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
            .with_change_batch_executor(PatchApplyingExecutor)
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

fn baseline_unavailable_validation_config() -> String {
    let diagnostic_command = r"from pathlib import Path; module='new-module' if Path('second.txt').exists() else 'existing-module'; print(f'delegated.txt(1,1): error TS2307: Cannot find module {module!r}.'); raise SystemExit(1)";
    VALIDATION_CONFIG.replace(
        "id = \"python-check\"\nphase = \"validation\"\nlanguage = \"python\"\nallowedCompanionPaths = []\nargv = [\"/usr/bin/python3\", \"-B\", \"-c\", 'from pathlib import Path; assert Path(\"delegated.txt\").read_text() == \"formatted\\n\"']",
        &format!(
            "id = \"python-check\"\nphase = \"validation\"\nlanguage = \"typescript\"\ndiagnosticParserVersion = \"typescript_v1\"\nallowedCompanionPaths = []\nargv = [\"/usr/bin/python3\", \"-B\", \"-c\", {diagnostic_command:?}]"
        ),
    )
}

/// Applies the exact Add File patch bytes through the injected executor port.
///
/// This is the same port seam the production deterministic delivery installs:
/// the Worker itself never writes checkout bytes, so every re-cut fixture that
/// exercises an executed batch installs its executor here.
#[derive(Debug, Default)]
struct PatchApplyingExecutor;

impl PatchApplyingExecutor {
    fn apply(request: ChangeBatchExecutionRequest<'_>) -> ChangeBatchExecutionResult {
        let mut written: Vec<(PathBuf, Vec<u8>)> = Vec::new();
        for line in request.patch.lines() {
            if let Some(path) = line.strip_prefix("*** Add File: ") {
                written.push((request.checkout.join(path.trim()), Vec::new()));
            } else if let Some(body) = line.strip_prefix('+')
                && let Some((_, bytes)) = written.last_mut()
            {
                bytes.extend_from_slice(body.as_bytes());
                bytes.push(b'\n');
            }
        }
        let files = written
            .into_iter()
            .map(|(path, bytes)| {
                std::fs::write(&path, &bytes).expect("apply fixture patch bytes");
                AppliedFileSummary {
                    after_sha256: Some(Sha256Digest(format!(
                        "sha256:{:x}",
                        Sha256::digest(&bytes)
                    ))),
                    before_sha256: None,
                    bytes_after: i64::try_from(bytes.len()).expect("fixture byte bound"),
                    bytes_before: 0,
                    mode_after: Some("0644".to_owned()),
                    mode_before: None,
                    move_path: None,
                    operation: AppliedFileOperation::Create,
                    path: path
                        .strip_prefix(request.checkout)
                        .expect("fixture checkout path")
                        .to_string_lossy()
                        .to_string(),
                }
            })
            .collect();
        ChangeBatchExecutionResult::Applied {
            files,
            artifact_ref: None,
        }
    }
}

impl ChangeBatchExecutor for PatchApplyingExecutor {
    fn execute<'operation>(
        &'operation mut self,
        request: ChangeBatchExecutionRequest<'operation>,
    ) -> ChangeBatchExecutorFuture<'operation> {
        let result = Self::apply(request);
        Box::pin(async move { Ok(result) })
    }

    fn recover<'operation>(
        &'operation mut self,
        request: ChangeBatchExecutionRequest<'operation>,
    ) -> ChangeBatchExecutorFuture<'operation> {
        let result = Self::apply(request);
        Box::pin(async move { Ok(result) })
    }

    fn cancel<'operation>(
        &'operation mut self,
        _request: ChangeBatchExecutionRequest<'operation>,
    ) -> ChangeBatchExecutorFuture<'operation> {
        Box::pin(async { Ok(ChangeBatchExecutionResult::RolledBack { artifact_ref: None }) })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
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

fn observation_chunk(
    open: &ModelOpenMessage,
    sequence: i64,
    payload: &serde_json::Value,
    is_final: bool,
) -> ModelChunkMessage {
    let bytes = serde_json::to_vec(payload).expect("Observer chunk payload");
    ModelChunkMessage {
        error: None,
        is_final,
        kind: ModelChunkMessageKind::ModelChunk,
        lease: open.lease.clone(),
        message_id: ExecutionMessageId(format!("xmsg_{sequence:026}")),
        model_exchange_id: open.model_exchange_id.clone(),
        payload: Some(EncodedPayload {
            content_type: "application/json".to_owned(),
            data_base64: STANDARD.encode(&bytes),
            payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes))),
        }),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: open.sent_at.clone(),
        sequence: ExecutionSequence(sequence),
        session_identity: open.session_identity.clone(),
        worker_session_id: open.worker_session_id.clone(),
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

// Its only caller is the `test-support` parallel-cleaning test below; the
// default feature set would otherwise flag this helper as dead code.
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
        .prepare_candidate(&active, winwincode_codex::RoleExecutionMode::React)
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
        .prepare_candidate(&active, winwincode_codex::RoleExecutionMode::React)
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
        .prepare_candidate(&active, winwincode_codex::RoleExecutionMode::React)
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
        .prepare_candidate(&successor, winwincode_codex::RoleExecutionMode::React)
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
            .prepare_candidate(&successor, winwincode_codex::RoleExecutionMode::React)
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

async fn prepare_terminal_observer_replay_fixture()
-> (Fixture, ActiveJob, ModelOpenMessage, ModelChunkMessage) {
    let fixture = Fixture::new("observer-terminal-before-receipt");
    fixture.install_validation_config_text(&baseline_unavailable_validation_config());
    let active = active_job();
    let mut runtime = fixture.runtime();
    runtime
        .open_for_job(&active, None)
        .expect("open terminal replay workspace");
    let proposal = batch_proposal(&active, source_revision(&fixture));
    let executed = Box::pin(runtime.execute_change_batch(
        &active,
        &proposal,
        &Instant("2026-08-28T00:00:02.000Z".to_owned()),
    ))
    .await
    .expect("retain unresolved validation");
    let observation = executed
        .observation_request
        .expect("retain bounded Observer intent");
    let model = ObservationModelConfiguration::try_new(
        "observer-provider",
        "observer-model",
        ModelGatewayRoute {
            capability: "observer-strict-json".to_owned(),
            route: "enterprise-observer".to_owned(),
        },
    )
    .expect("independent Observer route");
    let open = runtime
        .prepare_observation_model_open(
            &active,
            &observation,
            &model,
            &Instant("2026-08-28T00:00:03.000Z".to_owned()),
        )
        .expect("retain Observer open");
    let response = serde_json::json!({
        "schemaVersion": 1,
        "observationId": observation.intent.observation_id.0,
        "decision": "accept",
        "reasonCode": "criteria_satisfied",
        "summary": "The bounded evidence satisfies the requested criterion.",
        "rootCauses": [],
        "repairClass": null,
        "confidenceBps": 9000
    })
    .to_string();
    runtime
        .accept_observation_model_chunk(
            &active,
            &observation_chunk(
                &open,
                1,
                &serde_json::json!({"type": "output_text_delta", "delta": response}),
                false,
            ),
            &Instant("2026-08-28T00:00:04.000Z".to_owned()),
        )
        .expect("retain Observer response")
        .expect("Observer exchange");
    let completed = observation_chunk(
        &open,
        2,
        &serde_json::json!({
            "type": "completed",
            "responseId": "response-observer-terminal-replay",
            "actualCostMicros": 53,
            "tokenUsage": {
                "input_tokens": 80,
                "cached_input_tokens": 0,
                "cache_write_input_tokens": 0,
                "output_tokens": 20,
                "reasoning_output_tokens": 0,
                "total_tokens": 100
            },
            "endTurn": true
        }),
        true,
    );
    drop(runtime);
    (fixture, active, open, completed)
}

fn retain_terminal_frame_without_receipt(
    fixture: &Fixture,
    open: &ModelOpenMessage,
    completed: &ModelChunkMessage,
) {
    let mut interrupted =
        ChangeBatchStore::open(&fixture.root).expect("open terminal replay journal");
    let completed_bytes = serde_json::to_vec(&completed).expect("encode terminal Observer frame");
    interrupted
        .retain_observation_model_chunk(
            &ObservationModelFrame {
                model_exchange_id: open.model_exchange_id.clone(),
                sequence: 2,
                chunk_digest: Sha256Digest(format!(
                    "sha256:{:x}",
                    Sha256::digest(&completed_bytes)
                )),
                response_delta: &[],
                model_usage: Some(ExecutionOutcomeUsage {
                    cost_microunits: 53,
                    runtime_millis: 0,
                    tokens: 100,
                }),
                terminal_status: Some("completed"),
            },
            &Instant("2026-08-28T00:00:05.000Z".to_owned()),
        )
        .expect("retain terminal frame without receipt");
    assert!(
        interrupted
            .observation_model_record(&open.model_exchange_id)
            .expect("load terminal record")
            .expect("terminal record exists")
            .receipt
            .is_none()
    );
    drop(interrupted);
}

#[tokio::test]
async fn terminal_observer_frame_replays_after_restart_before_receipt_commit() {
    let (fixture, active, open, completed) =
        Box::pin(prepare_terminal_observer_replay_fixture()).await;
    retain_terminal_frame_without_receipt(&fixture, &open, &completed);

    let mut restarted = fixture.runtime();
    restarted
        .open_for_job_recovering(
            &active,
            None,
            &Instant("2026-08-28T00:00:06.000Z".to_owned()),
        )
        .expect("reopen terminal frame before receipt");
    assert_eq!(
        restarted
            .pending_observation_model_open(&active)
            .expect("load pending terminal open"),
        Some(open.clone())
    );
    let recovered = restarted
        .accept_observation_model_chunk(
            &active,
            &completed,
            &Instant("2026-08-28T00:00:07.000Z".to_owned()),
        )
        .expect("replay terminal Observer frame")
        .expect("Observer exchange");
    assert_eq!(
        recovered
            .completed_progress
            .iter()
            .map(|event| &event.state)
            .collect::<Vec<_>>(),
        [
            &ChangeBatchProgressState::ObservationCompleted,
            &ChangeBatchProgressState::Accepted,
        ]
    );
    assert!(recovered.receipt.is_some());
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn diagnostic_baseline_does_not_blame_history_then_routes_one_new_missing_module() {
    let fixture = Fixture::new("diagnostic-baseline-routing");
    let configuration = baseline_unavailable_validation_config();
    fixture.install_validation_config_text(&configuration);
    let active = active_job();
    let mut runtime = fixture.runtime();
    runtime
        .open_for_job(&active, None)
        .expect("open diagnostic workspace");
    let base = source_revision(&fixture);
    let first_proposal = batch_proposal(&active, base);
    let first = Box::pin(runtime.execute_change_batch(
        &active,
        &first_proposal,
        &Instant("2026-08-28T00:00:02.000Z".to_owned()),
    ))
    .await
    .expect("validate first diagnostic baseline");
    assert_eq!(
        first.progress.last().map(|event| &event.state),
        Some(&ChangeBatchProgressState::ObservationRequested),
        "an absent baseline must not blame an existing diagnostic on this batch"
    );
    let observation = first
        .observation_request
        .clone()
        .expect("baseline uncertainty retains one bounded Observer intent");
    assert!(observation.one_shot);
    assert!(!observation.intent.hard_check_failed);
    assert!(observation.intent.all_checks_executed);
    assert_eq!(
        Some(&observation.intent.result_revision),
        first.receipt.result_revision.as_ref()
    );
    let observer_model = ObservationModelConfiguration::try_new(
        "observer-provider",
        "observer-model",
        ModelGatewayRoute {
            capability: "observer-strict-json".to_owned(),
            route: "enterprise-observer".to_owned(),
        },
    )
    .expect("independent Observer route");
    let opened_at = Instant("2026-08-28T00:00:07.000Z".to_owned());
    let first_open = runtime
        .prepare_observation_model_open(&active, &observation, &observer_model, &opened_at)
        .expect("retain Observer open before provider send");
    let replay_open = runtime
        .prepare_observation_model_open(&active, &observation, &observer_model, &opened_at)
        .expect("replay exact Observer open");
    assert_eq!(first_open, replay_open);
    let payload = STANDARD
        .decode(&first_open.request.data_base64)
        .expect("decode Observer provider payload");
    assert_eq!(
        first_open.request.payload_digest,
        Sha256Digest(format!("sha256:{:x}", Sha256::digest(&payload))),
        "provider payload is content-bound"
    );
    let payload: serde_json::Value =
        serde_json::from_slice(&payload).expect("Observer provider payload JSON");
    assert_eq!(payload["provider"], "observer-provider");
    assert_eq!(payload["requestId"], first_open.request_id.0);
    assert_eq!(payload["request"]["tools"], serde_json::json!([]));
    assert_eq!(payload["request"]["tool_choice"], "none");
    assert_eq!(
        payload["request"]["text"]["format"]["strict"],
        serde_json::json!(true)
    );
    let encoded = serde_json::to_string(&payload).expect("bounded Observer payload");
    assert!(!encoded.contains("*** Begin Patch"));
    assert!(!encoded.to_ascii_lowercase().contains("credential"));
    drop(runtime);
    let mut runtime = fixture.runtime();
    runtime
        .open_for_job_recovering(
            &active,
            None,
            &Instant("2026-08-28T00:00:07.500Z".to_owned()),
        )
        .expect("reopen pending Observer checkpoint");
    assert_eq!(
        runtime
            .pending_observation_model_open(&active)
            .expect("load pending Observer open"),
        Some(first_open.clone()),
        "restart replays the exact retained model open rather than creating a second call"
    );
    let validation = first
        .receipt
        .validation
        .as_ref()
        .expect("validation receipt");
    assert_eq!(validation.artifact_refs.len(), 2);
    let result_revision = first.receipt.result_revision.clone().expect("result tree");
    let created = observation_chunk(
        &first_open,
        1,
        &serde_json::json!({"type": "created"}),
        false,
    );
    let created_result = runtime
        .accept_observation_model_chunk(
            &active,
            &created,
            &Instant("2026-08-28T00:00:08.000Z".to_owned()),
        )
        .expect("retain Observer created frame")
        .expect("Observer exchange");
    assert_eq!(
        created_result.retention,
        ObservationChunkRetention::Inserted {
            confirmed_sequence: 1
        }
    );
    let response = serde_json::json!({
        "schemaVersion": 1,
        "observationId": observation.intent.observation_id.0,
        "decision": "accept",
        "reasonCode": "criteria_satisfied",
        "summary": "The bounded evidence satisfies the requested criterion.",
        "rootCauses": [],
        "repairClass": null,
        "confidenceBps": 9000
    })
    .to_string();
    let delta = observation_chunk(
        &first_open,
        2,
        &serde_json::json!({"type": "output_text_delta", "delta": response}),
        false,
    );
    let mut stale = active.clone();
    stale.lease.fencing_token = FencingToken("2".to_owned());
    let stale_error = runtime
        .accept_observation_model_chunk(
            &stale,
            &delta,
            &Instant("2026-08-28T00:00:08.050Z".to_owned()),
        )
        .expect_err("stale Observer authority");
    assert_eq!(stale_error.code(), JobWorkspaceErrorCode::AuthorityMismatch);
    runtime
        .accept_observation_model_chunk(
            &active,
            &delta,
            &Instant("2026-08-28T00:00:08.100Z".to_owned()),
        )
        .expect("retain Observer response")
        .expect("Observer exchange");
    let completed_payload = serde_json::json!({
        "type": "completed",
        "responseId": "response-observer-fixture",
        "actualCostMicros": 47,
        "tokenUsage": {
            "input_tokens": 80,
            "cached_input_tokens": 0,
            "cache_write_input_tokens": 0,
            "output_tokens": 20,
            "reasoning_output_tokens": 0,
            "total_tokens": 100
        },
        "endTurn": true
    });
    let gap = observation_chunk(&first_open, 4, &completed_payload, true);
    assert_eq!(
        runtime
            .accept_observation_model_chunk(
                &active,
                &gap,
                &Instant("2026-08-28T00:00:08.200Z".to_owned()),
            )
            .expect("classify Observer gap")
            .expect("Observer exchange")
            .retention,
        ObservationChunkRetention::Gap {
            confirmed_sequence: 2
        }
    );
    let completed = observation_chunk(&first_open, 3, &completed_payload, true);
    drop(runtime);
    let mut interrupted =
        ChangeBatchStore::open(&fixture.root).expect("open interrupted Observer journal");
    let completed_bytes = serde_json::to_vec(&completed).expect("encode terminal Observer frame");
    assert_eq!(
        interrupted
            .retain_observation_model_chunk(
                &ObservationModelFrame {
                    model_exchange_id: first_open.model_exchange_id.clone(),
                    sequence: 3,
                    chunk_digest: Sha256Digest(format!(
                        "sha256:{:x}",
                        Sha256::digest(&completed_bytes)
                    )),
                    response_delta: &[],
                    model_usage: Some(ExecutionOutcomeUsage {
                        cost_microunits: 47,
                        runtime_millis: 0,
                        tokens: 100,
                    }),
                    terminal_status: Some("completed"),
                },
                &Instant("2026-08-28T00:00:08.250Z".to_owned()),
            )
            .expect("retain terminal frame before simulated process loss"),
        ObservationChunkRetention::Inserted {
            confirmed_sequence: 3
        }
    );
    let interrupted_record = interrupted
        .observation_model_record(&first_open.model_exchange_id)
        .expect("load terminal Observer exchange")
        .expect("terminal Observer exchange exists");
    let interrupted_response = parse_observation_response_strict(
        &interrupted_record.response_bytes,
        &interrupted_record.request.intent,
    )
    .expect("parse retained terminal Observer response");
    let interrupted_receipt = ObservationReceipt {
        identity: interrupted_record.request.intent.identity.clone(),
        input_digest: interrupted_record.request.intent.input_digest.clone(),
        model_usage: interrupted_record.model_usage.clone(),
        output_digest: derive_observation_output_digest(&interrupted_response)
            .expect("derive retained Observer output"),
        profile_digest: interrupted_record.request.intent.profile_digest.clone(),
        response: interrupted_response,
        result_revision: interrupted_record.request.intent.result_revision.clone(),
        source: ObservationSource::Model,
    };
    interrupted
        .retain_observation_receipt(
            &interrupted_receipt,
            &Instant("2026-08-28T00:00:08.260Z".to_owned()),
        )
        .expect("retain receipt before simulated process loss");
    let journal_database = fixture
        .root
        .join(".workspaces-change-batches/change-batch.sqlite3");
    let workspace_id = Connection::open(&journal_database)
        .expect("open Observer recovery database")
        .query_row(
            "SELECT workspace_id FROM change_batch_workspace WHERE active_batch_id = ?1",
            [&observation.intent.identity.batch_id.0],
            |row| row.get::<_, String>(0),
        )
        .expect("load Observer workspace identity");
    let retained_progress = interrupted
        .progress_events(&observation.intent.identity.batch_id)
        .expect("load retained Observer progress");
    let observation_completed = ChangeBatchProgressEvent {
        artifact_refs: Vec::new(),
        identity: observation.intent.identity.clone(),
        occurred_at: Instant("2026-08-28T00:00:08.265Z".to_owned()),
        sequence: retained_progress
            .last()
            .expect("ObservationRequested progress")
            .sequence
            + 1,
        state: ChangeBatchProgressState::ObservationCompleted,
        summary: "ChangeBatch bounded observation completed".to_owned(),
    };
    interrupted
        .retain_workspace_progress(
            &workspace_id,
            &observation_completed,
            BatchState::ObservationPending,
            BatchState::ObservationPending,
        )
        .expect("retain ObservationCompleted before simulated process loss");
    drop(interrupted);
    let mut runtime = fixture.runtime();
    runtime
        .open_for_job_recovering(
            &active,
            None,
            &Instant("2026-08-28T00:00:08.275Z".to_owned()),
        )
        .expect("reopen terminal Observer frame without receipt");
    assert_eq!(
        runtime
            .pending_observation_model_open(&active)
            .expect("reload terminal Observer exchange"),
        Some(first_open.clone()),
        "restart replays a billed terminal exchange until its final route is durable"
    );
    let applied = runtime
        .accept_observation_model_chunk(
            &active,
            &completed,
            &Instant("2026-08-28T00:00:08.300Z".to_owned()),
        )
        .expect("complete strict Observer response")
        .expect("Observer exchange");
    assert_eq!(
        applied
            .completed_progress
            .iter()
            .map(|event| &event.state)
            .collect::<Vec<_>>(),
        [&ChangeBatchProgressState::Accepted]
    );
    assert_eq!(
        applied.receipt.as_ref().map(|receipt| &receipt.source),
        Some(&ObservationSource::Model)
    );
    assert_eq!(
        applied
            .receipt
            .as_ref()
            .and_then(|receipt| receipt.model_usage.as_ref())
            .map(|usage| usage.cost_microunits),
        Some(47),
        "the exact settled Observer charge survives terminal replay"
    );
    assert!(
        applied
            .change_batch_receipt
            .as_ref()
            .and_then(|receipt| receipt.observation.as_ref())
            .is_some()
    );
    let replay = runtime
        .accept_observation_model_chunk(
            &active,
            &completed,
            &Instant("2026-08-28T00:00:08.400Z".to_owned()),
        )
        .expect("replay terminal Observer frame")
        .expect("Observer exchange");
    assert_eq!(
        replay.retention,
        ObservationChunkRetention::Duplicate {
            confirmed_sequence: 3
        }
    );
    assert!(replay.completed_progress.is_empty());
    let second = batch_proposal_with_patch(
        &active,
        result_revision,
        "*** Begin Patch\n*** Add File: second.txt\n+second\n*** End Patch\n",
        "turn-new-diagnostic",
    );
    let executed = Box::pin(runtime.execute_change_batch(
        &active,
        &second,
        &Instant("2026-08-28T00:00:10.000Z".to_owned()),
    ))
    .await
    .expect("compare new missing-module diagnostic");
    assert_eq!(
        executed.progress.last().map(|event| &event.state),
        Some(&ChangeBatchProgressState::RepairRequired)
    );
    assert!(
        executed.observation_request.is_none(),
        "a new missing module is a hard Repair decision and must not call the Observer model"
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
    let executed = Box::pin(runtime.execute_change_batch(
        &active,
        &proposal,
        &Instant("2026-08-28T00:00:02.000Z".to_owned()),
    ))
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
    let executed = Box::pin(runtime.execute_change_batch(
        &active,
        &proposal,
        &Instant("2026-08-28T00:00:02.000Z".to_owned()),
    ))
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

#[test]
fn unchanged_or_cancelled_writer_produces_no_candidate() {
    let fixture = Fixture::new("fail-closed");
    let mut active = active_job();
    let mut runtime = fixture.runtime();
    runtime
        .open_for_job(&active, None)
        .expect("create unchanged Job checkout");
    let unchanged = runtime
        .prepare_candidate(&active, winwincode_codex::RoleExecutionMode::React)
        .expect_err("unchanged checkout has no candidate");
    assert_eq!(unchanged.code(), JobWorkspaceErrorCode::Candidate);
    active.lifecycle = ActiveJobLifecycle::Cancelling;
    let cancelling = runtime
        .prepare_candidate(&active, winwincode_codex::RoleExecutionMode::React)
        .expect_err("cancelling Job has no candidate");
    assert_eq!(cancelling.code(), JobWorkspaceErrorCode::Candidate);
    runtime
        .close_job(&active.job.job_id, WorkspaceCloseReason::Cancelled)
        .expect("remove cancelled workspace");
}
