// SPDX-License-Identifier: Apache-2.0

//! Stable Candidate ref acceptance: recorded before cleanup, idempotent under
//! repeated freezes, and fail closed when the ref cannot be created.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use winwincode_domain::{
    CodexThreadId, ExecutionAckSequence, ExecutionJobId, FencingToken, Instant, LeaseId,
    ProductSessionId, RepositoryId, SessionIdentity, Sha256Digest, WorkerId, WorkerInstanceId,
    WorkerSessionId,
};
use winwincode_execution_port::generated::{
    ExecutionJob, ExecutionLeaseStamp, ExecutionLimits, ExecutionScope, ExecutionWorkspace,
    ExecutionWorkspaceWriteMode, ProductSessionExecutionScope, ProductSessionExecutionScopeKind,
};
use winwincode_worker::candidate_ref::{
    CANDIDATE_REF_PREFIX, CandidateRefErrorCode, candidate_ref_name, create_candidate_ref,
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
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after the Unix epoch")
            .as_nanos();
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "winwincode-worker-candidate-ref-{}-{nanos}-{sequence}",
            std::process::id()
        ));
        let sources = root.join("sources");
        let workspaces = root.join("workspaces");
        let repository_id = RepositoryId("repo_candidate_ref_fixture".to_owned());
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
fn freeze_publishes_stable_ref_that_survives_workspace_cleanup() {
    let fixture = Fixture::new();
    let mut workspace = fixture
        .manager()
        .create(&active_job(&fixture.repository_id, "stable", 1))
        .expect("create workspace");
    fs::write(
        workspace
            .checkout_path("candidate.txt")
            .expect("candidate path"),
        b"candidate\n",
    )
    .expect("write candidate change");

    let snapshot = workspace.snapshot_candidate().expect("freeze candidate");
    let ref_name = format!("{CANDIDATE_REF_PREFIX}{}", snapshot.candidate_commit_id);
    assert_eq!(
        ref_text(&fixture.repository(), &ref_name),
        snapshot.candidate_commit_id,
        "stable ref must resolve to the frozen candidate commit before cleanup"
    );

    let root = workspace.layout().root().to_path_buf();
    workspace
        .close(WorkspaceCloseReason::Completed)
        .expect("close workspace");
    assert!(!root.exists(), "workspace root must be removed");
    assert_eq!(
        ref_text(&fixture.repository(), &ref_name),
        snapshot.candidate_commit_id,
        "stable ref must still resolve after Worktree cleanup"
    );
    git(
        &fixture.repository(),
        &[
            "cat-file",
            "-e",
            &format!("{}^{{commit}}", snapshot.candidate_commit_id),
        ],
    );
}

#[test]
fn repeated_freeze_is_idempotent() {
    let fixture = Fixture::new();
    let mut workspace = fixture
        .manager()
        .create(&active_job(&fixture.repository_id, "repeat", 1))
        .expect("create workspace");
    fs::write(
        workspace
            .checkout_path("candidate.txt")
            .expect("candidate path"),
        b"candidate\n",
    )
    .expect("write candidate change");

    let first = workspace.snapshot_candidate().expect("first freeze");
    let receipt = create_candidate_ref(&fixture.repository(), &first.candidate_commit_id)
        .expect("re-record an already frozen candidate");
    assert!(receipt.preexisting, "same candidate must be a replay");
    assert_eq!(
        receipt.ref_name,
        format!("{CANDIDATE_REF_PREFIX}{}", first.candidate_commit_id)
    );
    let second = workspace.snapshot_candidate().expect("second freeze");
    assert_eq!(
        first.candidate_commit_id, second.candidate_commit_id,
        "deterministic freeze must reuse one candidate commit"
    );
    assert_eq!(first.content_digest, second.content_digest);
    assert_eq!(
        ref_text(
            &fixture.repository(),
            &format!("{CANDIDATE_REF_PREFIX}{}", first.candidate_commit_id)
        ),
        first.candidate_commit_id,
        "repeated freeze must keep one stable ref value"
    );

    workspace
        .close(WorkspaceCloseReason::Completed)
        .expect("close workspace");
}

#[test]
fn ref_creation_failure_fails_the_freeze_closed() {
    let fixture = Fixture::new();
    let mut workspace = fixture
        .manager()
        .create(&active_job(&fixture.repository_id, "closed", 1))
        .expect("create workspace");
    fs::write(
        workspace
            .checkout_path("candidate.txt")
            .expect("candidate path"),
        b"candidate\n",
    )
    .expect("write candidate change");
    let refs_dir = fixture
        .repository()
        .join(".git")
        .join("refs")
        .join("winwincode");
    fs::create_dir_all(&refs_dir).expect("create refs namespace");
    fs::write(
        refs_dir.join("candidates"),
        b"blocks the candidates directory\n",
    )
    .expect("block candidate ref namespace");

    let error = workspace
        .snapshot_candidate()
        .expect_err("freeze without a stable ref must fail closed");
    assert_eq!(error.code(), WorkspaceErrorCode::Git);
    let ref_prefix = fixture
        .repository()
        .join(".git")
        .join("refs")
        .join("winwincode")
        .join("candidates");
    assert!(
        !ref_prefix.exists() || ref_prefix.is_file(),
        "no candidate ref may be recorded when ref creation fails"
    );

    let root = workspace.layout().root().to_path_buf();
    workspace
        .close(WorkspaceCloseReason::Failed)
        .expect("failed freeze still reaches terminal cleanup");
    assert!(!root.exists());
}

#[test]
fn candidate_ref_name_follows_the_stable_namespace() {
    let short_id = "a".repeat(40);
    let long_id = "f".repeat(64);
    assert_eq!(
        candidate_ref_name(&short_id).expect("valid 40-hex id"),
        format!("{CANDIDATE_REF_PREFIX}{short_id}")
    );
    assert_eq!(
        candidate_ref_name(&long_id).expect("valid 64-hex id"),
        format!("{CANDIDATE_REF_PREFIX}{long_id}")
    );
    for rejected in [
        "",
        "../escape",
        "HEAD",
        "refs/heads/main",
        &"A".repeat(40),
        &"g".repeat(40),
        &"a".repeat(39),
        &"a".repeat(41),
    ] {
        let error = candidate_ref_name(rejected).expect_err("unsafe candidate id must fail");
        assert_eq!(error.code(), CandidateRefErrorCode::InvalidInput);
    }
}

#[test]
fn create_candidate_ref_records_and_replays_one_stable_ref() {
    let fixture = Fixture::new();
    let head = ref_text(&fixture.repository(), "HEAD^{commit}");
    let first = create_candidate_ref(&fixture.repository(), &head).expect("record one stable ref");
    assert!(!first.preexisting);
    assert_eq!(first.candidate_id, head);
    assert_eq!(first.candidate_commit_id, head);
    assert_eq!(first.ref_name, format!("{CANDIDATE_REF_PREFIX}{head}"));
    let replay = create_candidate_ref(&fixture.repository(), &head).expect("replay stable ref");
    assert!(replay.preexisting);
    assert_eq!(replay.ref_name, first.ref_name);
    assert_eq!(replay.candidate_commit_id, first.candidate_commit_id);
}

#[test]
fn create_candidate_ref_fails_closed_on_a_conflicting_ref_value() {
    let fixture = Fixture::new();
    let head = ref_text(&fixture.repository(), "HEAD^{commit}");
    let conflicting_object_id = ref_text(&fixture.repository(), "HEAD^{tree}");
    let ref_name = format!("{CANDIDATE_REF_PREFIX}{head}");
    git(
        &fixture.repository(),
        &[
            "update-ref",
            ref_name.as_str(),
            conflicting_object_id.as_str(),
        ],
    );
    let error = create_candidate_ref(&fixture.repository(), &head)
        .expect_err("a conflicting ref value must fail closed");
    assert_eq!(error.code(), CandidateRefErrorCode::Conflict);
}

#[test]
fn create_candidate_ref_rejects_unknown_commits() {
    let fixture = Fixture::new();
    let unknown_commit_id = "a".repeat(40);
    let error = create_candidate_ref(&fixture.repository(), &unknown_commit_id)
        .expect_err("unknown commit must fail closed");
    assert_eq!(error.code(), CandidateRefErrorCode::Git);
    let ref_name = format!("{CANDIDATE_REF_PREFIX}{unknown_commit_id}");
    let output = Command::new("git")
        .arg("-C")
        .arg(fixture.repository())
        .args(["rev-parse", "--verify", "--quiet", ref_name.as_str()])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run Git fixture query");
    assert!(
        !output.status.success(),
        "a failed ref creation must not leave a resolvable ref"
    );
}

fn active_job(repository_id: &RepositoryId, suffix: &str, attempt: i64) -> ActiveJob {
    let product_session_id = ProductSessionId(format!("psn_candidate_ref_{suffix}"));
    let worker_session_id = WorkerSessionId(format!("wsn_candidate_ref_{suffix}"));
    let codex_thread_id = CodexThreadId(format!("ctx_candidate_ref_{suffix}"));
    let job_id = ExecutionJobId(format!("job_candidate_ref_{suffix}"));
    let lease = ExecutionLeaseStamp {
        attempt,
        expires_at: Instant("2030-01-01T00:00:00.000Z".to_owned()),
        fencing_token: FencingToken(attempt.to_string()),
        issued_at: Instant("2029-01-01T00:00:00.000Z".to_owned()),
        job_id: job_id.clone(),
        lease_id: LeaseId(format!("lse_candidate_ref_{suffix}")),
        worker_id: WorkerId("wrk_workspace".to_owned()),
        worker_instance_id: WorkerInstanceId("wki_workspace".to_owned()),
    };
    ActiveJob {
        job: ExecutionJob {
            attempt,
            execution_profile: "fixture".to_owned(),
            goal: "verify stable candidate refs".to_owned(),
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

fn ref_text(repository: &Path, ref_name: &str) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["rev-parse", "--verify", "--end-of-options", ref_name])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run Git fixture query");
    assert!(
        output.status.success(),
        "Git fixture ref query failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}
