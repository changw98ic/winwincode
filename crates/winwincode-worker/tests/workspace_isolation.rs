// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_domain::{
    CodexThreadId, ExecutionAckSequence, ExecutionJobId, FencingToken, Instant, LeaseId,
    ProductSessionId, RepositoryId, SessionIdentity, Sha256Digest, WorkerId, WorkerInstanceId,
    WorkerSessionId,
};
use winwincode_execution_port::generated::{
    ExecutionJob, ExecutionLeaseStamp, ExecutionLimits, ExecutionScope, ExecutionWorkspace,
    ExecutionWorkspaceWriteMode, ProductSessionExecutionScope, ProductSessionExecutionScopeKind,
};
use winwincode_worker::workspace::{WorkspaceCloseReason, WorkspaceErrorCode, WorkspaceManager};
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
            "winwincode-worker-workspace-{}-{sequence}",
            std::process::id()
        ));
        let sources = root.join("sources");
        let workspaces = root.join("workspaces");
        let repository_id = RepositoryId("repo_workspace_fixture".to_owned());
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
fn parallel_jobs_have_isolated_candidates_and_artifact_provenance() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let active_a = active_job(&fixture.repository_id, "a", 1);
    let active_b = active_job(&fixture.repository_id, "b", 2);
    let mut workspace_a = manager.create(&active_a).expect("create workspace A");
    let mut workspace_b = manager.create(&active_b).expect("create workspace B");

    assert_ne!(workspace_a.id(), workspace_b.id());
    assert_ne!(workspace_a.layout().root(), workspace_b.layout().root());
    let candidate_a = workspace_a
        .checkout_path("candidate.txt")
        .expect("candidate A path");
    let candidate_b = workspace_b
        .checkout_path("candidate.txt")
        .expect("candidate B path");
    fs::write(&candidate_a, b"candidate A\n").expect("write candidate A");
    fs::write(&candidate_b, b"candidate B\n").expect("write candidate B");
    workspace_a
        .write_artifact("logs/result.txt", b"artifact A")
        .expect("write artifact A");
    workspace_b
        .write_artifact("logs/result.txt", b"artifact B")
        .expect("write artifact B");

    let snapshot_a = workspace_a
        .snapshot_candidate()
        .expect("snapshot candidate A");
    let snapshot_b = workspace_b
        .snapshot_candidate()
        .expect("snapshot candidate B");
    workspace_a
        .verify_candidate(&snapshot_a)
        .expect("verify candidate A");
    workspace_b
        .verify_candidate(&snapshot_b)
        .expect("verify candidate B");
    assert_ne!(snapshot_a.content_digest, snapshot_b.content_digest);
    assert_ne!(snapshot_a.provenance, snapshot_b.provenance);
    assert!(
        std::str::from_utf8(snapshot_a.manifest_bytes())
            .expect("candidate manifest UTF-8")
            .contains(&snapshot_a.candidate_commit_id)
    );

    let artifacts_a = workspace_a
        .snapshot_artifacts()
        .expect("snapshot artifacts A");
    let artifacts_b = workspace_b
        .snapshot_artifacts()
        .expect("snapshot artifacts B");
    workspace_a
        .verify_artifacts(&artifacts_a)
        .expect("verify artifacts A");
    workspace_b
        .verify_artifacts(&artifacts_b)
        .expect("verify artifacts B");
    assert_ne!(artifacts_a.content_digest, artifacts_b.content_digest);
    assert_eq!(artifacts_a.provenance.execution_job_id, active_a.job.job_id);
    assert_eq!(artifacts_a.provenance.attempt, 1);
    assert_eq!(artifacts_b.provenance.attempt, 2);
    assert_eq!(
        fs::read(fixture.repository().join("base.txt")).expect("read controlled source"),
        b"base\n"
    );
    assert!(!fixture.repository().join("candidate.txt").exists());

    let root_a = workspace_a.layout().root().to_path_buf();
    let root_b = workspace_b.layout().root().to_path_buf();
    workspace_a
        .close(WorkspaceCloseReason::Completed)
        .expect("close workspace A");
    workspace_b
        .close(WorkspaceCloseReason::Completed)
        .expect("close workspace B");
    assert!(!root_a.exists());
    assert!(!root_b.exists());
}

#[test]
fn traversal_and_symbolic_link_escape_are_rejected() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let mut workspace = manager
        .create(&active_job(&fixture.repository_id, "escape", 1))
        .expect("create workspace");

    let checkout_error = workspace
        .checkout_path("../foreign")
        .expect_err("checkout traversal must fail");
    assert_eq!(checkout_error.code(), WorkspaceErrorCode::PathEscape);
    let artifact_error = workspace
        .write_artifact("../foreign", b"bad")
        .expect_err("Artifact traversal must fail");
    assert_eq!(artifact_error.code(), WorkspaceErrorCode::PathEscape);

    let foreign = fixture.root.join("foreign");
    fs::create_dir(&foreign).expect("create foreign directory");
    let link = workspace.layout().artifacts().join("link");
    std::os::unix::fs::symlink(&foreign, &link).expect("create Artifact link");
    let link_error = workspace
        .write_artifact("link/escaped.txt", b"bad")
        .expect_err("symbolic-link escape must fail");
    assert_eq!(link_error.code(), WorkspaceErrorCode::PathEscape);
    assert!(!foreign.join("escaped.txt").exists());

    workspace
        .close(WorkspaceCloseReason::Failed)
        .expect("close workspace");
}

#[test]
fn cancellation_removes_private_files_while_restart_preserves_active_checkout() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let cancelled = manager
        .create(&active_job(&fixture.repository_id, "cancel", 1))
        .expect("create cancelled workspace");
    let cancelled_root = cancelled.layout().root().to_path_buf();
    fs::write(
        cancelled.layout().sandbox().join("state.sqlite3-wal"),
        b"wal",
    )
    .expect("write database WAL sidecar");
    fs::write(
        cancelled.layout().sandbox().join("state.sqlite3-shm"),
        b"shm",
    )
    .expect("write database SHM sidecar");
    cancelled
        .close(WorkspaceCloseReason::Cancelled)
        .expect("cancel workspace");
    assert!(!cancelled_root.exists());

    let active = active_job(&fixture.repository_id, "crash", 2);
    let crashed = manager.create(&active).expect("create crashed workspace");
    let crashed_root = crashed.layout().root().to_path_buf();
    let checkout = crashed.layout().checkout().to_path_buf();
    fs::write(
        crashed.layout().temporary().join("runtime.sqlite3-wal"),
        b"wal",
    )
    .expect("write crashed WAL sidecar");
    drop(crashed);

    assert!(crashed_root.exists());
    let recovered = fixture
        .manager()
        .create_or_recover(&active, None)
        .expect("recover exact active workspace");
    assert_eq!(recovered.layout().checkout(), checkout);
    assert!(
        recovered
            .layout()
            .temporary()
            .join("runtime.sqlite3-wal")
            .exists()
    );
    recovered
        .close(WorkspaceCloseReason::Failed)
        .expect("terminally remove recovered workspace");
    assert!(!crashed_root.exists());
    let worktree_list = git_output(&fixture.repository(), &["worktree", "list", "--porcelain"]);
    assert!(!String::from_utf8_lossy(&worktree_list).contains(checkout.to_string_lossy().as_ref()));
}

#[test]
fn artifact_snapshot_detects_byte_changes() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let mut workspace = manager
        .create(&active_job(&fixture.repository_id, "digest", 1))
        .expect("create workspace");
    workspace
        .write_artifact("result.json", br#"{"ok":true}"#)
        .expect("write Artifact");
    let snapshot = workspace.snapshot_artifacts().expect("snapshot Artifacts");
    fs::write(
        workspace.layout().artifacts().join("result.json"),
        b"changed",
    )
    .expect("alter Artifact");
    let error = workspace
        .verify_artifacts(&snapshot)
        .expect_err("altered Artifact must fail verification");
    assert_eq!(error.code(), WorkspaceErrorCode::DigestMismatch);
}

fn active_job(repository_id: &RepositoryId, suffix: &str, attempt: i64) -> ActiveJob {
    let product_session_id = ProductSessionId(format!("psn_workspace_{suffix}"));
    let worker_session_id = WorkerSessionId(format!("wsn_workspace_{suffix}"));
    let codex_thread_id = CodexThreadId(format!("ctx_workspace_{suffix}"));
    let job_id = ExecutionJobId(format!("job_workspace_{suffix}"));
    let lease = ExecutionLeaseStamp {
        attempt,
        expires_at: Instant("2030-01-01T00:00:00.000Z".to_owned()),
        fencing_token: FencingToken(attempt.to_string()),
        issued_at: Instant("2029-01-01T00:00:00.000Z".to_owned()),
        job_id: job_id.clone(),
        lease_id: LeaseId(format!("lse_workspace_{suffix}")),
        worker_id: WorkerId("wrk_workspace".to_owned()),
        worker_instance_id: WorkerInstanceId("wki_workspace".to_owned()),
    };
    ActiveJob {
        job: ExecutionJob {
            attempt,
            execution_profile: "fixture".to_owned(),
            goal: "verify isolated workspace".to_owned(),
            job_id,
            limits: ExecutionLimits {
                deadline_at: Instant("2030-01-01T00:00:00.000Z".to_owned()),
                max_artifact_bytes: 1_048_576,
                max_runtime_seconds: 300,
            },
            payload_digest: Sha256Digest("a".repeat(64)),
            scope: ExecutionScope::ProductSessionExecutionScope(ProductSessionExecutionScope {
                kind: ProductSessionExecutionScopeKind::ProductSession,
                product_session_id: product_session_id.clone(),
            }),
            stage_input: None,
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
            stage_run_id: None,
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
