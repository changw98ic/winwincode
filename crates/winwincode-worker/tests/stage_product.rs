// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest as _, Sha256};
use winwincode_domain::{
    ArtifactId, CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionAckSequence, ExecutionJobId,
    FencingToken, Instant, LeaseId, ProductSessionId, RepositoryId, SchemaVersion, SessionIdentity,
    Sha256Digest, StageRunId, WorkerId, WorkerInstanceId, WorkerSessionId,
};
use winwincode_execution_port::generated::{
    ArtifactKind, DeliveryStageAcceptanceCriterionInput, DeliveryStageExecutionScope,
    DeliveryStageExecutionScopeKind, DeliveryStageInput, DeliveryStageTaskInput, ExecutionJob,
    ExecutionLeaseStamp, ExecutionLimits, ExecutionScope, ExecutionWorkspace,
    ExecutionWorkspaceWriteMode,
};
use winwincode_worker::stage_product::{
    CANDIDATE_FILE_NAME, CANDIDATE_MEDIA_TYPE, CandidateProductErrorCode,
    prepare_candidate_artifact,
};
use winwincode_worker::workspace::{WorkspaceCloseReason, WorkspaceManager};
use winwincode_worker::{ActiveJob, ActiveJobLifecycle};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    root: PathBuf,
    sources: PathBuf,
    workspaces: PathBuf,
    repository_id: RepositoryId,
}

impl Fixture {
    fn new() -> Self {
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "winwincode-worker-stage-product-{}-{sequence}",
            std::process::id()
        ));
        let sources = root.join("sources");
        let workspaces = root.join("workspaces");
        let repository_id = RepositoryId("repo_stage_product_fixture".to_owned());
        let repository = sources.join(&repository_id.0);
        fs::create_dir_all(&repository).expect("create fixture repository");
        fs::create_dir_all(&workspaces).expect("create fixture workspace root");
        git(&repository, &["init", "--initial-branch=main"]);
        git(&repository, &["config", "user.name", "Fixture"]);
        git(
            &repository,
            &["config", "user.email", "fixture@winwincode.invalid"],
        );
        fs::write(repository.join("base.txt"), b"base\n").expect("write base file");
        git(&repository, &["add", "base.txt"]);
        git(&repository, &["commit", "-m", "base"]);
        Self {
            root,
            sources,
            workspaces,
            repository_id,
        }
    }

    fn manager(&self) -> WorkspaceManager {
        WorkspaceManager::open(&self.workspaces, &self.sources).expect("open workspace manager")
    }

    fn repository(&self) -> PathBuf {
        self.sources.join(&self.repository_id.0)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn executor_freezes_real_checkout_into_exact_candidate_artifact() {
    let fixture = Fixture::new();
    let active = active_job(&fixture.repository_id, "writer", "executor");
    let mut workspace = fixture
        .manager()
        .create(&active)
        .expect("create writer workspace");
    fs::write(
        workspace
            .checkout_path("candidate.txt")
            .expect("candidate path"),
        b"candidate\n",
    )
    .expect("write candidate change");

    let prepared =
        prepare_candidate_artifact(&active, &mut workspace).expect("prepare candidate artifact");
    let descriptor = prepared.descriptor(ArtifactId("art_stage_product_candidate".to_owned()));

    assert_eq!(descriptor.kind, ArtifactKind::Candidate);
    assert_eq!(descriptor.media_type, CANDIDATE_MEDIA_TYPE);
    assert_eq!(descriptor.file_name.as_deref(), Some(CANDIDATE_FILE_NAME));
    assert_eq!(
        usize::try_from(descriptor.size_bytes).expect("candidate byte length"),
        prepared.bytes().len()
    );
    assert_eq!(descriptor.digest, *prepared.digest());
    assert_eq!(
        prepared.digest().0,
        format!("sha256:{:x}", Sha256::digest(prepared.bytes()))
    );
    assert_eq!(
        prepared.snapshot().provenance.execution_job_id,
        active.job.job_id
    );
    assert_eq!(
        prepared.snapshot().provenance.stage_run_id,
        active.session_identity.stage_run_id
    );
    assert!(
        std::str::from_utf8(prepared.bytes())
            .expect("candidate manifest UTF-8")
            .contains(&prepared.snapshot().candidate_commit_id)
    );
    let upload = prepared
        .clone()
        .into_upload(Instant("2029-06-01T00:00:00.000Z".to_owned()));
    assert_eq!(upload.bytes, prepared.bytes());
    assert_eq!(upload.digest, *prepared.digest());
    assert_eq!(upload.execution_profile, "executor");
    assert_eq!(upload.lease, active.lease);
    assert_eq!(upload.session_identity, active.session_identity);
    assert!(!fixture.repository().join("candidate.txt").exists());
    workspace
        .close(WorkspaceCloseReason::Completed)
        .expect("close writer workspace");
}

#[test]
fn non_writer_and_cancelled_jobs_emit_no_candidate() {
    let fixture = Fixture::new();
    let mut reviewer = active_job(&fixture.repository_id, "reviewer", "reviewer");
    let mut reviewer_workspace = fixture
        .manager()
        .create(&reviewer)
        .expect("create reviewer workspace");
    fs::write(
        reviewer_workspace
            .checkout_path("candidate.txt")
            .expect("candidate path"),
        b"candidate\n",
    )
    .expect("write candidate change");
    assert_eq!(
        prepare_candidate_artifact(&reviewer, &mut reviewer_workspace)
            .expect_err("reviewer must not produce candidate")
            .code(),
        CandidateProductErrorCode::InvalidRole
    );

    reviewer.job.execution_profile = "executor".to_owned();
    reviewer.lifecycle = ActiveJobLifecycle::Cancelling;
    assert_eq!(
        prepare_candidate_artifact(&reviewer, &mut reviewer_workspace)
            .expect_err("cancelled executor must not produce candidate")
            .code(),
        CandidateProductErrorCode::InvalidLifecycle
    );
    reviewer.lifecycle = ActiveJobLifecycle::Running;
    reviewer.job.workspace.write_mode = ExecutionWorkspaceWriteMode::ReadOnly;
    assert_eq!(
        prepare_candidate_artifact(&reviewer, &mut reviewer_workspace)
            .expect_err("read-only executor must not produce candidate")
            .code(),
        CandidateProductErrorCode::InvalidScope
    );
    assert!(
        !git_output(
            reviewer_workspace.layout().checkout(),
            &["status", "--porcelain"]
        )
        .is_empty()
    );
    reviewer_workspace
        .close(WorkspaceCloseReason::Cancelled)
        .expect("close reviewer workspace");
}

#[test]
fn candidate_rejects_workspace_from_another_exact_attempt() {
    let fixture = Fixture::new();
    let active_a = active_job(&fixture.repository_id, "writer-a", "executor");
    let active_b = active_job(&fixture.repository_id, "writer-b", "executor");
    let mut workspace_a = fixture
        .manager()
        .create(&active_a)
        .expect("create writer A workspace");
    fs::write(
        workspace_a
            .checkout_path("candidate.txt")
            .expect("candidate path"),
        b"candidate\n",
    )
    .expect("write candidate change");

    assert_eq!(
        prepare_candidate_artifact(&active_b, &mut workspace_a)
            .expect_err("foreign workspace must fail")
            .code(),
        CandidateProductErrorCode::AuthorityMismatch
    );
    assert!(!git_output(workspace_a.layout().checkout(), &["status", "--porcelain"]).is_empty());
    workspace_a
        .close(WorkspaceCloseReason::Failed)
        .expect("close writer A workspace");
}

fn active_job(repository_id: &RepositoryId, suffix: &str, role: &str) -> ActiveJob {
    let product_session_id = ProductSessionId(format!("psn_stage_product_{suffix}"));
    let stage_run_id = StageRunId(format!("run_stage_product_{suffix}"));
    let worker_session_id = WorkerSessionId(format!("wsn_stage_product_{suffix}"));
    let codex_thread_id = CodexThreadId(format!("ctx_stage_product_{suffix}"));
    let job_id = ExecutionJobId(format!("job_stage_product_{suffix}"));
    let lease = ExecutionLeaseStamp {
        attempt: 1,
        expires_at: Instant("2030-01-01T00:00:00.000Z".to_owned()),
        fencing_token: FencingToken(format!("fence-{suffix}")),
        issued_at: Instant("2029-01-01T00:00:00.000Z".to_owned()),
        job_id: job_id.clone(),
        lease_id: LeaseId(format!("lse_stage_product_{suffix}")),
        worker_id: WorkerId("wrk_stage_product".to_owned()),
        worker_instance_id: WorkerInstanceId("wki_stage_product".to_owned()),
    };
    let criterion_id = format!("criterion-stage-product-{suffix}");
    let task_id = DeliveryTaskId(format!("dtk_stage_product_{suffix}"));
    ActiveJob {
        job: ExecutionJob {
            attempt: 1,
            execution_profile: role.to_owned(),
            goal: "produce one exact StrongFlow stage product".to_owned(),
            job_id,
            limits: ExecutionLimits {
                deadline_at: Instant("2030-01-01T00:00:00.000Z".to_owned()),
                max_artifact_bytes: 1_048_576,
                max_runtime_seconds: 300,
            },
            payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            scope: ExecutionScope::DeliveryStageExecutionScope(DeliveryStageExecutionScope {
                delivery_id: DeliveryId(format!("dlv_stage_product_{suffix}")),
                delivery_task_id: Some(task_id.clone()),
                kind: DeliveryStageExecutionScopeKind::DeliveryStage,
                product_session_id: product_session_id.clone(),
                rework_authorization: None,
                stage_run_id: stage_run_id.clone(),
            }),
            stage_input: Some(DeliveryStageInput {
                acceptance_criteria: vec![DeliveryStageAcceptanceCriterionInput {
                    criterion_id: criterion_id.clone(),
                    description: "The exact candidate is durable.".into(),
                    required: true,
                    verification_method: Some("Inspect the candidate manifest.".into()),
                }],
                candidate_ref: None,
                constraints: Vec::new(),
                delivery_spec_id: format!("spec-stage-product-{suffix}"),
                delivery_spec_revision: 1,
                goal: "Produce one exact candidate.".into(),
                out_of_scope: Vec::new(),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: vec!["Candidate workspace".into()],
                task: Some(DeliveryStageTaskInput {
                    acceptance_criterion_ids: vec![criterion_id],
                    goal: "produce one exact StrongFlow stage product".into(),
                    task_id,
                    title: "Produce candidate".into(),
                }),
                title: "Stage product fixture".into(),
            }),
            workspace: ExecutionWorkspace {
                checkout_revision: "HEAD".to_owned(),
                repository_id: repository_id.clone(),
                write_mode: ExecutionWorkspaceWriteMode::Candidate,
            },
        },
        lease,
        worker_session_id: worker_session_id.clone(),
        session_identity: SessionIdentity {
            codex_thread_id: codex_thread_id.clone(),
            product_session_id,
            stage_run_id: Some(stage_run_id),
            worker_session_id,
        },
        codex_thread_id,
        lifecycle: ActiveJobLifecycle::Running,
        last_event_sequence: ExecutionAckSequence(0),
    }
}

fn git(repository: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run Git fixture command");
    assert!(
        output.status.success(),
        "Git fixture command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_output(repository: &Path, arguments: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run Git fixture query");
    assert!(output.status.success());
    output.stdout
}
