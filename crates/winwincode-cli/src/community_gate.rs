// SPDX-License-Identifier: Apache-2.0

//! Community local-only evidence closure.
//!
//! This module joins already authoritative leaf results. It does not run a
//! `Worker`, invent Delivery facts, or infer evidence from chat. The resulting
//! package is accepted only when one local Git candidate, a passing independent
//! Verdict, the current test-asset contract, and every exported digest agree.

use std::fmt;
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_delivery::domain::{
    CriterionVerdict, DELIVERY_SCHEMA_VERSION, Delivery, DeliveryStage, DeliveryStatus,
    EvidenceRef, EvidenceRefType, FrozenDeliveryCandidate, RepositoryKind, StageRunActorType,
    StageRunStatus,
};
use winwincode_evidence_export::{
    ArtifactSource, DocumentKind, EvidenceDocument, EvidenceErrorKind, EvidenceManifest,
    ExportCapacity, ExportReport, ExportRequest, TraceRecord, TraceSource, export_evidence,
    verify_evidence_package,
};
use winwincode_repository_context::{LocalCodeIndexMode, RepositoryContext};
use winwincode_test_assets::manifest::{
    TEST_ASSET_MANIFEST_SCHEMA_VERSION, TestAssetEvidenceBinding, TestAssetManifest,
    TestAssetVerdictBinding, detect_verdict_invalidation,
};
use winwincode_test_assets::{FindingDisposition, TestManipulationFinding};

use crate::Attachment;

pub const COMMUNITY_LOCAL_GATE_SCHEMA_VERSION: u8 = 1;

/// Local Community dependencies that must remain absent throughout the gate.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CommunityLocalEnvironment {
    pub commercial_account_configured: bool,
    pub vendor_telemetry_enabled: bool,
}

/// Complete explicit input for one local evidence closure.
#[derive(Debug)]
pub struct CommunityLocalGateRequest<'facts> {
    pub repository_root: &'facts Path,
    pub attachment: &'facts Attachment,
    pub repository_context: &'facts RepositoryContext,
    pub delivery: &'facts Delivery,
    pub candidate: &'facts FrozenDeliveryCandidate,
    pub test_asset_manifest: &'facts TestAssetManifest,
    pub test_evidence_bindings: &'facts [TestAssetEvidenceBinding],
    pub test_verdict_binding: &'facts TestAssetVerdictBinding,
    pub test_manipulation_findings: &'facts [TestManipulationFinding],
    pub export_root: &'facts Path,
    pub package_id: &'facts str,
    pub artifacts: Vec<ArtifactSource>,
    pub capacity: ExportCapacity,
    pub create_archive: bool,
    pub environment: CommunityLocalEnvironment,
}

/// Stable contract and source identities retained beside the exported files.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommunityLocalSourceTrace {
    pub gate_schema_version: u8,
    pub attachment_schema_version: u8,
    pub delivery_schema_version: u8,
    pub test_asset_schema_version: u8,
    pub evidence_package_schema_version: u32,
    pub repository_baseline_sha: String,
    pub repository_index_mode: String,
    pub delivery_id: String,
    pub delivery_revision: u64,
    pub delivery_spec_id: String,
    pub delivery_spec_revision: u64,
    pub candidate_ref: String,
    pub candidate_commit_id: String,
    pub candidate_tree_id: String,
    pub candidate_diff_sha256: String,
    pub test_asset_manifest_id: String,
    pub test_asset_manifest_revision: u64,
    pub test_asset_manifest_sha256: String,
    pub verdict_id: String,
    pub evidence_manifest_sha256: String,
}

/// Successful offline-verifiable Community closure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommunityLocalGateReceipt {
    pub export: ExportReport,
    pub manifest: EvidenceManifest,
    pub source_trace: CommunityLocalSourceTrace,
}

/// Product-facing failure ownership. These classes never depend on whether a
/// Git remote exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommunityGateFailureCategory {
    Implementation,
    Acceptance,
    Environment,
}

/// Stable failure code for remediation and deterministic tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommunityGateFailureCode {
    ExternalDependencyPresent,
    RepositoryUnavailable,
    SourceContractMismatch,
    CandidateSourceMismatch,
    TestPolicyViolation,
    AcceptanceReviewRequired,
    VerdictNotPassing,
    IndependentVerificationMissing,
    TestEvidenceMismatch,
    EvidenceExportFailed,
}

/// Categorized gate failure with bounded detail.
#[derive(Debug)]
pub struct CommunityLocalGateError {
    category: CommunityGateFailureCategory,
    code: CommunityGateFailureCode,
    message: String,
}

impl CommunityLocalGateError {
    fn new(
        category: CommunityGateFailureCategory,
        code: CommunityGateFailureCode,
        message: impl Into<String>,
    ) -> Self {
        Self {
            category,
            code,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn category(&self) -> CommunityGateFailureCategory {
        self.category
    }

    #[must_use]
    pub const fn code(&self) -> CommunityGateFailureCode {
        self.code
    }
}

impl fmt::Display for CommunityLocalGateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CommunityLocalGateError {}

struct ValidatedLocalSources {
    patch: Vec<u8>,
    verdict: Vec<u8>,
    merge_guide: Vec<u8>,
    manifest_id: String,
    manifest_revision: u64,
    manifest_sha256: String,
    verdict_id: String,
}

/// Runs the local-only Community gate and immediately verifies the published
/// evidence directory offline.
///
/// # Errors
///
/// Returns a stable implementation, acceptance, or environment failure. A
/// missing remote never changes a Verdict: only an existing canonical passing
/// Verdict with independent test Evidence can pass.
#[allow(clippy::too_many_lines)]
pub fn run_community_local_gate(
    request: &CommunityLocalGateRequest<'_>,
) -> Result<CommunityLocalGateReceipt, CommunityLocalGateError> {
    validate_environment(request)?;
    let repository = validate_repository_sources(request)?;
    validate_test_policy(request)?;
    let sources = validate_delivery_and_test_evidence(request, &repository)?;
    let trace_records = trace_records(request, &sources)?;
    let export_request = ExportRequest {
        package_id: request.package_id.to_owned(),
        source_commit: request.candidate.candidate_commit_id().to_owned(),
        trace_records,
        documents: vec![
            document(DocumentKind::PatchDiff, sources.patch),
            document(DocumentKind::Verdict, sources.verdict),
            document(DocumentKind::MergeGuide, sources.merge_guide),
        ],
        artifacts: request.artifacts.clone(),
        capacity: request.capacity,
        create_archive: request.create_archive,
    };
    let export = export_evidence(request.export_root, &export_request)
        .map_err(|error| export_error(&error))?;
    let manifest =
        verify_evidence_package(&export.package_path).map_err(|error| export_error(&error))?;
    if manifest.source_commit != request.candidate.candidate_commit_id()
        || manifest.schema_version == 0
        || !manifest.stable_bytes
        || !manifest.non_deterministic_fields.is_empty()
    {
        return Err(implementation_error(
            CommunityGateFailureCode::SourceContractMismatch,
            "offline Evidence manifest does not retain the exact deterministic candidate source",
        ));
    }
    let snapshot = request.delivery.snapshot();
    Ok(CommunityLocalGateReceipt {
        source_trace: CommunityLocalSourceTrace {
            gate_schema_version: COMMUNITY_LOCAL_GATE_SCHEMA_VERSION,
            attachment_schema_version: request.attachment.schema_version,
            delivery_schema_version: snapshot.schema_version,
            test_asset_schema_version: request.test_asset_manifest.schema_version,
            evidence_package_schema_version: manifest.schema_version,
            repository_baseline_sha: request.repository_context.baseline_sha.clone(),
            repository_index_mode: index_mode_name(
                request.repository_context.local_code_index.mode,
            )
            .to_owned(),
            delivery_id: snapshot.id.0.clone(),
            delivery_revision: snapshot.revision,
            delivery_spec_id: snapshot.spec.id.0.clone(),
            delivery_spec_revision: snapshot.spec.revision,
            candidate_ref: request.candidate.candidate_ref().to_owned(),
            candidate_commit_id: request.candidate.candidate_commit_id().to_owned(),
            candidate_tree_id: request.candidate.candidate_tree_id().to_owned(),
            candidate_diff_sha256: request.candidate.diff_sha256().to_owned(),
            test_asset_manifest_id: sources.manifest_id,
            test_asset_manifest_revision: sources.manifest_revision,
            test_asset_manifest_sha256: sources.manifest_sha256,
            verdict_id: sources.verdict_id,
            evidence_manifest_sha256: export.manifest_sha256.clone(),
        },
        export,
        manifest,
    })
}

fn validate_environment(
    request: &CommunityLocalGateRequest<'_>,
) -> Result<(), CommunityLocalGateError> {
    let external = request.attachment.remote_configured
        || request.environment.commercial_account_configured
        || request.environment.vendor_telemetry_enabled
        || request.delivery.snapshot().spec.source_ref.is_some()
        || request
            .delivery
            .snapshot()
            .spec
            .publication_target
            .is_some()
        || request.delivery.snapshot().spec.repository.kind != RepositoryKind::LocalGit;
    if external {
        return Err(CommunityLocalGateError::new(
            CommunityGateFailureCategory::Environment,
            CommunityGateFailureCode::ExternalDependencyPresent,
            "Community local gate requires local Git with no Remote, commercial account, vendor telemetry, or hosted publication source",
        ));
    }
    Ok(())
}

fn validate_repository_sources(
    request: &CommunityLocalGateRequest<'_>,
) -> Result<std::path::PathBuf, CommunityLocalGateError> {
    if request.attachment.schema_version != 1
        || request.delivery.snapshot().schema_version != DELIVERY_SCHEMA_VERSION
        || request.test_asset_manifest.schema_version != TEST_ASSET_MANIFEST_SCHEMA_VERSION
    {
        return Err(implementation_error(
            CommunityGateFailureCode::SourceContractMismatch,
            "one local gate source uses an unsupported contract version",
        ));
    }
    let repository = request.repository_root.canonicalize().map_err(|error| {
        CommunityLocalGateError::new(
            CommunityGateFailureCategory::Environment,
            CommunityGateFailureCode::RepositoryUnavailable,
            format!("local repository cannot be opened: {error}"),
        )
    })?;
    let attached = Path::new(&request.attachment.repository_root)
        .canonicalize()
        .map_err(|error| {
            CommunityLocalGateError::new(
                CommunityGateFailureCategory::Environment,
                CommunityGateFailureCode::RepositoryUnavailable,
                format!("attached repository cannot be opened: {error}"),
            )
        })?;
    let snapshot = request.delivery.snapshot();
    if !git_bytes(&repository, &["remote"])?.is_empty() {
        return Err(CommunityLocalGateError::new(
            CommunityGateFailureCategory::Environment,
            CommunityGateFailureCode::ExternalDependencyPresent,
            "Community local gate requires the repository to remain without a Git Remote",
        ));
    }
    let matching_baseline = repository == attached
        && request.attachment.baseline_sha == request.repository_context.baseline_sha
        && request.attachment.baseline_sha == snapshot.spec.base_revision
        && request.attachment.baseline_sha == request.candidate.base_commit_id()
        && request.repository_context.baseline_verified
        && request.repository_context.local_code_index.available
        && request.repository_context.local_code_index.fresh
        && request.repository_context.local_code_index.baseline_sha
            == request.repository_context.baseline_sha;
    let matching_candidate = request.candidate.delivery_id() == &snapshot.id
        && request.candidate.delivery_spec_id() == &snapshot.spec.id
        && request.candidate.delivery_spec_revision() == snapshot.spec.revision
        && request.candidate.repository() == &snapshot.spec.repository
        && request.candidate.base_revision() == snapshot.spec.base_revision;
    if !matching_baseline || !matching_candidate {
        return Err(implementation_error(
            CommunityGateFailureCode::SourceContractMismatch,
            "attachment, repository context, Delivery, and candidate source identities do not agree",
        ));
    }

    let base_tree = git_text(
        &repository,
        &[
            "rev-parse",
            &format!("{}^{{tree}}", request.candidate.base_commit_id()),
        ],
    )?;
    let candidate_tree = git_text(
        &repository,
        &[
            "rev-parse",
            &format!("{}^{{tree}}", request.candidate.candidate_commit_id()),
        ],
    )?;
    if base_tree != request.candidate.base_tree_id()
        || candidate_tree != request.candidate.candidate_tree_id()
    {
        return Err(implementation_error(
            CommunityGateFailureCode::CandidateSourceMismatch,
            "candidate commit or tree no longer matches the frozen local Git source",
        ));
    }
    git_success(
        &repository,
        &[
            "merge-base",
            "--is-ancestor",
            request.candidate.base_commit_id(),
            request.candidate.candidate_commit_id(),
        ],
    )?;
    Ok(repository)
}

fn validate_test_policy(
    request: &CommunityLocalGateRequest<'_>,
) -> Result<(), CommunityLocalGateError> {
    if request
        .test_manipulation_findings
        .iter()
        .any(|finding| finding.disposition == FindingDisposition::Block)
    {
        return Err(implementation_error(
            CommunityGateFailureCode::TestPolicyViolation,
            "candidate contains a deterministic blocking test-manipulation finding",
        ));
    }
    if request
        .test_manipulation_findings
        .iter()
        .any(|finding| finding.disposition == FindingDisposition::Review)
    {
        return Err(CommunityLocalGateError::new(
            CommunityGateFailureCategory::Acceptance,
            CommunityGateFailureCode::AcceptanceReviewRequired,
            "candidate contains a test change that requires independent acceptance review",
        ));
    }
    Ok(())
}

fn validate_delivery_and_test_evidence(
    request: &CommunityLocalGateRequest<'_>,
    repository: &Path,
) -> Result<ValidatedLocalSources, CommunityLocalGateError> {
    let snapshot = request.delivery.snapshot();
    let verdict = snapshot.verdict.as_ref().ok_or_else(|| {
        CommunityLocalGateError::new(
            CommunityGateFailureCategory::Acceptance,
            CommunityGateFailureCode::VerdictNotPassing,
            "Delivery has no canonical Verdict",
        )
    })?;
    if !matches!(
        snapshot.status,
        DeliveryStatus::ReadyToDeliver | DeliveryStatus::Delivered
    ) || verdict.status != CriterionVerdict::Pass
        || verdict.candidate_ref != request.candidate.candidate_ref()
        || !verdict.unresolved_findings.is_empty()
    {
        return Err(CommunityLocalGateError::new(
            CommunityGateFailureCategory::Acceptance,
            CommunityGateFailureCode::VerdictNotPassing,
            "Community local gate requires the unchanged canonical passing Verdict",
        ));
    }
    request.test_asset_manifest.validate().map_err(|error| {
        implementation_error(
            CommunityGateFailureCode::TestEvidenceMismatch,
            format!("TestAsset manifest is invalid: {error}"),
        )
    })?;
    if request.test_asset_manifest.candidate_ref != request.candidate.candidate_ref()
        || request.test_asset_manifest.source_commit != request.candidate.candidate_commit_id()
    {
        return Err(implementation_error(
            CommunityGateFailureCode::TestEvidenceMismatch,
            "TestAsset manifest names another candidate source",
        ));
    }
    let rebuilt = TestAssetVerdictBinding::new(
        verdict,
        request.test_asset_manifest,
        request.test_evidence_bindings,
    )
    .map_err(|error| {
        implementation_error(
            CommunityGateFailureCode::TestEvidenceMismatch,
            format!("TestAsset Evidence cannot bind to the Verdict: {error}"),
        )
    })?;
    if &rebuilt != request.test_verdict_binding
        || detect_verdict_invalidation(
            request.test_verdict_binding,
            request.candidate.candidate_ref(),
            request.test_asset_manifest,
            verdict.produced_at_millis,
        )
        .map_err(|error| {
            implementation_error(
                CommunityGateFailureCode::TestEvidenceMismatch,
                format!("TestAsset Verdict identity is invalid: {error}"),
            )
        })?
        .is_some()
    {
        return Err(implementation_error(
            CommunityGateFailureCode::TestEvidenceMismatch,
            "current TestAsset manifest or candidate invalidates the Verdict binding",
        ));
    }
    validate_independent_test_evidence(request, verdict)?;

    let manifest_ref = request
        .test_asset_manifest
        .artifact_ref()
        .map_err(|error| {
            implementation_error(
                CommunityGateFailureCode::TestEvidenceMismatch,
                format!("TestAsset manifest identity cannot be derived: {error}"),
            )
        })?;
    build_local_documents(request, repository, verdict, manifest_ref)
}

fn build_local_documents(
    request: &CommunityLocalGateRequest<'_>,
    repository: &Path,
    verdict: &winwincode_delivery::domain::DeliveryVerdict,
    manifest_ref: winwincode_test_assets::manifest::TestAssetManifestRef,
) -> Result<ValidatedLocalSources, CommunityLocalGateError> {
    let revision_range = format!(
        "{}..{}",
        request.candidate.base_commit_id(),
        request.candidate.candidate_commit_id()
    );
    let patch = git_bytes(
        repository,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            "--full-index",
            &revision_range,
        ],
    )?;
    if patch.is_empty() || digest(&patch) != request.candidate.diff_sha256() {
        return Err(implementation_error(
            CommunityGateFailureCode::CandidateSourceMismatch,
            "local patch bytes do not match the frozen candidate diff",
        ));
    }
    let mut verdict_bytes = serde_json::to_vec_pretty(verdict).map_err(|error| {
        implementation_error(
            CommunityGateFailureCode::SourceContractMismatch,
            format!("canonical Verdict cannot be encoded: {error}"),
        )
    })?;
    verdict_bytes.push(b'\n');
    Ok(ValidatedLocalSources {
        patch,
        verdict: verdict_bytes,
        merge_guide: merge_guide(request).into_bytes(),
        manifest_id: manifest_ref.id,
        manifest_revision: manifest_ref.revision,
        manifest_sha256: manifest_ref.digest_sha256,
        verdict_id: verdict.id.0.clone(),
    })
}

fn validate_independent_test_evidence(
    request: &CommunityLocalGateRequest<'_>,
    verdict: &winwincode_delivery::domain::DeliveryVerdict,
) -> Result<(), CommunityLocalGateError> {
    let snapshot = request.delivery.snapshot();
    let cited = verdict
        .criteria
        .iter()
        .flat_map(|criterion| criterion.evidence_refs.iter())
        .collect::<Vec<_>>();
    let mut has_verifier = false;
    let mut has_blocking = false;
    for test_binding in request.test_evidence_bindings {
        let evidence = snapshot
            .evidence
            .iter()
            .find(|evidence| evidence.id == *test_binding.evidence_ref_id())
            .filter(|evidence| cited.contains(&&evidence.id))
            .ok_or_else(|| independent_error("test Evidence is absent from the current Verdict"))?;
        if evidence.evidence_type != EvidenceRefType::Test
            || evidence.candidate_ref != request.candidate.candidate_ref()
        {
            return Err(independent_error(
                "test Evidence belongs to another type or candidate",
            ));
        }
        let (stage, binding) = evidence_authority(snapshot, evidence)
            .ok_or_else(|| independent_error("test Evidence authority is incomplete"))?;
        let independent = stage.stage == DeliveryStage::Verifying
            && stage.actor_type == StageRunActorType::Codex
            && stage.status == StageRunStatus::Succeeded
            && matches!(stage.role.as_str(), "reviewer" | "verifier")
            && stage.id != *request.candidate.producer_stage_run_id()
            && binding.execution_job_id != *request.candidate.producer_execution_job_id()
            && binding.worker_session_id.as_ref()
                != Some(request.candidate.producer_worker_session_id())
            && binding.codex_thread_id.as_ref()
                != Some(request.candidate.producer_codex_thread_id());
        if !independent {
            return Err(independent_error(
                "test Evidence reuses or fails to prove an independent verification identity",
            ));
        }
        has_verifier |= stage.role == "verifier";
        has_blocking |= test_binding.blocks_delivery();
    }
    if !has_verifier || !has_blocking {
        return Err(independent_error(
            "passing Verdict requires independent verifier Evidence from a blocking canonical TestAsset",
        ));
    }
    Ok(())
}

fn evidence_authority<'snapshot>(
    snapshot: &'snapshot winwincode_delivery::domain::DeliverySnapshot,
    evidence: &EvidenceRef,
) -> Option<(
    &'snapshot winwincode_delivery::domain::StageRun,
    &'snapshot winwincode_delivery::domain::SessionBinding,
)> {
    let stage = snapshot
        .stage_runs
        .iter()
        .find(|stage| stage.id == evidence.stage_run_id)?;
    let binding = snapshot
        .session_bindings
        .iter()
        .find(|binding| binding.id == evidence.session_binding_id)?;
    (binding.stage_run_id == stage.id).then_some((stage, binding))
}

fn trace_records(
    request: &CommunityLocalGateRequest<'_>,
    sources: &ValidatedLocalSources,
) -> Result<Vec<TraceRecord>, CommunityLocalGateError> {
    let snapshot = request.delivery.snapshot();
    let verdict = snapshot
        .verdict
        .as_ref()
        .expect("verdict was validated before trace construction");
    let delivery_bytes = request.delivery.encode_json().map_err(|error| {
        implementation_error(
            CommunityGateFailureCode::SourceContractMismatch,
            format!("Delivery cannot be encoded for trace binding: {error}"),
        )
    })?;
    let binding_bytes = serde_json::to_vec(request.test_verdict_binding).map_err(|error| {
        implementation_error(
            CommunityGateFailureCode::SourceContractMismatch,
            format!("TestAsset binding cannot be encoded for trace binding: {error}"),
        )
    })?;
    Ok(vec![
        TraceRecord {
            source: TraceSource::Delivery,
            occurred_at_millis: snapshot.updated_at_millis,
            sequence: snapshot.revision,
            record_id: snapshot.id.0.clone(),
            scope_id: snapshot.id.0.clone(),
            kind: "delivery.verdict.pass".into(),
            content_digest: digest(&delivery_bytes),
        },
        TraceRecord {
            source: TraceSource::WorkerRuntime,
            occurred_at_millis: request.candidate.producer_finished_at_millis(),
            sequence: request.candidate.producer_last_event_sequence(),
            record_id: request.candidate.producer_artifact_ref().to_owned(),
            scope_id: snapshot.id.0.clone(),
            kind: "execution.completed".into(),
            content_digest: request.candidate.diff_sha256().to_owned(),
        },
        TraceRecord {
            source: TraceSource::Artifact,
            occurred_at_millis: verdict.produced_at_millis,
            sequence: sources.manifest_revision,
            record_id: sources.manifest_id.clone(),
            scope_id: snapshot.id.0.clone(),
            kind: "test_asset.manifest".into(),
            content_digest: sources.manifest_sha256.clone(),
        },
        TraceRecord {
            source: TraceSource::Audit,
            occurred_at_millis: verdict.produced_at_millis,
            sequence: snapshot.revision,
            record_id: verdict.id.0.clone(),
            scope_id: snapshot.id.0.clone(),
            kind: "community.local_gate".into(),
            content_digest: digest(&binding_bytes),
        },
    ])
}

fn merge_guide(request: &CommunityLocalGateRequest<'_>) -> String {
    let candidate = request.candidate;
    let mut guide = String::new();
    writeln!(guide, "# Local merge guide").expect("String write");
    writeln!(guide).expect("String write");
    writeln!(guide, "Base commit: `{}`", candidate.base_commit_id()).expect("String write");
    writeln!(
        guide,
        "Candidate commit: `{}`",
        candidate.candidate_commit_id()
    )
    .expect("String write");
    writeln!(guide, "Candidate tree: `{}`", candidate.candidate_tree_id()).expect("String write");
    writeln!(guide, "Patch SHA-256: `{}`", candidate.diff_sha256()).expect("String write");
    writeln!(guide).expect("String write");
    writeln!(
        guide,
        "Verify ancestry: `git merge-base --is-ancestor {} {}`",
        candidate.base_commit_id(),
        candidate.candidate_commit_id()
    )
    .expect("String write");
    writeln!(
        guide,
        "Merge locally: `git merge --no-ff {}`",
        candidate.candidate_commit_id()
    )
    .expect("String write");
    writeln!(
        guide,
        "Cherry-pick locally: `git cherry-pick {}`",
        candidate.candidate_commit_id()
    )
    .expect("String write");
    guide
}

fn document(kind: DocumentKind, bytes: Vec<u8>) -> EvidenceDocument {
    EvidenceDocument {
        kind,
        expected_sha256: digest(&bytes),
        bytes,
    }
}

fn git_text(repository: &Path, arguments: &[&str]) -> Result<String, CommunityLocalGateError> {
    String::from_utf8(git_bytes(repository, arguments)?)
        .map(|value| value.trim().to_owned())
        .map_err(|_| {
            implementation_error(
                CommunityGateFailureCode::CandidateSourceMismatch,
                "local Git returned non-UTF-8 source identity",
            )
        })
}

fn git_success(repository: &Path, arguments: &[&str]) -> Result<(), CommunityLocalGateError> {
    git_command(repository, arguments).map(|_| ())
}

fn git_bytes(repository: &Path, arguments: &[&str]) -> Result<Vec<u8>, CommunityLocalGateError> {
    git_command(repository, arguments).map(|output| output.stdout)
}

fn git_command(
    repository: &Path,
    arguments: &[&str],
) -> Result<std::process::Output, CommunityLocalGateError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .map_err(|error| {
            CommunityLocalGateError::new(
                CommunityGateFailureCategory::Environment,
                CommunityGateFailureCode::RepositoryUnavailable,
                format!("local Git command cannot start: {error}"),
            )
        })?;
    if !output.status.success() {
        return Err(implementation_error(
            CommunityGateFailureCode::CandidateSourceMismatch,
            format!(
                "local Git source check failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(output)
}

fn export_error(error: &winwincode_evidence_export::EvidenceError) -> CommunityLocalGateError {
    let category = match error.kind() {
        EvidenceErrorKind::InsufficientDisk
        | EvidenceErrorKind::Io
        | EvidenceErrorKind::Conflict => CommunityGateFailureCategory::Environment,
        EvidenceErrorKind::InvalidInput
        | EvidenceErrorKind::DigestMismatch
        | EvidenceErrorKind::SecretDetected
        | EvidenceErrorKind::Corrupt => CommunityGateFailureCategory::Implementation,
    };
    CommunityLocalGateError::new(
        category,
        CommunityGateFailureCode::EvidenceExportFailed,
        format!("Evidence export failed: {error}"),
    )
}

fn implementation_error(
    code: CommunityGateFailureCode,
    message: impl Into<String>,
) -> CommunityLocalGateError {
    CommunityLocalGateError::new(CommunityGateFailureCategory::Implementation, code, message)
}

const fn index_mode_name(mode: LocalCodeIndexMode) -> &'static str {
    match mode {
        LocalCodeIndexMode::AstGrepOutline => "ast-grep-outline",
        LocalCodeIndexMode::GitFileInventory => "git-file-inventory",
    }
}

fn independent_error(message: impl Into<String>) -> CommunityLocalGateError {
    CommunityLocalGateError::new(
        CommunityGateFailureCategory::Acceptance,
        CommunityGateFailureCode::IndependentVerificationMissing,
        message,
    )
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
