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
    ArtifactAccess, ArtifactChunk, ArtifactErrorKind, ArtifactMeteringAttribution, ArtifactOpen,
    ArtifactProvenance, ArtifactRetention, ArtifactStore, CandidateSourceManifest,
    FakeArtifactObjectStore, GitCandidateReviewFileEncoding, GitCandidateReviewFileStatus,
    GitSourcePathState, GitSourceResolver, LocalGitSourceResolver, ReceiptScopeKey,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-git-source-{name}-{}-{suffix}",
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
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
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
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .status()
        .expect("git commit");
    assert!(status.success());
}

fn repository_fixture(root: &Path) -> (String, String) {
    fs::create_dir_all(root).expect("repository root");
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["init", "-q", "-b", "main"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .status()
        .expect("git init");
    assert!(status.success());
    fs::create_dir_all(root.join("src")).expect("source directory");
    fs::write(root.join("src/app.txt"), b"base\n").expect("base source");
    git(root, &["add", "--", "src/app.txt"]);
    commit(root, "base", "2026-08-25T00:00:00Z");
    let base = text(git(root, &["rev-parse", "HEAD"]));

    fs::write(root.join("src/app.txt"), b"base\ncandidate\n").expect("candidate source");
    fs::write(root.join("src/blob.bin"), [0_u8, 1, 2, 0, 255]).expect("candidate binary");
    fs::write(
        root.join("src/legacy.txt"),
        [b'l', b'a', b't', b'i', b'n', b'-', 0xff, b'\n'],
    )
    .expect("candidate unknown 8-bit text");
    git(
        root,
        &["add", "--", "src/app.txt", "src/blob.bin", "src/legacy.txt"],
    );
    commit(root, "candidate", "2026-08-25T00:01:00Z");
    let candidate = text(git(root, &["rev-parse", "HEAD"]));
    (base, candidate)
}

fn scope() -> ReceiptScopeKey {
    ReceiptScopeKey::from_encoded(b"repository:one".to_vec()).expect("scope")
}

fn message(value: u64) -> ExecutionMessageId {
    ExecutionMessageId(format!("xmsg_{value:026}"))
}

fn request(value: u64) -> RequestId {
    RequestId(format!("req_{value:026}"))
}

fn provenance() -> ArtifactProvenance {
    ArtifactProvenance::execution_job(
        ExecutionJobId("job_00000000000000000000000003".into()),
        1,
        LeaseId("lse_00000000000000000000000004".into()),
        FencingToken("42".into()),
        WorkerId("wrk_00000000000000000000000001".into()),
        WorkerInstanceId("wki_00000000000000000000000002".into()),
        WorkerSessionId("wsn_00000000000000000000000005".into()),
    )
    .expect("provenance")
}

fn artifact_chunk(
    scope_key: ReceiptScopeKey,
    message_id: ExecutionMessageId,
    artifact_id: ArtifactId,
    sequence: u64,
    digest: Sha256Digest,
    bytes: Vec<u8>,
    is_final: bool,
) -> ArtifactChunk {
    ArtifactChunk::new(
        scope_key,
        message_id,
        artifact_id,
        provenance(),
        1_100 + sequence,
        sequence,
        "application/octet-stream",
        digest,
        bytes,
        is_final,
    )
}

fn metering_attribution() -> ArtifactMeteringAttribution {
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

#[test]
#[allow(clippy::too_many_lines)]
fn local_git_resolver_rebuilds_identity_from_exact_candidate_artifact() {
    let root = temporary_directory("rebuild");
    let repositories = root.join("repositories");
    let repository = repositories.join("project-one");
    let (base_commit, candidate_commit) = repository_fixture(&repository);
    let manifest = CandidateSourceManifest::new(candidate_commit.clone())
        .expect("candidate source manifest")
        .encode()
        .expect("manifest encoding");
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&manifest)));
    let chunk_digest = digest.clone();
    let artifact_id = ArtifactId("art_0000000000000000000000000C".into());
    let artifact_scope = scope();
    let artifact_provenance = provenance();
    let mut artifacts = ArtifactStore::open(
        root.join("catalog"),
        Box::new(FakeArtifactObjectStore::new()),
    )
    .expect("artifact store");
    artifacts
        .open_artifact(ArtifactOpen::new(
            artifact_scope.clone(),
            message(1),
            request(1),
            artifact_id.clone(),
            "candidate",
            "application/vnd.winwincode.git-candidate+json",
            digest.clone(),
            manifest.len() as u64,
            Some("candidate.json".into()),
            artifact_provenance.clone(),
            metering_attribution(),
            ArtifactRetention::Indefinite,
            1_800_000_000_000,
        ))
        .expect("candidate artifact open");
    artifacts
        .append_chunk(&artifact_chunk(
            artifact_scope.clone(),
            message(2),
            artifact_id.clone(),
            1,
            chunk_digest,
            manifest,
            true,
        ))
        .expect("candidate artifact complete");
    let object = artifacts
        .read_exact(&ArtifactAccess::new(
            artifact_scope,
            artifact_id,
            digest,
            artifact_provenance,
        ))
        .expect("candidate artifact read");

    let resolver = LocalGitSourceResolver::open(&repositories).expect("Git resolver");
    let source = resolver
        .resolve_candidate(&object, "project-one", &base_commit)
        .expect("rebuilt candidate source");

    assert_eq!(source.base_commit_id(), base_commit);
    assert_eq!(source.candidate_commit_id(), candidate_commit);
    assert_eq!(
        source.base_tree_id(),
        text(git(
            &repository,
            &["rev-parse", &format!("{base_commit}^{{tree}}")]
        ))
    );
    assert_eq!(
        source.candidate_tree_id(),
        text(git(
            &repository,
            &["rev-parse", &format!("{candidate_commit}^{{tree}}")]
        ))
    );
    let expected_diff = git(
        &repository,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            "--full-index",
            &format!("{base_commit}..{candidate_commit}"),
        ],
    );
    assert_eq!(
        source.diff_sha256(),
        format!("{:x}", Sha256::digest(expected_diff))
    );
    assert_eq!(source.changed_paths().len(), 3);
    assert_eq!(source.changed_paths()[0].path(), "src/app.txt");
    assert_eq!(
        source.changed_paths()[0].state(),
        GitSourcePathState::Present
    );
    assert_eq!(
        source.changed_paths()[0].object_id(),
        Some(text(git(
            &repository,
            &[
                "rev-parse",
                "--verify",
                &format!("{candidate_commit}:src/app.txt")
            ]
        )))
        .as_deref()
    );
    assert_eq!(source.changed_hunks().len(), 3);
    assert_eq!(source.changed_hunks()[0].file_path(), "src/app.txt");
    let review = resolver
        .candidate_review(&source)
        .expect("Candidate changed-file review");
    assert_eq!(review.candidate_commit_id(), candidate_commit);
    assert_eq!(review.candidate_tree_id(), source.candidate_tree_id());
    assert_eq!(review.diff_sha256(), source.diff_sha256());
    assert_eq!(review.files().len(), 3);
    assert_eq!(review.files()[0].path(), "src/app.txt");
    assert_eq!(review.files()[0].old_path(), None);
    assert_eq!(
        review.files()[0].status(),
        GitCandidateReviewFileStatus::Modified
    );
    assert_eq!(review.files()[0].additions(), Some(1));
    assert_eq!(review.files()[0].deletions(), Some(0));
    assert!(!review.files()[0].is_binary());
    assert_eq!(
        review.files()[0].encoding(),
        GitCandidateReviewFileEncoding::Utf8
    );
    assert_eq!(review.files()[1].path(), "src/blob.bin");
    assert_eq!(
        review.files()[1].status(),
        GitCandidateReviewFileStatus::Added
    );
    assert_eq!(review.files()[1].additions(), None);
    assert_eq!(review.files()[1].deletions(), None);
    assert!(review.files()[1].is_binary());
    assert_eq!(
        review.files()[1].encoding(),
        GitCandidateReviewFileEncoding::Binary
    );
    assert_eq!(review.files()[2].path(), "src/legacy.txt");
    assert!(!review.files()[2].is_binary());
    assert_eq!(
        review.files()[2].encoding(),
        GitCandidateReviewFileEncoding::Unknown8Bit
    );
    let path_diff = resolver
        .candidate_diff(&source, "src/app.txt")
        .expect("Candidate path diff");
    assert_eq!(path_diff.path(), "src/app.txt");
    assert!(!path_diff.is_binary());
    assert_eq!(
        path_diff.file_diff_sha256(),
        format!("{:x}", Sha256::digest(path_diff.bytes()))
    );
    assert!(String::from_utf8_lossy(path_diff.bytes()).contains("+candidate"));
    let binary_diff = resolver
        .candidate_diff(&source, "src/blob.bin")
        .expect("Candidate binary diff");
    assert!(binary_diff.is_binary());
    assert_eq!(binary_diff.status(), GitCandidateReviewFileStatus::Added);
    assert_eq!(
        binary_diff.encoding(),
        GitCandidateReviewFileEncoding::Binary
    );
    assert!(binary_diff.bytes().starts_with(b"diff --git "));
    let unknown_text_diff = resolver
        .candidate_diff(&source, "src/legacy.txt")
        .expect("Candidate unknown 8-bit diff");
    assert!(!unknown_text_diff.is_binary());
    assert_eq!(
        unknown_text_diff.encoding(),
        GitCandidateReviewFileEncoding::Unknown8Bit
    );
    let traversal = resolver
        .candidate_diff(&source, "../secret")
        .expect_err("path traversal must not select Candidate data");
    assert_eq!(traversal.kind(), ArtifactErrorKind::InvalidInput);
    assert_eq!(
        source.artifact().artifact_id().0,
        "art_0000000000000000000000000C"
    );

    artifacts.close().expect("artifact close");
    fs::remove_dir_all(root).expect("fixture release");
}

#[test]
fn local_git_resolver_rejects_a_noncanonical_candidate_manifest() {
    let root = temporary_directory("noncanonical-manifest");
    let repositories = root.join("repositories");
    let repository = repositories.join("project-one");
    let (base_commit, candidate_commit) = repository_fixture(&repository);
    let manifest = format!("{{\"candidateCommitId\":\"{candidate_commit}\",\"schemaVersion\":1}}")
        .into_bytes();
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&manifest)));
    let artifact_id = ArtifactId("art_0000000000000000000000000D".into());
    let artifact_scope = scope();
    let artifact_provenance = provenance();
    let mut artifacts = ArtifactStore::open(
        root.join("catalog"),
        Box::new(FakeArtifactObjectStore::new()),
    )
    .expect("artifact store");
    artifacts
        .open_artifact(ArtifactOpen::new(
            artifact_scope.clone(),
            message(3),
            request(3),
            artifact_id.clone(),
            "candidate",
            "application/vnd.winwincode.git-candidate+json",
            digest.clone(),
            manifest.len() as u64,
            Some("candidate.json".into()),
            artifact_provenance.clone(),
            metering_attribution(),
            ArtifactRetention::Indefinite,
            1_800_000_000_000,
        ))
        .expect("candidate artifact open");
    artifacts
        .append_chunk(&artifact_chunk(
            artifact_scope.clone(),
            message(4),
            artifact_id.clone(),
            1,
            digest.clone(),
            manifest,
            true,
        ))
        .expect("candidate artifact complete");
    let object = artifacts
        .read_exact(&ArtifactAccess::new(
            artifact_scope,
            artifact_id,
            digest,
            artifact_provenance,
        ))
        .expect("candidate artifact read");

    let resolver = LocalGitSourceResolver::open(&repositories).expect("Git resolver");
    let error = resolver
        .resolve_candidate(&object, "project-one", &base_commit)
        .expect_err("field-reordered manifest must not become a source fact");
    assert_eq!(error.kind(), ArtifactErrorKind::InvalidInput);

    artifacts.close().expect("artifact close");
    fs::remove_dir_all(root).expect("fixture release");
}

#[test]
fn local_git_resolver_ignores_repository_replace_refs() {
    let root = temporary_directory("replace-ref");
    let repositories = root.join("repositories");
    let repository = repositories.join("project-one");
    let (base_commit, candidate_commit) = repository_fixture(&repository);
    let candidate_tree = text(git(
        &repository,
        &["rev-parse", &format!("{candidate_commit}^{{tree}}")],
    ));
    git(&repository, &["replace", &candidate_commit, &base_commit]);

    let manifest = CandidateSourceManifest::new(candidate_commit.clone())
        .expect("candidate source manifest")
        .encode()
        .expect("manifest encoding");
    let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(&manifest)));
    let artifact_id = ArtifactId("art_0000000000000000000000000E".into());
    let artifact_scope = scope();
    let artifact_provenance = provenance();
    let mut artifacts = ArtifactStore::open(
        root.join("catalog"),
        Box::new(FakeArtifactObjectStore::new()),
    )
    .expect("artifact store");
    artifacts
        .open_artifact(ArtifactOpen::new(
            artifact_scope.clone(),
            message(5),
            request(5),
            artifact_id.clone(),
            "candidate",
            "application/vnd.winwincode.git-candidate+json",
            digest.clone(),
            manifest.len() as u64,
            Some("candidate.json".into()),
            artifact_provenance.clone(),
            metering_attribution(),
            ArtifactRetention::Indefinite,
            1_800_000_000_000,
        ))
        .expect("candidate artifact open");
    artifacts
        .append_chunk(&artifact_chunk(
            artifact_scope.clone(),
            message(6),
            artifact_id.clone(),
            1,
            digest.clone(),
            manifest,
            true,
        ))
        .expect("candidate artifact complete");
    let object = artifacts
        .read_exact(&ArtifactAccess::new(
            artifact_scope,
            artifact_id,
            digest,
            artifact_provenance,
        ))
        .expect("candidate artifact read");

    let resolver = LocalGitSourceResolver::open(&repositories).expect("Git resolver");
    let source = resolver
        .resolve_candidate(&object, "project-one", &base_commit)
        .expect("replace refs must not reinterpret controlled source objects");
    assert_eq!(source.candidate_commit_id(), candidate_commit);
    assert_eq!(source.candidate_tree_id(), candidate_tree);

    artifacts.close().expect("artifact close");
    fs::remove_dir_all(root).expect("fixture release");
}

#[test]
fn local_git_resolver_ignores_inherited_repository_environment() {
    let root = temporary_directory("inherited-git-environment");
    let foreign_repository = root.join("foreign");
    repository_fixture(&foreign_repository);
    let output = Command::new(std::env::current_exe().expect("current test executable"))
        .args([
            "--exact",
            "local_git_resolver_rebuilds_identity_from_exact_candidate_artifact",
            "--nocapture",
        ])
        .env("GIT_DIR", foreign_repository.join(".git"))
        .env("GIT_WORK_TREE", &foreign_repository)
        .output()
        .expect("isolated Git environment probe");
    assert!(
        output.status.success(),
        "controlled source resolution inherited repository authority:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_dir_all(root).expect("fixture release");
}
