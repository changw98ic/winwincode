use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};
use winwincode_domain::{
    ArtifactId, DeliveryId, ExecutionJobId, ExecutionMessageId, FencingToken, LeaseId,
    OrganizationId, ProductSessionId, ProjectId, RepositoryId, RequestId, Sha256Digest, UserId,
    WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_storage::{
    ArtifactChunk, ArtifactMeteringAttribution, ArtifactOpen, ArtifactProvenance,
    ArtifactRetention, ArtifactStore, CandidateGitPinReceipt, CandidateGitReleaseAuthority,
    CandidateGitRetentionState, CandidateGitTerminalOutcome, CandidateSourceManifest,
    FakeArtifactObjectStore, LocalGitSourceResolver, ProductStateStorage, SqliteStorage,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-git-candidate-retention-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn git(root: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .expect("git command");
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
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
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .status()
        .expect("git commit");
    assert!(status.success());
}

fn repository_fixture(root: &Path) -> (String, String) {
    fs::create_dir_all(root).expect("repository directory");
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["init", "-q", "-b", "main"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .status()
        .expect("git init");
    assert!(output.success());
    fs::write(root.join("candidate.txt"), b"base\n").expect("base file");
    git(root, &["add", "--", "candidate.txt"]);
    commit(root, "base", "2026-08-25T00:00:00Z");
    let base = git(root, &["rev-parse", "HEAD"]);
    fs::write(root.join("candidate.txt"), b"base\ncandidate\n").expect("candidate file");
    git(root, &["add", "--", "candidate.txt"]);
    commit(root, "candidate", "2026-08-25T00:01:00Z");
    let candidate = git(root, &["rev-parse", "HEAD"]);
    (base, candidate)
}

fn artifact_provenance() -> ArtifactProvenance {
    ArtifactProvenance::execution_job(
        ExecutionJobId("job_00000000000000000000000003".into()),
        1,
        LeaseId("lse_00000000000000000000000004".into()),
        FencingToken("42".into()),
        WorkerId("wrk_00000000000000000000000001".into()),
        WorkerInstanceId("wki_00000000000000000000000002".into()),
        WorkerSessionId("wsn_00000000000000000000000005".into()),
    )
    .expect("Artifact provenance")
}

fn artifact_attribution() -> ArtifactMeteringAttribution {
    ArtifactMeteringAttribution {
        organization_id: OrganizationId("org_00000000000000000000000001".into()),
        workspace_id: WorkspaceId("wsp_00000000000000000000000001".into()),
        project_id: ProjectId("prj_00000000000000000000000001".into()),
        repository_id: RepositoryId("rep_00000000000000000000000001".into()),
        delivery_id: Some(DeliveryId("dlv_00000000000000000000000001".into())),
        product_session_id: Some(ProductSessionId("psn_00000000000000000000000001".into())),
        user_id: UserId("usr_00000000000000000000000001".into()),
    }
}

fn pin_fixture() -> (
    PathBuf,
    ArtifactStore,
    winwincode_storage::ArtifactWriteReceipt,
    winwincode_storage::ValidatedGitSourceArtifact,
    String,
    String,
) {
    let root = temporary_directory("pin");
    let repositories = root.join("repositories");
    let repository = repositories.join("project-one");
    let (base, candidate) = repository_fixture(&repository);
    let bytes = CandidateSourceManifest::new(candidate.clone())
        .expect("candidate manifest")
        .encode()
        .expect("candidate manifest bytes");
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes)));
    let scope = winwincode_storage::ReceiptScopeKey::from_encoded(b"repository:one".to_vec())
        .expect("scope");
    let provenance = artifact_provenance();
    let artifact_id = ArtifactId("art_0000000000000000000000000C".into());
    let mut artifacts = ArtifactStore::open(
        root.join("artifact-catalog"),
        Box::new(FakeArtifactObjectStore::new()),
    )
    .expect("Artifact store");
    artifacts
        .open_artifact(ArtifactOpen::new(
            scope.clone(),
            ExecutionMessageId("xmsg_00000000000000000000000001".into()),
            RequestId("req_00000000000000000000000001".into()),
            artifact_id.clone(),
            "candidate",
            "application/vnd.winwincode.git-candidate+json",
            digest.clone(),
            bytes.len() as u64,
            Some("candidate.json".into()),
            provenance.clone(),
            artifact_attribution(),
            ArtifactRetention::Indefinite,
            1_800_000_000_000,
        ))
        .expect("Artifact open");
    let receipt = artifacts
        .append_chunk(&ArtifactChunk::new(
            scope.clone(),
            ExecutionMessageId("xmsg_00000000000000000000000002".into()),
            artifact_id,
            provenance.clone(),
            1_800_000_000_001,
            1,
            "application/vnd.winwincode.git-candidate+json",
            digest,
            bytes,
            true,
        ))
        .expect("final Artifact chunk");
    let object = artifacts
        .read_exact(&winwincode_storage::ArtifactAccess::new(
            scope,
            receipt.record().artifact_id().clone(),
            receipt.record().digest().clone(),
            provenance,
        ))
        .expect("complete Artifact");
    let resolver = LocalGitSourceResolver::open(&repositories).expect("Git resolver");
    let source = resolver
        .resolve_candidate(&object, "project-one", &base)
        .expect("validated source");
    (root, artifacts, receipt, source, base, candidate)
}

fn final_ack_digest(seed: u8) -> Sha256Digest {
    Sha256Digest(format!(
        "sha256:{}",
        char::from(seed).to_string().repeat(64)
    ))
}

fn release_after_restart(
    storage: &mut SqliteStorage,
    controlled_repository_root: &Path,
    repository: &Path,
    pin: &CandidateGitPinReceipt,
    candidate: &str,
) {
    let release_authority = CandidateGitReleaseAuthority::delivery_final_without_future_reads(
        DeliveryId("dlv_00000000000000000000000001".into()),
        CandidateGitTerminalOutcome::Delivered,
        final_ack_digest(b'b'),
        final_ack_digest(b'c'),
    )
    .expect("release authority");
    let foreign_authority = CandidateGitReleaseAuthority::delivery_final_without_future_reads(
        DeliveryId("dlv_00000000000000000000000002".into()),
        CandidateGitTerminalOutcome::Delivered,
        final_ack_digest(b'b'),
        final_ack_digest(b'c'),
    )
    .expect("foreign release authority");
    let foreign_error = {
        let mut retention = storage
            .git_candidate_retention(controlled_repository_root)
            .expect("retention for foreign release");
        retention
            .release_after_delivery_final(pin, &foreign_authority)
            .expect_err("foreign Delivery release must conflict")
    };
    assert_eq!(
        foreign_error.kind(),
        winwincode_storage::CandidateGitRetentionErrorKind::Conflict
    );
    assert_eq!(
        git(repository, &["rev-parse", pin.reference_name()]),
        candidate
    );
    let release = {
        let mut retention = storage
            .git_candidate_retention(controlled_repository_root)
            .expect("retention for release");
        retention
            .release_after_delivery_final(pin, &release_authority)
            .expect("release candidate")
    };
    assert_eq!(release.state(), CandidateGitRetentionState::Released);
    assert!(git_status_missing(repository, release.reference_name()));
}

#[test]
fn pins_candidate_before_gc_replays_after_restart_and_releases_after_read_closure() {
    let (root, artifacts, receipt, source, _base, candidate) = pin_fixture();
    let repository_root = root.join("repositories");
    let mut storage = SqliteStorage::open(root.join("control")).expect("Control Plane storage");
    let ack_digest = final_ack_digest(b'a');
    let pin = {
        let mut retention = storage
            .git_candidate_retention(&repository_root)
            .expect("candidate retention");
        retention
            .pin_after_final_artifact_ack(&receipt, &source, &ack_digest)
            .expect("pin candidate")
    };
    assert_eq!(pin.state(), CandidateGitRetentionState::Pinned);
    assert!(!pin.is_idempotent_replay());
    let delivery_pins = {
        let mut retention = storage
            .git_candidate_retention(&repository_root)
            .expect("candidate retention listing");
        retention
            .load_by_delivery(pin.delivery_id())
            .expect("candidate pins for Delivery")
    };
    assert_eq!(delivery_pins.len(), 1);
    assert_eq!(delivery_pins[0].artifact_id(), pin.artifact_id());
    assert_eq!(delivery_pins[0].reference_name(), pin.reference_name());
    assert_eq!(
        delivery_pins[0].candidate_commit_id(),
        pin.candidate_commit_id()
    );
    assert_eq!(delivery_pins[0].state(), CandidateGitRetentionState::Pinned);
    assert_eq!(
        git(
            &repository_root.join("project-one"),
            &["show-ref", "--hash", pin.reference_name()]
        ),
        candidate
    );

    let repository = repository_root.join("project-one");
    git(
        &repository,
        &["update-ref", "refs/heads/main", source.base_commit_id()],
    );
    git(&repository, &["gc", "--prune=now"]);
    assert_eq!(
        git(&repository, &["rev-parse", pin.reference_name()]),
        candidate
    );
    git(
        &repository,
        &["checkout", "-q", "--detach", pin.reference_name()],
    );
    assert_eq!(
        fs::read_to_string(repository.join("candidate.txt")).expect("pinned checkout"),
        "base\ncandidate\n"
    );

    Box::new(storage).close().expect("close retention storage");
    drop(artifacts);

    let mut restarted_storage =
        SqliteStorage::open(root.join("control")).expect("restart Control Plane storage");
    let replay = {
        let mut retention = restarted_storage
            .git_candidate_retention(&repository_root)
            .expect("restart candidate retention");
        retention
            .pin_after_final_artifact_ack(&receipt, &source, &ack_digest)
            .expect("idempotent pin replay")
    };
    assert_eq!(replay.reference_name(), pin.reference_name());
    assert!(replay.is_idempotent_replay());
    release_after_restart(
        &mut restarted_storage,
        &repository_root,
        &repository,
        &replay,
        &candidate,
    );
    Box::new(restarted_storage)
        .close()
        .expect("close restarted storage");
    fs::remove_dir_all(root).expect("fixture cleanup");
}

#[test]
fn changed_ack_or_source_identity_is_rejected_without_moving_the_reference() {
    let (root, artifacts, receipt, source, _base, _candidate) = pin_fixture();
    let repository_root = root.join("repositories");
    let mut storage = SqliteStorage::open(root.join("control")).expect("Control Plane storage");
    let ack_digest = final_ack_digest(b'd');
    let pin = {
        let mut retention = storage
            .git_candidate_retention(&repository_root)
            .expect("candidate retention");
        retention
            .pin_after_final_artifact_ack(&receipt, &source, &ack_digest)
            .expect("initial pin")
    };
    let changed_ack = final_ack_digest(b'e');
    let error = {
        let mut retention = storage
            .git_candidate_retention(&repository_root)
            .expect("candidate retention replay");
        retention
            .pin_after_final_artifact_ack(&receipt, &source, &changed_ack)
            .expect_err("changed final acknowledgement must conflict")
    };
    assert_eq!(
        error.kind(),
        winwincode_storage::CandidateGitRetentionErrorKind::Conflict
    );
    assert_eq!(
        git(
            &repository_root.join("project-one"),
            &["show-ref", "--hash", pin.reference_name()]
        ),
        pin.candidate_commit_id()
    );
    Box::new(storage).close().expect("close retention storage");
    drop(artifacts);
    fs::remove_dir_all(root).expect("fixture cleanup");
}

fn git_status_missing(repository: &Path, reference: &str) -> bool {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["rev-parse", "--verify", "--quiet", reference])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git show-ref");
    !output.status.success() && output.status.code() == Some(1) && output.stdout.is_empty()
}
