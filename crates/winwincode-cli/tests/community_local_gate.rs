// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};
use winwincode_cli::{
    AttachRequest, Attachment, BaselineChoice, CommunityGateFailureCategory,
    CommunityGateFailureCode, CommunityLocalEnvironment, CommunityLocalGateRequest,
    LocalLauncherPort, SetupOutcome, SystemLocalLauncher, run_community_local_gate,
};
use winwincode_delivery::{
    application::{
        stage::{
            TerminalArtifactReference, TerminalOutcomeStatus,
            test_support::{
                active_lease_identity, delivery_terminal_outcome_facts, session_binding_authority,
                terminal_outcome_metadata, terminal_worker_outcome,
            },
        },
        verdict::{
            SubmitVerdictFacts, compute_verdict_transition,
            test_support::{VerdictFixtureOutcome, verdict_facts_fixture, verdict_fixture},
        },
    },
    domain::{
        DELIVERY_SCHEMA_VERSION, Delivery, FrozenDeliveryCandidate, RepositoryKind, RepositoryRef,
        StageRunStatus, candidate::freeze_delivery_candidate_from_source,
    },
    store::{
        CreateDelivery, DeliveryCommand, DeliveryCommandPort, DeliveryStore,
        InMemoryDeliveryJournal, SubmitDeliveryVerdict,
    },
};
use winwincode_domain::{
    ArtifactId, CodexThreadId, DeliveryId, ExecutionAckSequence, ExecutionJobId,
    ExecutionMessageId, FencingToken, Instant, LeaseId, OrganizationId, ProductSessionId,
    ProjectId, RepositoryId, RequestId, Sha256Digest, StageRunId, UserId, WorkerId,
    WorkerInstanceId, WorkerSessionId, WorkspaceId,
};
use winwincode_evidence_export::{
    ArtifactSource, ExportCapacity, ExportClassification, verify_evidence_archive,
};
use winwincode_repository_context::{
    RepositoryContext, RepositoryContextPort, RepositoryContextQuery, RepositoryContextScanner,
};
use winwincode_storage::{
    ArtifactAccess, ArtifactChunk, ArtifactMeteringAttribution, ArtifactOpen, ArtifactProvenance,
    ArtifactRetention, ArtifactStore, CandidateSourceManifest, FakeArtifactObjectStore,
    LocalGitSourceResolver, ReceiptScopeKey,
};
use winwincode_test_assets::manifest::{
    TEST_ASSET_MANIFEST_SCHEMA_VERSION, TestAsset, TestAssetAuthority, TestAssetEvidenceBinding,
    TestAssetGate, TestAssetLifecycle, TestAssetManifest, TestAssetMutability,
    TestAssetVerdictBinding,
};
use winwincode_test_assets::{ChangedFile, TestManipulationFinding, analyze_test_changes};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "winwincode-community-gate-{label}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct CommunityFixture {
    directory: TestDirectory,
    repository: PathBuf,
    attachment: Attachment,
    context: RepositoryContext,
    delivery: Delivery,
    candidate: FrozenDeliveryCandidate,
    manifest: TestAssetManifest,
    test_bindings: Vec<TestAssetEvidenceBinding>,
    verdict_binding: TestAssetVerdictBinding,
    runtime_log: PathBuf,
}

impl CommunityFixture {
    fn new(label: &str, outcome: VerdictFixtureOutcome) -> Self {
        let directory = TestDirectory::new(label);
        let (repositories, repository, attachment, context, base_commit, candidate_commit) =
            prepare_repository_environment(&directory.0);

        let fixture = verdict_fixture(
            &DeliveryId("dlv_01J00000000000000000000906".into()),
            outcome,
        );
        let (delivery, producer_id, execution_job_id, worker_session_id, codex_thread_id) =
            prepare_delivery(fixture.delivery, &base_commit);
        let (candidate, runtime_log) = freeze_local_candidate(
            &directory.0,
            &repositories,
            &delivery,
            &candidate_commit,
            &base_commit,
            producer_id,
            execution_job_id,
            worker_session_id,
            codex_thread_id,
        );
        let facts = verdict_facts_fixture(&delivery, &candidate, outcome);
        let produced_at = delivery.snapshot().updated_at_millis + 100;
        let transition = compute_verdict_transition(
            &delivery,
            SubmitVerdictFacts {
                expected_revision: delivery.revision(),
                candidate: &candidate,
                verification: facts.verification(),
                evidence: facts.evidence(),
                produced_at_millis: produced_at,
            },
        )
        .expect("compute independent Verdict transition");
        let journal = Arc::new(InMemoryDeliveryJournal::new());
        let store = DeliveryStore::new(journal);
        store
            .execute(DeliveryCommand::SeedForTest(CreateDelivery {
                request_id: RequestId("community-delivery-create".into()),
                request_digest: "1".repeat(64),
                snapshot: delivery,
            }))
            .expect("create deterministic Delivery journal");
        let delivery = store
            .execute(DeliveryCommand::SubmitVerdict(Box::new(
                SubmitDeliveryVerdict {
                    request_id: RequestId("community-verdict-submit".into()),
                    request_digest: "2".repeat(64),
                    expected_revision: transition.delivery().revision() - 1,
                    transition,
                },
            )))
            .expect("persist computed Verdict")
            .snapshot;

        let (manifest, test_bindings, verdict_binding) =
            build_test_asset_bindings(&repository, &delivery, &candidate, candidate_commit);

        Self {
            directory,
            repository,
            attachment,
            context,
            delivery,
            candidate,
            manifest,
            test_bindings,
            verdict_binding,
            runtime_log,
        }
    }

    fn request<'fixture>(
        &'fixture self,
        export_root: &'fixture Path,
        findings: &'fixture [TestManipulationFinding],
        environment: CommunityLocalEnvironment,
        capacity: ExportCapacity,
    ) -> CommunityLocalGateRequest<'fixture> {
        let bytes = fs::read(&self.runtime_log).expect("runtime log bytes");
        CommunityLocalGateRequest {
            repository_root: &self.repository,
            attachment: &self.attachment,
            repository_context: &self.context,
            delivery: &self.delivery,
            candidate: &self.candidate,
            test_asset_manifest: &self.manifest,
            test_evidence_bindings: &self.test_bindings,
            test_verdict_binding: &self.verdict_binding,
            test_manipulation_findings: findings,
            export_root,
            package_id: "community-local-001",
            artifacts: vec![ArtifactSource {
                artifact_id: "art_runtime_001".into(),
                logical_name: "verification.log".into(),
                source_path: self.runtime_log.clone(),
                expected_sha256: digest(&bytes),
                expected_bytes: bytes.len() as u64,
                classification: ExportClassification::Confidential,
            }],
            capacity,
            create_archive: true,
            environment,
        }
    }
}

fn build_test_asset_bindings(
    repository: &Path,
    delivery: &Delivery,
    candidate: &FrozenDeliveryCandidate,
    candidate_commit: String,
) -> (
    TestAssetManifest,
    Vec<TestAssetEvidenceBinding>,
    TestAssetVerdictBinding,
) {
    let verifier_evidence = delivery
        .snapshot()
        .evidence
        .iter()
        .find(|evidence| {
            delivery
                .snapshot()
                .stage_runs
                .iter()
                .any(|stage| stage.id == evidence.stage_run_id && stage.role == "verifier")
        })
        .expect("verifier test Evidence");
    let test_source =
        fs::read(repository.join("tests/community.rs")).expect("read canonical TestAsset source");
    let manifest = TestAssetManifest {
        schema_version: TEST_ASSET_MANIFEST_SCHEMA_VERSION,
        id: "community-local-tests".into(),
        revision: 1,
        candidate_ref: candidate.candidate_ref().into(),
        source_commit: candidate_commit,
        assets: vec![TestAsset {
            id: "canonical-community-test".into(),
            owner: "acceptance-owner".into(),
            scope: vec!["community-local".into()],
            purpose: "Verify the local candidate without hosted dependencies".into(),
            authority: TestAssetAuthority::Canonical,
            mutability: TestAssetMutability::Protected,
            lifecycle: TestAssetLifecycle::Active,
            gate: TestAssetGate::RequirementBlocking,
            requirement_refs: vec![delivery.snapshot().spec.acceptance_criteria[0].id.0.clone()],
            source_path: "tests/community.rs".into(),
            content_sha256: digest(&test_source),
        }],
    };
    let test_bindings = vec![
        manifest
            .bind_evidence(verifier_evidence, "canonical-community-test")
            .expect("bind verifier Evidence to canonical TestAsset"),
    ];
    let verdict = delivery
        .snapshot()
        .verdict
        .as_ref()
        .expect("computed Verdict");
    let verdict_binding = TestAssetVerdictBinding::new(verdict, &manifest, &test_bindings)
        .expect("bind Verdict to exact TestAsset manifest");
    (manifest, test_bindings, verdict_binding)
}

fn prepare_repository_environment(
    root: &Path,
) -> (
    PathBuf,
    PathBuf,
    Attachment,
    RepositoryContext,
    String,
    String,
) {
    let repositories = root.join("repositories");
    let repository = repositories.join("project-one");
    let base_commit = create_base_repository(&repository);
    let launcher = SystemLocalLauncher::new(root.join("state"));
    let attachment = match launcher
        .attach_repository(&AttachRequest {
            repository_path: repository.clone(),
            baseline: Some(BaselineChoice::Head),
            confirm_snapshot: false,
        })
        .expect("attach local repository")
    {
        SetupOutcome::Ready { attachment } => attachment.attachment,
        other => panic!("unexpected attach outcome: {other:?}"),
    };
    assert!(!attachment.remote_configured);
    let context = RepositoryContextScanner::default()
        .inspect(&RepositoryContextQuery::new(&repository, &base_commit))
        .expect("baseline RepositoryContext");
    let candidate_commit = commit_candidate(&repository);
    (
        repositories,
        repository,
        attachment,
        context,
        base_commit,
        candidate_commit,
    )
}

#[test]
fn no_remote_flow_exports_repeatable_offline_evidence_and_local_merge_instructions() {
    let fixture = CommunityFixture::new("pass", VerdictFixtureOutcome::Pass);
    let clean_findings = analyze_test_changes(&[ChangedFile {
        path: "src/app.txt",
        baseline: Some("base\n"),
        candidate: Some("base\ncandidate\n"),
    }]);
    assert!(clean_findings.is_empty());
    assert!(git(&fixture.repository, &["remote"]).is_empty());
    let capacity = ExportCapacity {
        available_bytes: 10_000_000,
        reserve_bytes: 1_000,
        warning_below_bytes: 1_000,
    };
    let first = run_community_local_gate(&fixture.request(
        &fixture.directory.0.join("first"),
        &clean_findings,
        CommunityLocalEnvironment::default(),
        capacity,
    ))
    .expect("first Community closure");
    let second = run_community_local_gate(&fixture.request(
        &fixture.directory.0.join("second"),
        &clean_findings,
        CommunityLocalEnvironment::default(),
        capacity,
    ))
    .expect("second Community closure");

    assert_eq!(first.manifest, second.manifest);
    assert_eq!(first.source_trace, second.source_trace);
    assert_eq!(first.export.manifest_sha256, second.export.manifest_sha256);
    assert_eq!(first.manifest.trace_record_count, 4);
    assert!(first.manifest.stable_bytes);
    assert!(first.source_trace.repository_baseline_sha.len() == 40);
    assert_eq!(
        fs::read(first.export.archive_path.as_ref().expect("first archive"))
            .expect("first archive bytes"),
        fs::read(second.export.archive_path.as_ref().expect("second archive"))
            .expect("second archive bytes")
    );
    assert_eq!(
        verify_evidence_archive(first.export.archive_path.as_ref().expect("archive"))
            .expect("offline archive verification"),
        first.manifest
    );
    let guide =
        fs::read_to_string(first.export.package_path.join("merge-guide.md")).expect("merge guide");
    assert!(guide.contains(&format!(
        "git merge --no-ff {}",
        fixture.candidate.candidate_commit_id()
    )));
    assert!(guide.contains(&format!(
        "git cherry-pick {}",
        fixture.candidate.candidate_commit_id()
    )));
}

#[test]
fn failures_distinguish_implementation_acceptance_and_environment_ownership() {
    let fixture = CommunityFixture::new("categories", VerdictFixtureOutcome::Pass);
    let capacity = ExportCapacity {
        available_bytes: 10_000_000,
        reserve_bytes: 1_000,
        warning_below_bytes: 0,
    };
    let blocked = analyze_test_changes(&[ChangedFile {
        path: "tests/community.test.ts",
        baseline: Some("assert!(true);\n"),
        candidate: None,
    }]);
    assert!(!blocked.is_empty());
    let error = run_community_local_gate(&fixture.request(
        &fixture.directory.0.join("blocked"),
        &blocked,
        CommunityLocalEnvironment::default(),
        capacity,
    ))
    .expect_err("test deletion must block");
    assert_eq!(
        error.category(),
        CommunityGateFailureCategory::Implementation
    );
    assert_eq!(error.code(), CommunityGateFailureCode::TestPolicyViolation);

    let review = analyze_test_changes(&[ChangedFile {
        path: "vitest.config.ts",
        baseline: Some("include: ['tests/**']\n"),
        candidate: Some("include: ['checks/**']\n"),
    }]);
    let error = run_community_local_gate(&fixture.request(
        &fixture.directory.0.join("review"),
        &review,
        CommunityLocalEnvironment::default(),
        capacity,
    ))
    .expect_err("ambiguous test discovery change must require acceptance review");
    assert_eq!(error.category(), CommunityGateFailureCategory::Acceptance);
    assert_eq!(
        error.code(),
        CommunityGateFailureCode::AcceptanceReviewRequired
    );

    let error = run_community_local_gate(&fixture.request(
        &fixture.directory.0.join("telemetry"),
        &[],
        CommunityLocalEnvironment {
            commercial_account_configured: false,
            vendor_telemetry_enabled: true,
        },
        capacity,
    ))
    .expect_err("vendor telemetry is outside the local Community closure");
    assert_eq!(error.category(), CommunityGateFailureCategory::Environment);
    assert_eq!(
        error.code(),
        CommunityGateFailureCode::ExternalDependencyPresent
    );

    let error = run_community_local_gate(&fixture.request(
        &fixture.directory.0.join("disk"),
        &[],
        CommunityLocalEnvironment::default(),
        ExportCapacity {
            available_bytes: 1,
            reserve_bytes: 0,
            warning_below_bytes: 0,
        },
    ))
    .expect_err("insufficient disk must be environmental");
    assert_eq!(error.category(), CommunityGateFailureCategory::Environment);
    assert_eq!(error.code(), CommunityGateFailureCode::EvidenceExportFailed);

    run_git(
        &fixture.repository,
        &["remote", "add", "origin", "file:///local-fixture-only"],
    );
    let error = run_community_local_gate(&fixture.request(
        &fixture.directory.0.join("remote-added-after-attach"),
        &[],
        CommunityLocalEnvironment::default(),
        capacity,
    ))
    .expect_err("a Remote added after attachment must remain outside the local closure");
    assert_eq!(error.category(), CommunityGateFailureCategory::Environment);
    assert_eq!(
        error.code(),
        CommunityGateFailureCode::ExternalDependencyPresent
    );
}

#[test]
fn no_remote_never_turns_a_failing_verdict_into_a_pass() {
    let fixture = CommunityFixture::new("fail-verdict", VerdictFixtureOutcome::Fail);
    assert!(!fixture.attachment.remote_configured);
    let error = run_community_local_gate(&fixture.request(
        &fixture.directory.0.join("output"),
        &[],
        CommunityLocalEnvironment::default(),
        ExportCapacity {
            available_bytes: 10_000_000,
            reserve_bytes: 1_000,
            warning_below_bytes: 0,
        },
    ))
    .expect_err("failing local Verdict must remain failing");
    assert_eq!(error.category(), CommunityGateFailureCategory::Acceptance);
    assert_eq!(error.code(), CommunityGateFailureCode::VerdictNotPassing);
}

fn create_base_repository(repository: &Path) -> String {
    fs::create_dir_all(repository.join("src")).expect("source directory");
    fs::create_dir_all(repository.join("tests")).expect("test directory");
    run_git(repository, &["init", "-q", "-b", "main"]);
    fs::write(
        repository.join("Cargo.toml"),
        "[package]\nname='community-fixture'\nversion='0.0.0'\n",
    )
    .expect("Cargo manifest");
    fs::write(repository.join("src/app.txt"), b"base\n").expect("base source");
    fs::write(
        repository.join("tests/community.rs"),
        b"#[test]\nfn local() { assert_eq!(2 + 2, 4); }\n",
    )
    .expect("canonical test");
    run_git(repository, &["add", "--", "."]);
    commit(repository, "base", "2026-08-25T00:00:00Z");
    text(git(repository, &["rev-parse", "HEAD"]))
}

fn commit_candidate(repository: &Path) -> String {
    fs::write(repository.join("src/app.txt"), b"base\ncandidate\n").expect("candidate source");
    run_git(repository, &["add", "--", "src/app.txt"]);
    commit(repository, "candidate", "2026-08-25T00:01:00Z");
    text(git(repository, &["rev-parse", "HEAD"]))
}

fn prepare_delivery(
    delivery: Delivery,
    base_commit: &str,
) -> (
    Delivery,
    StageRunId,
    ExecutionJobId,
    WorkerSessionId,
    Option<CodexThreadId>,
) {
    let mut snapshot = delivery.into_snapshot();
    snapshot.spec.repository = RepositoryRef {
        schema_version: DELIVERY_SCHEMA_VERSION,
        kind: RepositoryKind::LocalGit,
        locator: "project-one".into(),
    };
    snapshot.spec.base_revision = base_commit.into();
    let producer = snapshot
        .stage_runs
        .iter_mut()
        .find(|run| run.role == "executor")
        .expect("executor");
    producer.id = StageRunId("run_00000000000000000000000906".into());
    producer.status = StageRunStatus::Succeeded;
    let producer_id = producer.id.clone();
    let binding = snapshot
        .session_bindings
        .iter_mut()
        .find(|binding| binding.id.0 == "binding-executor-1")
        .expect("executor binding");
    binding.stage_run_id = producer_id.clone();
    binding.product_session_id = ProductSessionId("psn_00000000000000000000000906".into());
    binding.execution_job_id = ExecutionJobId("job_00000000000000000000000906".into());
    binding.worker_session_id = Some(WorkerSessionId("wsn_00000000000000000000000906".into()));
    binding.codex_thread_id = Some(CodexThreadId("cdx_00000000000000000000000906".into()));
    let execution_job_id = binding.execution_job_id.clone();
    let worker_session_id = binding.worker_session_id.clone().expect("WorkerSession");
    let codex_thread_id = binding.codex_thread_id.clone();
    (
        Delivery::try_from_snapshot(snapshot).expect("local verifying Delivery"),
        producer_id,
        execution_job_id,
        worker_session_id,
        codex_thread_id,
    )
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn freeze_local_candidate(
    root: &Path,
    repositories: &Path,
    delivery: &Delivery,
    candidate_commit: &str,
    base_commit: &str,
    producer_id: StageRunId,
    execution_job_id: ExecutionJobId,
    worker_session_id: WorkerSessionId,
    codex_thread_id: Option<CodexThreadId>,
) -> (FrozenDeliveryCandidate, PathBuf) {
    let artifact_id = ArtifactId("art_00000000000000000000000906".into());
    let manifest = CandidateSourceManifest::new(candidate_commit.to_owned())
        .expect("candidate manifest")
        .encode()
        .expect("manifest encoding");
    let manifest_digest = Sha256Digest(format!("sha256:{}", digest(&manifest)));
    let scope = ReceiptScopeKey::from_encoded(b"repository:project-one".to_vec()).expect("scope");
    let provenance = ArtifactProvenance::execution_job(
        execution_job_id,
        1,
        LeaseId("lse_00000000000000000000000906".into()),
        FencingToken("906".into()),
        WorkerId("wrk_00000000000000000000000906".into()),
        WorkerInstanceId("wki_00000000000000000000000906".into()),
        worker_session_id,
    )
    .expect("Artifact provenance");
    let finished_at = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| run.id == producer_id)
        .and_then(|run| run.finished_at_millis)
        .expect("executor finish");
    let mut artifacts = ArtifactStore::open(
        root.join("artifact-catalog"),
        Box::new(FakeArtifactObjectStore::new()),
    )
    .expect("Artifact store");
    artifacts
        .open_artifact(ArtifactOpen::new(
            scope.clone(),
            ExecutionMessageId("xmsg_00000000000000000000000906".into()),
            RequestId("req_00000000000000000000000906".into()),
            artifact_id.clone(),
            "candidate",
            "application/vnd.winwincode.git-candidate+json",
            manifest_digest.clone(),
            manifest.len() as u64,
            Some("candidate.json".into()),
            provenance.clone(),
            ArtifactMeteringAttribution {
                organization_id: OrganizationId("org_00000000000000000000000906".into()),
                workspace_id: WorkspaceId("wsp_00000000000000000000000906".into()),
                project_id: ProjectId("prj_00000000000000000000000906".into()),
                repository_id: RepositoryId("rep_00000000000000000000000906".into()),
                delivery_id: Some(delivery.id().clone()),
                product_session_id: Some(ProductSessionId("psn_00000000000000000000000906".into())),
                user_id: UserId("usr_00000000000000000000000906".into()),
            },
            ArtifactRetention::Indefinite,
            finished_at - 1,
        ))
        .expect("Artifact open");
    artifacts
        .append_chunk(&ArtifactChunk::new(
            scope.clone(),
            ExecutionMessageId("xmsg_00000000000000000000000907".into()),
            artifact_id.clone(),
            provenance.clone(),
            finished_at,
            1,
            "application/octet-stream",
            manifest_digest.clone(),
            manifest,
            true,
        ))
        .expect("Artifact complete");
    let object = artifacts
        .read_exact(&ArtifactAccess::new(
            scope,
            artifact_id.clone(),
            manifest_digest.clone(),
            provenance.clone(),
        ))
        .expect("Artifact read");
    let source = LocalGitSourceResolver::open(repositories)
        .expect("source resolver")
        .resolve_candidate(&object, "project-one", base_commit)
        .expect("rebuilt local source");
    let lease = active_lease_identity(
        provenance.execution_job_id().clone(),
        provenance.attempt(),
        provenance.lease_id().clone(),
        provenance.fencing_token().clone(),
        provenance.worker_id().clone(),
        provenance.worker_instance_id().clone(),
        provenance.worker_session_id().clone(),
    );
    let authority = session_binding_authority(
        lease,
        Instant("2026-08-25T00:00:00.000Z".into()),
        Instant("2026-08-25T01:00:00.000Z".into()),
    );
    let outcome = delivery_terminal_outcome_facts(
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
                    artifact_id,
                    digest: manifest_digest,
                }],
            ),
        ),
    );
    let candidate = freeze_delivery_candidate_from_source(delivery, &source, &outcome)
        .expect("freeze candidate from exact local source");
    artifacts.close().expect("Artifact close");
    let runtime_log = root.join("verification.log");
    fs::write(
        &runtime_log,
        b"independent verifier completed with status pass\n",
    )
    .expect("runtime log");
    (candidate, runtime_log)
}

fn run_git(repository: &Path, arguments: &[&str]) {
    let output = git_output(repository, arguments);
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git(repository: &Path, arguments: &[&str]) -> Vec<u8> {
    let output = git_output(repository, arguments);
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn git_output(repository: &Path, arguments: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("git command")
}

fn commit(repository: &Path, message: &str, timestamp: &str) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["commit", "-q", "-m", message])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "WinWinCode Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@winwincode.invalid")
        .env("GIT_COMMITTER_NAME", "WinWinCode Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@winwincode.invalid")
        .env("GIT_AUTHOR_DATE", timestamp)
        .env("GIT_COMMITTER_DATE", timestamp)
        .output()
        .expect("git commit");
    assert!(
        output.status.success(),
        "git commit: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .expect("Git UTF-8")
        .trim()
        .to_owned()
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
