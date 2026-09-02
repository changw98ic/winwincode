// SPDX-License-Identifier: Apache-2.0

use std::{path::PathBuf, process::Command};

use sha2::{Digest, Sha256};
use winwincode_domain::{
    CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionAckSequence, ExecutionJobId, FencingToken,
    Instant, LeaseId, ProductSessionId, RepositoryId, RequestId, SchemaVersion, SessionIdentity,
    Sha256Digest, StageRunId, WorkerId, WorkerInstanceId, WorkerSessionId,
};
use winwincode_execution_port::generated::{
    DeliveryStageAcceptanceCriterionInput, DeliveryStageExecutionScope,
    DeliveryStageExecutionScopeKind, DeliveryStageInput, DeliveryStageTaskInput, ExecutionJob,
    ExecutionJobReplacementAuthority, ExecutionLeaseStamp, ExecutionLimits, ExecutionScope,
    ExecutionWorkspace, ExecutionWorkspaceWriteMode,
};
use winwincode_worker::{
    ActiveJob, ActiveJobLifecycle,
    workspace::WorkspaceCloseReason,
    workspace_runtime::{JobWorkspaceErrorCode, JobWorkspaceRuntime},
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
        JobWorkspaceRuntime::open(&self.workspaces, &self.sources).expect("open workspace runtime")
    }

    fn repository(&self) -> PathBuf {
        self.sources.join("repo_00000000000000000000000001")
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
