use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};
use winwincode_delivery::{
    application::{
        stage::{
            TerminalArtifactReference, TerminalOutcomeStatus,
            test_support::{
                active_lease_identity, delivery_terminal_outcome_facts, session_binding_authority,
                terminal_outcome_metadata, terminal_worker_outcome,
            },
        },
        verdict::test_support::{VerdictFixtureOutcome, verdict_fixture},
    },
    domain::{
        DELIVERY_SCHEMA_VERSION, Delivery, RepositoryKind, RepositoryRef, StageRunStatus,
        candidate::freeze_delivery_candidate_from_source,
    },
};
use winwincode_domain::{
    ArtifactId, CodexThreadId, DeliveryId, ExecutionAckSequence, ExecutionJobId,
    ExecutionMessageId, FencingToken, Instant, LeaseId, ProductSessionId, RequestId, Sha256Digest,
    StageRunId, WorkerId, WorkerInstanceId, WorkerSessionId,
};
use winwincode_storage::{
    ArtifactAccess, ArtifactChunk, ArtifactOpen, ArtifactProvenance, ArtifactRetention,
    ArtifactStore, CandidateSourceManifest, FakeArtifactObjectStore, LocalGitSourceResolver,
    ReceiptScopeKey,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-delivery-candidate-source-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn git(root: &Path, arguments: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git command");
    assert!(
        output.status.success(),
        "git {:?}: {}",
        arguments,
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn text(value: Vec<u8>) -> String {
    String::from_utf8(value)
        .expect("git UTF-8")
        .trim()
        .to_owned()
}

fn commit(root: &Path, message: &str, timestamp: &str) {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["commit", "-q", "-m", message])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "WinWinCode Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@winwincode.invalid")
        .env("GIT_COMMITTER_NAME", "WinWinCode Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@winwincode.invalid")
        .env("GIT_AUTHOR_DATE", timestamp)
        .env("GIT_COMMITTER_DATE", timestamp)
        .status()
        .expect("git commit");
    assert!(status.success());
}

fn repository_fixture(root: &Path) -> (String, String) {
    fs::create_dir_all(root).expect("repository root");
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["init", "-q", "-b", "main"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("git init")
            .success()
    );
    fs::create_dir_all(root.join("src")).expect("source directory");
    fs::write(root.join("src/app.txt"), b"base\n").expect("base source");
    git(root, &["add", "--", "src/app.txt"]);
    commit(root, "base", "2026-08-25T00:00:00Z");
    let base = text(git(root, &["rev-parse", "HEAD"]));
    fs::write(root.join("src/app.txt"), b"base\ncandidate\n").expect("candidate source");
    git(root, &["add", "--", "src/app.txt"]);
    commit(root, "candidate", "2026-08-25T00:01:00Z");
    let candidate = text(git(root, &["rev-parse", "HEAD"]));
    (base, candidate)
}

#[test]
#[allow(clippy::too_many_lines)]
fn delivery_freezes_only_the_rebuilt_source_named_by_the_successful_worker_outcome() {
    let root = temporary_directory("freeze");
    let repositories = root.join("repositories");
    let repository = repositories.join("project-one");
    let (base_commit, candidate_commit) = repository_fixture(&repository);
    let fixture = verdict_fixture(
        &DeliveryId("dlv_01J00000000000000000000301".into()),
        VerdictFixtureOutcome::Pass,
    );
    let mut snapshot = fixture.delivery.into_snapshot();
    snapshot.spec.repository = RepositoryRef {
        schema_version: DELIVERY_SCHEMA_VERSION,
        kind: RepositoryKind::LocalGit,
        locator: "project-one".into(),
    };
    snapshot.spec.base_revision.clone_from(&base_commit);
    let producer = snapshot
        .stage_runs
        .iter_mut()
        .find(|run| run.role == "executor")
        .expect("executor");
    producer.id = StageRunId("run_00000000000000000000000301".into());
    producer.status = StageRunStatus::Succeeded;
    let finished_at = producer.finished_at_millis.expect("executor finish");
    let producer_id = producer.id.clone();
    let binding = snapshot
        .session_bindings
        .iter_mut()
        .find(|binding| binding.id.0 == "binding-executor-1")
        .expect("executor binding");
    binding.stage_run_id = producer_id.clone();
    binding.product_session_id = ProductSessionId("psn_00000000000000000000000301".into());
    binding.execution_job_id = ExecutionJobId("job_00000000000000000000000301".into());
    binding.worker_session_id = Some(WorkerSessionId("wsn_00000000000000000000000301".into()));
    binding.codex_thread_id = Some(CodexThreadId("cdx_00000000000000000000000301".into()));
    let execution_job_id = binding.execution_job_id.clone();
    let worker_session_id = binding.worker_session_id.clone().expect("WorkerSession");
    let codex_thread_id = binding.codex_thread_id.clone();
    let delivery = Delivery::try_from_snapshot(snapshot).expect("settled executor Delivery");

    let artifact_id = ArtifactId("art_00000000000000000000000301".into());
    let manifest = CandidateSourceManifest::new(candidate_commit.clone())
        .expect("candidate manifest")
        .encode()
        .expect("manifest encoding");
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&manifest)));
    let scope = ReceiptScopeKey::from_encoded(b"repository:project-one".to_vec()).expect("scope");
    let provenance = ArtifactProvenance::execution_job(
        execution_job_id,
        1,
        LeaseId("lse_00000000000000000000000301".into()),
        FencingToken("301".into()),
        WorkerId("wrk_00000000000000000000000301".into()),
        WorkerInstanceId("wki_00000000000000000000000301".into()),
        worker_session_id,
    )
    .expect("Artifact provenance");
    let mut artifacts = ArtifactStore::open(
        root.join("catalog"),
        Box::new(FakeArtifactObjectStore::new()),
    )
    .expect("Artifact store");
    artifacts
        .open_artifact(ArtifactOpen::new(
            scope.clone(),
            ExecutionMessageId("xmsg_00000000000000000000000301".into()),
            RequestId("req_00000000000000000000000301".into()),
            artifact_id.clone(),
            "candidate",
            "application/vnd.winwincode.git-candidate+json",
            digest.clone(),
            manifest.len() as u64,
            Some("candidate.json".into()),
            provenance.clone(),
            ArtifactRetention::Indefinite,
            finished_at.saturating_sub(1),
        ))
        .expect("Artifact open");
    artifacts
        .append_chunk(&ArtifactChunk::new(
            scope.clone(),
            ExecutionMessageId("xmsg_00000000000000000000000302".into()),
            artifact_id.clone(),
            provenance.clone(),
            finished_at,
            1,
            "application/octet-stream",
            digest.clone(),
            manifest,
            true,
        ))
        .expect("Artifact complete");
    let object = artifacts
        .read_exact(&ArtifactAccess::new(
            scope,
            artifact_id.clone(),
            digest.clone(),
            provenance.clone(),
        ))
        .expect("Artifact read");
    let source = LocalGitSourceResolver::open(&repositories)
        .expect("source resolver")
        .resolve_candidate(&object, "project-one", &base_commit)
        .expect("rebuilt source");

    let active_lease = active_lease_identity(
        provenance.execution_job_id().clone(),
        provenance.attempt(),
        provenance.lease_id().clone(),
        provenance.fencing_token().clone(),
        provenance.worker_id().clone(),
        provenance.worker_instance_id().clone(),
        provenance.worker_session_id().clone(),
    );
    let authority = session_binding_authority(
        active_lease,
        Instant("2026-08-25T00:00:00.000Z".into()),
        Instant("2026-08-25T01:00:00.000Z".into()),
    );
    let terminal = terminal_worker_outcome(
        producer_id.clone(),
        provenance.execution_job_id().clone(),
        1,
        provenance.lease_id().clone(),
        provenance.fencing_token().clone(),
        provenance.worker_id().clone(),
        provenance.worker_instance_id().clone(),
        provenance.worker_session_id().clone(),
        TerminalOutcomeStatus::Succeeded,
        terminal_outcome_metadata(
            codex_thread_id.clone(),
            finished_at,
            ExecutionAckSequence(12),
            vec![TerminalArtifactReference {
                artifact_id,
                digest,
            }],
        ),
    );
    let outcome = delivery_terminal_outcome_facts(authority.clone(), terminal);

    let foreign_artifact_outcome = delivery_terminal_outcome_facts(
        authority,
        terminal_worker_outcome(
            producer_id,
            provenance.execution_job_id().clone(),
            1,
            provenance.lease_id().clone(),
            provenance.fencing_token().clone(),
            provenance.worker_id().clone(),
            provenance.worker_instance_id().clone(),
            provenance.worker_session_id().clone(),
            TerminalOutcomeStatus::Succeeded,
            terminal_outcome_metadata(
                codex_thread_id,
                finished_at,
                ExecutionAckSequence(12),
                vec![TerminalArtifactReference {
                    artifact_id: ArtifactId("art_00000000000000000000000999".into()),
                    digest: Sha256Digest(format!("sha256:{}", "f".repeat(64))),
                }],
            ),
        ),
    );
    freeze_delivery_candidate_from_source(&delivery, &source, &foreign_artifact_outcome)
        .expect_err("candidate source must be named by the accepted terminal outcome");

    let candidate = freeze_delivery_candidate_from_source(&delivery, &source, &outcome)
        .expect("candidate from rebuilt source");
    assert_eq!(candidate.base_commit_id(), base_commit);
    assert_eq!(candidate.candidate_commit_id(), candidate_commit);
    assert_eq!(candidate.changed_paths().len(), 1);
    assert_eq!(candidate.changed_paths()[0].path, "src/app.txt");
    assert_eq!(
        candidate.producer_artifact_ref(),
        &object.metadata().artifact_id().0
    );

    artifacts.close().expect("Artifact close");
    fs::remove_dir_all(root).expect("fixture release");
}
