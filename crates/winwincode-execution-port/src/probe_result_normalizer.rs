// SPDX-License-Identifier: Apache-2.0

//! Pure, deterministic normalization and durable validation of `DebugProbe`
//! process evidence.
//!
//! Generated types own the wire shape. This module owns host-selected profile
//! sealing, raw Artifact bindings, L0/L1 cross-field semantics, and canonical
//! content identities. Raw process bytes are accepted only as borrowed input
//! and are never retained in the normalized bundle or its summary projection.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::path::{Component, Path};

use serde::Serialize;
use sha2::{Digest as _, Sha256};
use winwincode_domain::{DebugHypothesisId, Sha256Digest, WorkspaceRevision};

use crate::debug_probe_contract::{
    ValidatedProbeExecutionIntent, validate_probe_execution_receipt,
};
use crate::diagnostic_parser::{
    DiagnosticInputCompleteness, DiagnosticParseBatch, DiagnosticParseErrorCode,
    build_diagnostic_baseline, compare_diagnostic_baselines, diagnostic_input,
    parse_diagnostic_occurrences, validate_normalized_diagnostic,
};
use crate::generated::{
    ArtifactReference, DebugHypothesisEvidence, DebugProbeErrorCode, DebugProbeIdentity,
    DiagnosticBaselineComparison, DiagnosticCategory, DiagnosticChangeStatus,
    DiagnosticParserVersion, HypothesisEvidenceCandidate, NormalizedDiagnostic,
    ProbeBaselineEvidence, ProbeBaselineSelection, ProbeBaselineSelectionState, ProbeBaselineState,
    ProbeDiagnosticOccurrence, ProbeEvidenceBundle, ProbeEvidenceCompleteness,
    ProbeEvidenceCompletenessStatus, ProbeEvidenceIncompleteReason, ProbeEvidenceSummary,
    ProbeExecutionReceipt, ProbeFailedTest, ProbeNormalizerProfile, ProbeNormalizerVersion,
    ProbeRawStream, ProbeRawStreamBinding, ProbeRawStreamEncoding, ProbeStackCluster,
    ProbeStackFrame, ProbeStackParserVersion, ProbeWorkspaceDeltaEvidence,
    ProbeWorkspaceDeltaState,
};

const PROFILE_DIGEST_DOMAIN: &[u8] = b"winwincode.debug-probe.normalizer-profile.v1\0";
const STACK_DIGEST_DOMAIN: &[u8] = b"winwincode.debug-probe.stack.v1\0";
const BUNDLE_DIGEST_DOMAIN: &[u8] = b"winwincode.debug-probe.evidence-bundle.v1\0";
const HYPOTHESIS_EVIDENCE_DIGEST_DOMAIN: &[u8] = b"winwincode.debug-probe.hypothesis-evidence.v1\0";
const MAX_RAW_STREAM_BYTES: usize = 16_777_216;
const MAX_STACK_FRAMES: usize = 64;
const MAX_STACK_CLUSTERS: usize = 256;
const MAX_OCCURRENCES: i64 = 1_000_000;

/// Stable, secret-safe evidence normalization failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProbeEvidenceError {
    code: DebugProbeErrorCode,
    message: &'static str,
}

impl ProbeEvidenceError {
    /// Returns the canonical machine-readable error category.
    #[must_use]
    pub const fn code(&self) -> &DebugProbeErrorCode {
        &self.code
    }

    /// Returns a bounded message that never echoes process output.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        self.message
    }
}

impl fmt::Display for ProbeEvidenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ProbeEvidenceError {}

/// A host-selected normalizer profile whose content digest has been verified.
#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedProbeNormalizerProfile {
    profile: ProbeNormalizerProfile,
}

/// Host-owned baseline selection admitted before normalization.
///
/// Callers cannot construct an arbitrary baseline comparison. A comparable
/// baseline is derived only from a previously sealed bundle and its exact
/// canonical Artifact reference.
#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedProbeBaselineInput {
    kind: ProbeBaselineInputKind,
    selection: ProbeBaselineSelection,
}

impl ValidatedProbeBaselineInput {
    /// Returns the exact host-owned choice persisted before process execution.
    #[must_use]
    pub const fn selection(&self) -> &ProbeBaselineSelection {
        &self.selection
    }
}

#[derive(Clone, Debug, PartialEq)]
enum ProbeBaselineInputKind {
    NotApplicable,
    Unavailable,
    Comparable(ComparableProbeBaseline),
    Incompatible(BaselineAuthorityBinding),
}

#[derive(Clone, Debug, PartialEq)]
struct ComparableProbeBaseline {
    authority: BaselineAuthorityBinding,
    revision: WorkspaceRevision,
    diagnostics: Vec<NormalizedDiagnostic>,
}

#[derive(Clone, Debug, PartialEq)]
struct BaselineAuthorityBinding {
    environment_digest: Sha256Digest,
    probe_definition_digest: Sha256Digest,
    profile_digest: Sha256Digest,
    bundle_artifact_ref: ArtifactReference,
    bundle_digest: Sha256Digest,
}

impl ValidatedProbeNormalizerProfile {
    /// Returns the exact generated profile retained by the seal.
    #[must_use]
    pub const fn profile(&self) -> &ProbeNormalizerProfile {
        &self.profile
    }

    /// Consumes the seal and returns the generated profile.
    #[must_use]
    pub fn into_profile(self) -> ProbeNormalizerProfile {
        self.profile
    }
}

/// A durable evidence bundle whose authority, derived fields, and digest have
/// all been recomputed.
#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedProbeEvidenceBundle {
    bundle: ProbeEvidenceBundle,
}

/// A previously stored bundle rebound to the exact semantic digest and
/// canonical-byte Artifact reference retained by the evidence journal.
#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedPriorProbeEvidenceBundle {
    bundle: ProbeEvidenceBundle,
    artifact_ref: ArtifactReference,
}

impl ValidatedPriorProbeEvidenceBundle {
    /// Returns the revalidated prior bundle.
    #[must_use]
    pub const fn bundle(&self) -> &ProbeEvidenceBundle {
        &self.bundle
    }

    /// Returns the journal-bound canonical bundle Artifact reference.
    #[must_use]
    pub const fn artifact_ref(&self) -> &ArtifactReference {
        &self.artifact_ref
    }
}

impl ValidatedProbeEvidenceBundle {
    /// Returns the exact generated bundle retained by the seal.
    #[must_use]
    pub const fn bundle(&self) -> &ProbeEvidenceBundle {
        &self.bundle
    }

    /// Consumes the seal and returns the generated bundle.
    #[must_use]
    pub fn into_bundle(self) -> ProbeEvidenceBundle {
        self.bundle
    }
}

/// A bounded summary and unpolarized hypothesis candidates derived from one
/// canonical evidence bundle.
#[derive(Clone, Debug, PartialEq)]
pub struct ProbeEvidenceProjection {
    summary: ProbeEvidenceSummary,
    candidates: Vec<HypothesisEvidenceCandidate>,
}

impl ProbeEvidenceProjection {
    /// Returns the bounded summary that may be surfaced at a round boundary.
    #[must_use]
    pub const fn summary(&self) -> &ProbeEvidenceSummary {
        &self.summary
    }

    /// Returns target-bound evidence candidates without polarity or confidence.
    #[must_use]
    pub fn candidates(&self) -> &[HypothesisEvidenceCandidate] {
        &self.candidates
    }

    /// Consumes the projection into its generated wire values.
    #[must_use]
    pub fn into_parts(self) -> (ProbeEvidenceSummary, Vec<HypothesisEvidenceCandidate>) {
        (self.summary, self.candidates)
    }
}

/// One borrowed raw process stream together with its content-addressed Artifact.
#[derive(Clone, Debug)]
pub struct ProbeRawStreamInput<'a> {
    stream: ProbeRawStream,
    artifact_ref: ArtifactReference,
    bytes: &'a [u8],
}

impl<'a> ProbeRawStreamInput<'a> {
    /// Creates a raw input. Its Artifact digest is checked during normalization.
    #[must_use]
    pub const fn new(
        stream: ProbeRawStream,
        artifact_ref: ArtifactReference,
        bytes: &'a [u8],
    ) -> Self {
        Self {
            stream,
            artifact_ref,
            bytes,
        }
    }
}

/// Derives the canonical host-selected profile digest.
///
/// The digest excludes `profileDigest` and uses domain-separated length frames.
///
/// # Errors
///
/// Rejects a profile that cannot be represented canonically.
pub fn derive_probe_normalizer_profile_digest(
    profile: &ProbeNormalizerProfile,
) -> Result<Sha256Digest, ProbeEvidenceError> {
    let mut digest = FramedDigest::new(PROFILE_DIGEST_DOMAIN);
    digest.text(normalizer_version_tag(&profile.normalizer_version))?;
    digest.optional_text(
        profile
            .diagnostic_parser_version
            .as_ref()
            .map(diagnostic_parser_version_tag),
    )?;
    digest.optional_text(
        profile
            .stack_parser_version
            .as_ref()
            .map(stack_parser_version_tag),
    )?;
    Ok(digest.finish())
}

/// Seals one exact host-selected normalizer profile.
///
/// # Errors
///
/// Rejects a profile whose stored digest differs from its canonical fields.
pub fn seal_probe_normalizer_profile(
    profile: ProbeNormalizerProfile,
) -> Result<ValidatedProbeNormalizerProfile, ProbeEvidenceError> {
    if !valid_sha256_digest(&profile.profile_digest)
        || derive_probe_normalizer_profile_digest(&profile)? != profile.profile_digest
    {
        return Err(invalid_probe("probe normalizer profile digest is invalid"));
    }
    Ok(ValidatedProbeNormalizerProfile { profile })
}

/// Selects the explicit no-baseline state for a profile without a diagnostic
/// parser.
#[must_use]
pub const fn probe_baseline_not_applicable() -> ValidatedProbeBaselineInput {
    ValidatedProbeBaselineInput {
        kind: ProbeBaselineInputKind::NotApplicable,
        selection: empty_baseline_selection(ProbeBaselineSelectionState::NotApplicable),
    }
}

/// Selects the explicit host-observed missing-baseline state.
#[must_use]
pub const fn probe_baseline_unavailable() -> ValidatedProbeBaselineInput {
    ValidatedProbeBaselineInput {
        kind: ProbeBaselineInputKind::Unavailable,
        selection: empty_baseline_selection(ProbeBaselineSelectionState::Unavailable),
    }
}

/// Selects a prior sealed bundle as the only source of comparable baseline
/// evidence.
///
/// Matching environment, probe definition, and profile digests produce a
/// comparable input. A mismatch is retained explicitly as incompatible. The
/// prior bundle must be complete because an incomplete result is not a sound
/// comparison base.
///
/// # Errors
///
/// Rejects an invalid or content-mismatched prior bundle Artifact reference,
/// a prior bundle without a diagnostic parser, or incomplete prior evidence.
pub fn select_probe_evidence_baseline(
    prior: &ValidatedPriorProbeEvidenceBundle,
    current_intent: &ValidatedProbeExecutionIntent,
    current_profile: &ValidatedProbeNormalizerProfile,
) -> Result<ValidatedProbeBaselineInput, ProbeEvidenceError> {
    let prior_bundle = prior.bundle();
    if prior_bundle.profile.diagnostic_parser_version.is_none()
        || prior_bundle.completeness.reasons.iter().any(|reason| {
            matches!(
                reason,
                ProbeEvidenceIncompleteReason::OutputTruncated
                    | ProbeEvidenceIncompleteReason::InvalidUtf8
                    | ProbeEvidenceIncompleteReason::DiagnosticInputTooLarge
                    | ProbeEvidenceIncompleteReason::InvalidPayload
                    | ProbeEvidenceIncompleteReason::TooManyDiagnostics
                    | ProbeEvidenceIncompleteReason::InvalidPath
            )
        })
    {
        return Err(invalid_probe(
            "prior probe evidence diagnostics are not a complete baseline",
        ));
    }
    let authority = BaselineAuthorityBinding {
        environment_digest: prior_bundle.identity.environment_digest.clone(),
        probe_definition_digest: prior_bundle.probe_definition_digest.clone(),
        profile_digest: prior_bundle.profile.profile_digest.clone(),
        bundle_artifact_ref: prior.artifact_ref.clone(),
        bundle_digest: prior_bundle.bundle_digest.clone(),
    };
    let compatible = authority.environment_digest
        == current_intent.intent().identity.environment_digest
        && authority.probe_definition_digest
            == current_intent.intent().spec.probe_definition_digest
        && authority.profile_digest == current_profile.profile.profile_digest;
    let selected = ValidatedProbeBaselineInput {
        selection: ProbeBaselineSelection {
            prior_bundle_artifact_ref: Some(prior.artifact_ref.clone()),
            prior_bundle_digest: Some(prior_bundle.bundle_digest.clone()),
            prior_environment_digest: Some(prior_bundle.identity.environment_digest.clone()),
            prior_probe_definition_digest: Some(prior_bundle.probe_definition_digest.clone()),
            prior_profile_digest: Some(prior_bundle.profile.profile_digest.clone()),
            state: ProbeBaselineSelectionState::PriorBundle,
        },
        kind: if compatible {
            ProbeBaselineInputKind::Comparable(ComparableProbeBaseline {
                authority,
                revision: prior_bundle.identity.workspace_revision.clone(),
                diagnostics: prior_bundle
                    .diagnostics
                    .iter()
                    .map(|occurrence| occurrence.diagnostic.clone())
                    .collect(),
            })
        } else {
            ProbeBaselineInputKind::Incompatible(authority)
        },
    };
    validate_baseline_selection(&selected, current_profile)?;
    Ok(selected)
}

/// Re-seals an exact persisted baseline selection against its retained prior
/// bundle and current host profile.
///
/// # Errors
///
/// Rejects nullable-field drift, a missing or substituted prior bundle, or a
/// selection that is inconsistent with the current diagnostic profile.
pub fn seal_probe_baseline_selection(
    selection: &ProbeBaselineSelection,
    prior: Option<&ValidatedPriorProbeEvidenceBundle>,
    current_intent: &ValidatedProbeExecutionIntent,
    current_profile: &ValidatedProbeNormalizerProfile,
) -> Result<ValidatedProbeBaselineInput, ProbeEvidenceError> {
    let expected = match selection.state {
        ProbeBaselineSelectionState::NotApplicable => {
            if selection != &empty_baseline_selection(ProbeBaselineSelectionState::NotApplicable)
                || prior.is_some()
            {
                return Err(invalid_probe(
                    "not-applicable baseline selection is invalid",
                ));
            }
            probe_baseline_not_applicable()
        }
        ProbeBaselineSelectionState::Unavailable => {
            if selection != &empty_baseline_selection(ProbeBaselineSelectionState::Unavailable)
                || prior.is_some()
            {
                return Err(invalid_probe("unavailable baseline selection is invalid"));
            }
            probe_baseline_unavailable()
        }
        ProbeBaselineSelectionState::PriorBundle => {
            let Some(prior) = prior else {
                return Err(invalid_probe("prior baseline bundle is missing"));
            };
            select_probe_evidence_baseline(prior, current_intent, current_profile)?
        }
    };
    if &expected.selection != selection {
        return Err(stale_authority("persisted baseline selection is stale"));
    }
    validate_baseline_selection(&expected, current_profile)?;
    Ok(expected)
}

const fn empty_baseline_selection(state: ProbeBaselineSelectionState) -> ProbeBaselineSelection {
    ProbeBaselineSelection {
        prior_bundle_artifact_ref: None,
        prior_bundle_digest: None,
        prior_environment_digest: None,
        prior_probe_definition_digest: None,
        prior_profile_digest: None,
        state,
    }
}

/// Normalizes borrowed raw output into one durable L0/L1 evidence bundle.
///
/// The selected parsers come only from the already-sealed host profile. No
/// parser is inferred from command text, probe kind, or process output.
///
/// # Errors
///
/// Rejects stale authority, receipt/raw binding drift, malformed selected
/// parser output, invalid baseline evidence, or any non-canonical derived fact.
pub fn normalize_probe_evidence(
    intent: &ValidatedProbeExecutionIntent,
    receipt: &ProbeExecutionReceipt,
    profile: &ValidatedProbeNormalizerProfile,
    raw_streams: &[ProbeRawStreamInput<'_>],
    baseline_selection: &ValidatedProbeBaselineInput,
    workspace_root: &Path,
) -> Result<ValidatedProbeEvidenceBundle, ProbeEvidenceError> {
    validate_probe_execution_receipt(receipt, intent)
        .map_err(|_| stale_authority("probe receipt does not match its sealed intent"))?;
    let NormalizedRawStreams {
        bindings,
        stdout,
        stderr,
        mut reasons,
    } = normalize_raw_streams(receipt, raw_streams)?;

    if receipt.output_truncated {
        reasons.insert(IncompleteReasonKey::OutputTruncated);
    }

    validate_baseline_selection(baseline_selection, profile)?;
    let mut diagnostics = Vec::new();
    let mut failed_tests = Vec::new();
    let mut diagnostic_parse_complete = true;
    if let Some(parser_version) = profile.profile.diagnostic_parser_version.clone() {
        let input = diagnostic_input(&parser_version, stdout, stderr);
        let completeness = if receipt.output_truncated {
            DiagnosticInputCompleteness::Truncated
        } else {
            DiagnosticInputCompleteness::Complete
        };
        match parse_diagnostic_occurrences(parser_version, input, workspace_root, completeness) {
            Ok(batch) => {
                for occurrence in batch.occurrences {
                    let frequency = i64::try_from(occurrence.frequency)
                        .map_err(|_| invalid_probe("diagnostic occurrence count overflowed"))?;
                    if frequency > MAX_OCCURRENCES {
                        return Err(invalid_probe("diagnostic occurrence count is invalid"));
                    }
                    if occurrence.diagnostic.category == DiagnosticCategory::TestFailure {
                        if failed_tests.len() == 512 {
                            return Err(invalid_probe("failed test evidence exceeds its limit"));
                        }
                        failed_tests.push(ProbeFailedTest {
                            diagnostic: occurrence.diagnostic.clone(),
                            occurrence_count: frequency,
                        });
                    }
                    diagnostics.push(ProbeDiagnosticOccurrence {
                        diagnostic: occurrence.diagnostic,
                        occurrence_count: frequency,
                    });
                }
            }
            Err(error) => {
                diagnostic_parse_complete = false;
                reasons.insert(incomplete_reason_for_diagnostic_error(error.code()));
            }
        }
    }

    let stack_clusters = if let Some(version) = profile.profile.stack_parser_version.as_ref() {
        normalize_stack_clusters(version, raw_streams, workspace_root, &mut reasons)?
    } else {
        Vec::new()
    };

    let normalized_diagnostics = diagnostics
        .iter()
        .map(|occurrence| occurrence.diagnostic.clone())
        .collect::<Vec<_>>();
    let baseline = build_probe_baseline_evidence(
        baseline_selection,
        profile,
        &receipt.identity,
        &intent.intent().spec.probe_definition_digest,
        &normalized_diagnostics,
        diagnostic_parse_complete,
    )?;
    add_baseline_reason(&baseline, &mut reasons);
    let completeness = completeness_from_keys(&reasons);
    let mut target_hypothesis_ids = intent.intent().spec.target_hypothesis_ids.clone();
    target_hypothesis_ids.sort_by(|left, right| left.0.cmp(&right.0));

    let placeholder_digest = Sha256Digest(format!("sha256:{}", "0".repeat(64)));
    let mut bundle = ProbeEvidenceBundle {
        baseline,
        bundle_digest: placeholder_digest,
        completeness,
        diagnostics,
        failed_tests,
        identity: receipt.identity.clone(),
        l0_receipt: receipt.clone(),
        normalized_at: receipt.finished_at.clone(),
        plan_digest: receipt.plan_digest.clone(),
        probe_definition_digest: intent.intent().spec.probe_definition_digest.clone(),
        profile: profile.profile.clone(),
        raw_streams: bindings,
        schema_version: 1,
        stack_clusters,
        target_hypothesis_ids,
        workspace_delta: ProbeWorkspaceDeltaEvidence {
            changed_files: Vec::new(),
            state: ProbeWorkspaceDeltaState::NotApplicable,
        },
    };
    bundle.bundle_digest = derive_probe_evidence_bundle_digest(&bundle)?;
    let sealed = seal_probe_evidence_bundle(bundle, intent)?;
    validate_probe_evidence_bundle_inputs(&sealed, intent, profile, baseline_selection)?;
    Ok(sealed)
}

/// Derives the semantic digest of a bundle, excluding `bundleDigest` itself.
///
/// Every component is serialized independently into a length frame under a
/// domain tag, avoiding delimiter ambiguity and map-order dependence.
///
/// # Errors
///
/// Returns an error if a generated component cannot be serialized.
pub fn derive_probe_evidence_bundle_digest(
    bundle: &ProbeEvidenceBundle,
) -> Result<Sha256Digest, ProbeEvidenceError> {
    let mut digest = FramedDigest::new(BUNDLE_DIGEST_DOMAIN);
    digest.i64(bundle.schema_version);
    digest.json(&bundle.identity)?;
    digest.text(&bundle.plan_digest.0)?;
    digest.text(&bundle.probe_definition_digest.0)?;
    digest.json(&bundle.profile)?;
    digest.json(&bundle.l0_receipt)?;
    digest.json(&bundle.raw_streams)?;
    digest.json(&bundle.completeness)?;
    digest.json(&bundle.diagnostics)?;
    digest.json(&bundle.failed_tests)?;
    digest.json(&bundle.stack_clusters)?;
    digest.json(&bundle.workspace_delta)?;
    digest.json(&bundle.baseline)?;
    digest.json(&bundle.target_hypothesis_ids)?;
    digest.text(&bundle.normalized_at.0)?;
    Ok(digest.finish())
}

/// Revalidates every durable cross-field and derived-field invariant.
///
/// This operation requires only the stored bundle and the already-sealed
/// intent; it does not read the workspace or rerun a parser.
///
/// # Errors
///
/// Rejects any authority, ordering, count, status, reference, or digest drift.
pub fn validate_probe_evidence_bundle(
    bundle: &ProbeEvidenceBundle,
    intent: &ValidatedProbeExecutionIntent,
) -> Result<(), ProbeEvidenceError> {
    validate_probe_execution_receipt(&bundle.l0_receipt, intent)
        .map_err(|_| stale_authority("probe evidence receipt authority is stale"))?;
    validate_self_contained_bundle(bundle)?;
    if bundle.identity != intent.intent().identity
        || bundle.plan_digest != intent.intent().plan_digest
        || bundle.probe_definition_digest != intent.intent().spec.probe_definition_digest
    {
        return Err(stale_authority("probe evidence authority is stale"));
    }
    validate_target_hypotheses(&bundle.target_hypothesis_ids, intent)
}

/// Revalidates that a sealed bundle was produced from the exact profile and
/// baseline selection persisted with the execution record.
///
/// # Errors
///
/// Rejects profile substitution, baseline-choice substitution, or prior
/// authority drift even when the substituted bundle is otherwise valid.
pub fn validate_probe_evidence_bundle_inputs(
    bundle: &ValidatedProbeEvidenceBundle,
    intent: &ValidatedProbeExecutionIntent,
    profile: &ValidatedProbeNormalizerProfile,
    baseline: &ValidatedProbeBaselineInput,
) -> Result<(), ProbeEvidenceError> {
    validate_probe_evidence_bundle(bundle.bundle(), intent)?;
    if bundle.bundle.profile != profile.profile {
        return Err(stale_authority("probe normalizer profile was substituted"));
    }
    let actual = &bundle.bundle.baseline;
    match &baseline.kind {
        ProbeBaselineInputKind::NotApplicable => {
            if actual.state != ProbeBaselineState::NotApplicable
                || actual.comparison.is_some()
                || !all_baseline_bindings_absent(actual)
            {
                return Err(stale_authority("probe baseline selection was substituted"));
            }
        }
        ProbeBaselineInputKind::Unavailable => {
            if actual.state != ProbeBaselineState::Unavailable
                || actual.comparison.is_some()
                || !all_baseline_bindings_absent(actual)
            {
                return Err(stale_authority("probe baseline selection was substituted"));
            }
        }
        ProbeBaselineInputKind::Comparable(prior) => {
            if !matches!(
                actual.state,
                ProbeBaselineState::Available | ProbeBaselineState::ResultIncomplete
            ) || !baseline_authority_matches(actual, &prior.authority)
            {
                return Err(stale_authority("probe baseline selection was substituted"));
            }
        }
        ProbeBaselineInputKind::Incompatible(authority) => {
            if actual.state != ProbeBaselineState::Incompatible
                || !baseline_authority_matches(actual, authority)
            {
                return Err(stale_authority("probe baseline selection was substituted"));
            }
        }
    }
    Ok(())
}

fn baseline_authority_matches(
    evidence: &ProbeBaselineEvidence,
    authority: &BaselineAuthorityBinding,
) -> bool {
    evidence.baseline_environment_digest.as_ref() == Some(&authority.environment_digest)
        && evidence.baseline_probe_definition_digest.as_ref()
            == Some(&authority.probe_definition_digest)
        && evidence.baseline_profile_digest.as_ref() == Some(&authority.profile_digest)
        && evidence.baseline_bundle_artifact_ref.as_ref() == Some(&authority.bundle_artifact_ref)
        && evidence.baseline_bundle_digest.as_ref() == Some(&authority.bundle_digest)
}

fn validate_self_contained_bundle(bundle: &ProbeEvidenceBundle) -> Result<(), ProbeEvidenceError> {
    if bundle.schema_version != 1
        || bundle.identity != bundle.l0_receipt.identity
        || bundle.plan_digest != bundle.l0_receipt.plan_digest
        || !valid_sha256_digest(&bundle.plan_digest)
        || !valid_sha256_digest(&bundle.probe_definition_digest)
        || bundle.normalized_at != bundle.l0_receipt.finished_at
    {
        return Err(stale_authority(
            "probe evidence internal authority is stale",
        ));
    }
    seal_probe_normalizer_profile(bundle.profile.clone())?;
    validate_raw_bindings(&bundle.raw_streams, &bundle.l0_receipt)?;
    validate_completeness(&bundle.completeness)?;
    validate_completeness_bindings(
        &bundle.completeness,
        &bundle.raw_streams,
        &bundle.l0_receipt,
    )?;
    validate_diagnostics(&bundle.diagnostics)?;
    validate_failed_tests(&bundle.failed_tests, &bundle.diagnostics)?;
    validate_stack_clusters(&bundle.stack_clusters)?;
    validate_workspace_delta(&bundle.workspace_delta)?;
    validate_baseline(
        &bundle.baseline,
        &bundle.identity,
        &bundle.probe_definition_digest,
        &bundle.profile,
        &bundle.diagnostics,
        &bundle.completeness,
    )?;
    validate_target_hypothesis_shape(&bundle.target_hypothesis_ids)?;
    if !valid_sha256_digest(&bundle.bundle_digest)
        || derive_probe_evidence_bundle_digest(bundle)? != bundle.bundle_digest
    {
        return Err(artifact_mismatch("probe evidence bundle digest is invalid"));
    }
    Ok(())
}

/// Seals an owned durable bundle after recomputing every semantic invariant.
///
/// # Errors
///
/// Returns the same validation failures as [`validate_probe_evidence_bundle`].
fn seal_probe_evidence_bundle(
    bundle: ProbeEvidenceBundle,
    intent: &ValidatedProbeExecutionIntent,
) -> Result<ValidatedProbeEvidenceBundle, ProbeEvidenceError> {
    validate_probe_evidence_bundle(&bundle, intent)?;
    Ok(ValidatedProbeEvidenceBundle { bundle })
}

/// Parses and seals canonical stored bundle bytes without rerunning a parser.
///
/// # Errors
///
/// Rejects malformed JSON, non-canonical serialization, journal identity
/// drift, or any semantic drift.
pub fn reopen_probe_evidence_bundle_from_journal(
    bytes: &[u8],
    intent: &ValidatedProbeExecutionIntent,
    expected_artifact_ref: &ArtifactReference,
    expected_bundle_digest: &Sha256Digest,
    profile: &ValidatedProbeNormalizerProfile,
    baseline: &ValidatedProbeBaselineInput,
) -> Result<ValidatedProbeEvidenceBundle, ProbeEvidenceError> {
    let bundle: ProbeEvidenceBundle = serde_json::from_slice(bytes)
        .map_err(|_| invalid_probe("probe evidence bundle bytes are invalid"))?;
    validate_prior_storage_binding(
        &bundle,
        bytes,
        expected_artifact_ref,
        expected_bundle_digest,
    )?;
    let sealed = seal_probe_evidence_bundle(bundle, intent)?;
    validate_probe_evidence_bundle_inputs(&sealed, intent, profile, baseline)?;
    Ok(sealed)
}

/// Returns the unique canonical JSON bytes used for bundle Artifact storage.
///
/// # Errors
///
/// Returns an error only if generated serialization fails.
pub fn canonical_probe_evidence_bundle_bytes(
    bundle: &ValidatedProbeEvidenceBundle,
) -> Result<Vec<u8>, ProbeEvidenceError> {
    serde_json::to_vec(bundle.bundle())
        .map_err(|_| invalid_probe("probe evidence bundle serialization failed"))
}

/// Rebinds a freshly validated bundle to the exact canonical Artifact that is
/// persisted in the evidence journal.
///
/// # Errors
///
/// Rejects an invalid Artifact identity or a digest that does not match the
/// canonical bundle bytes.
pub fn bind_prior_probe_evidence_bundle(
    bundle: &ValidatedProbeEvidenceBundle,
    artifact_ref: ArtifactReference,
) -> Result<ValidatedPriorProbeEvidenceBundle, ProbeEvidenceError> {
    let bytes = canonical_probe_evidence_bundle_bytes(bundle)?;
    validate_prior_storage_binding(
        bundle.bundle(),
        &bytes,
        &artifact_ref,
        &bundle.bundle.bundle_digest,
    )?;
    Ok(ValidatedPriorProbeEvidenceBundle {
        bundle: bundle.bundle.clone(),
        artifact_ref,
    })
}

/// Reopens a prior canonical bundle using only its stored bytes and the two
/// independent identities retained by the journal.
///
/// This restart path deliberately returns a prior-only type: it proves exact
/// byte/semantic identity and all self-contained evidence invariants without
/// claiming to reconstruct the original private D2 intent seal.
///
/// # Errors
///
/// Rejects malformed or non-canonical bytes, Artifact digest drift, journal
/// semantic-digest drift, or any self-contained bundle invariant violation.
pub fn seal_prior_probe_evidence_bundle_bytes(
    bytes: &[u8],
    expected_artifact_ref: ArtifactReference,
    expected_bundle_digest: &Sha256Digest,
) -> Result<ValidatedPriorProbeEvidenceBundle, ProbeEvidenceError> {
    let bundle: ProbeEvidenceBundle = serde_json::from_slice(bytes)
        .map_err(|_| invalid_probe("prior probe evidence bundle bytes are invalid"))?;
    validate_prior_storage_binding(
        &bundle,
        bytes,
        &expected_artifact_ref,
        expected_bundle_digest,
    )?;
    Ok(ValidatedPriorProbeEvidenceBundle {
        bundle,
        artifact_ref: expected_artifact_ref,
    })
}

fn validate_prior_storage_binding(
    bundle: &ProbeEvidenceBundle,
    bytes: &[u8],
    artifact_ref: &ArtifactReference,
    expected_bundle_digest: &Sha256Digest,
) -> Result<(), ProbeEvidenceError> {
    validate_artifact_reference(artifact_ref)?;
    if !valid_sha256_digest(expected_bundle_digest)
        || bundle.bundle_digest != *expected_bundle_digest
        || sha256_digest(bytes) != artifact_ref.digest
    {
        return Err(artifact_mismatch(
            "prior probe evidence journal binding is invalid",
        ));
    }
    let canonical = serde_json::to_vec(bundle)
        .map_err(|_| invalid_probe("prior probe evidence serialization failed"))?;
    if canonical != bytes {
        return Err(invalid_probe(
            "prior probe evidence bundle bytes are not canonical",
        ));
    }
    validate_self_contained_bundle(bundle)
}

/// Projects one sealed bundle into a bounded summary and unpolarized evidence.
///
/// `bundleArtifactRef` addresses the exact canonical bytes returned by
/// [`canonical_probe_evidence_bundle_bytes`]. It is deliberately separate from
/// the L0 receipt's raw stream Artifact references.
///
/// # Errors
///
/// Rejects an invalid Artifact identity or content digest.
pub fn project_probe_evidence(
    bundle: &ValidatedProbeEvidenceBundle,
    bundle_artifact_ref: ArtifactReference,
) -> Result<ProbeEvidenceProjection, ProbeEvidenceError> {
    validate_artifact_reference(&bundle_artifact_ref)?;
    let bytes = canonical_probe_evidence_bundle_bytes(bundle)?;
    if sha256_digest(&bytes) != bundle_artifact_ref.digest {
        return Err(artifact_mismatch(
            "probe evidence bundle Artifact digest is invalid",
        ));
    }
    let value = bundle.bundle();
    let diagnostic_occurrence_count = value.diagnostics.iter().try_fold(0_i64, |total, item| {
        total
            .checked_add(item.occurrence_count)
            .ok_or_else(|| invalid_probe("diagnostic occurrence count overflowed"))
    })?;
    let summary_text = format!(
        "probe evidence: {} unique diagnostics ({} occurrences), {} failed tests, {} stack clusters, {} changed files; {}",
        value.diagnostics.len(),
        diagnostic_occurrence_count,
        value.failed_tests.len(),
        value.stack_clusters.len(),
        value.workspace_delta.changed_files.len(),
        match value.completeness.status {
            ProbeEvidenceCompletenessStatus::Complete => "complete",
            ProbeEvidenceCompletenessStatus::Incomplete => "incomplete",
        }
    );
    let candidates = value
        .target_hypothesis_ids
        .iter()
        .map(|target| {
            Ok(HypothesisEvidenceCandidate {
                target_hypothesis_id: target.clone(),
                evidence: DebugHypothesisEvidence {
                    artifact_ref: bundle_artifact_ref.clone(),
                    evidence_digest: derive_hypothesis_evidence_digest(
                        &value.identity,
                        target,
                        &value.bundle_digest,
                    )?,
                    identity: value.identity.clone(),
                    recorded_at: value.normalized_at.clone(),
                },
            })
        })
        .collect::<Result<Vec<_>, ProbeEvidenceError>>()?;
    let summary = ProbeEvidenceSummary {
        bundle_artifact_ref,
        bundle_digest: value.bundle_digest.clone(),
        changed_file_count: count_i64(value.workspace_delta.changed_files.len())?,
        completeness: value.completeness.clone(),
        diagnostic_occurrence_count,
        failed_test_count: count_i64(value.failed_tests.len())?,
        identity: value.identity.clone(),
        recorded_at: value.normalized_at.clone(),
        stack_cluster_count: count_i64(value.stack_clusters.len())?,
        summary: summary_text,
        unique_diagnostic_count: count_i64(value.diagnostics.len())?,
    };
    Ok(ProbeEvidenceProjection {
        summary,
        candidates,
    })
}

/// Derives the target-bound semantic evidence digest for one bundle.
///
/// # Errors
///
/// Returns an error if the generated identity cannot be serialized.
pub fn derive_hypothesis_evidence_digest(
    identity: &DebugProbeIdentity,
    target: &DebugHypothesisId,
    bundle_digest: &Sha256Digest,
) -> Result<Sha256Digest, ProbeEvidenceError> {
    let mut digest = FramedDigest::new(HYPOTHESIS_EVIDENCE_DIGEST_DOMAIN);
    let identity_bytes = serde_json::to_vec(identity)
        .map_err(|_| invalid_probe("hypothesis evidence identity serialization failed"))?;
    digest.bytes(&identity_bytes);
    digest.bytes(target.0.as_bytes());
    digest.bytes(bundle_digest.0.as_bytes());
    Ok(digest.finish())
}

struct NormalizedRawStreams<'a> {
    bindings: Vec<ProbeRawStreamBinding>,
    stdout: &'a [u8],
    stderr: &'a [u8],
    reasons: BTreeSet<IncompleteReasonKey>,
}

fn normalize_raw_streams<'a>(
    receipt: &ProbeExecutionReceipt,
    inputs: &'a [ProbeRawStreamInput<'a>],
) -> Result<NormalizedRawStreams<'a>, ProbeEvidenceError> {
    if inputs.len() > 2 {
        return Err(invalid_probe("probe raw stream count is invalid"));
    }
    let mut seen_streams = HashSet::new();
    let mut seen_artifacts = HashSet::new();
    let mut bindings = Vec::with_capacity(inputs.len());
    let mut stdout = &[][..];
    let mut stderr = &[][..];
    let mut reasons = BTreeSet::new();
    for input in inputs {
        validate_artifact_reference(&input.artifact_ref)?;
        if input.bytes.len() > MAX_RAW_STREAM_BYTES
            || !seen_streams.insert(raw_stream_tag(&input.stream))
            || !seen_artifacts.insert(input.artifact_ref.artifact_id.0.as_str())
        {
            return Err(invalid_probe("probe raw stream binding is invalid"));
        }
        if sha256_digest(input.bytes) != input.artifact_ref.digest {
            return Err(artifact_mismatch(
                "probe raw stream Artifact digest is invalid",
            ));
        }
        let encoding = if std::str::from_utf8(input.bytes).is_ok() {
            ProbeRawStreamEncoding::Utf8
        } else {
            reasons.insert(IncompleteReasonKey::InvalidUtf8);
            ProbeRawStreamEncoding::InvalidUtf8
        };
        match input.stream {
            ProbeRawStream::Stdout => stdout = input.bytes,
            ProbeRawStream::Stderr => stderr = input.bytes,
        }
        bindings.push(ProbeRawStreamBinding {
            artifact_ref: input.artifact_ref.clone(),
            encoding,
            retained_bytes: count_i64(input.bytes.len())?,
            stream: input.stream.clone(),
        });
    }
    bindings.sort_by_key(|binding| raw_stream_rank(&binding.stream));
    validate_raw_bindings(&bindings, receipt)?;
    Ok(NormalizedRawStreams {
        bindings,
        stdout,
        stderr,
        reasons,
    })
}

fn validate_raw_bindings(
    bindings: &[ProbeRawStreamBinding],
    receipt: &ProbeExecutionReceipt,
) -> Result<(), ProbeEvidenceError> {
    if bindings.len() > 2 {
        return Err(invalid_probe("probe raw stream count is invalid"));
    }
    let mut seen_streams = HashSet::new();
    let mut seen_artifacts = HashSet::new();
    let mut previous_rank = None;
    let mut retained = 0_i64;
    for binding in bindings {
        validate_artifact_reference(&binding.artifact_ref)?;
        let rank = raw_stream_rank(&binding.stream);
        if previous_rank.is_some_and(|previous| previous >= rank)
            || !seen_streams.insert(raw_stream_tag(&binding.stream))
            || !seen_artifacts.insert(binding.artifact_ref.artifact_id.0.as_str())
            || !(0..=16_777_216).contains(&binding.retained_bytes)
        {
            return Err(invalid_probe("probe raw stream binding is invalid"));
        }
        previous_rank = Some(rank);
        retained = retained
            .checked_add(binding.retained_bytes)
            .ok_or_else(|| invalid_probe("probe raw byte count overflowed"))?;
    }
    if retained != receipt.output_bytes {
        return Err(invalid_probe("probe raw byte count does not match receipt"));
    }
    let raw_refs = bindings
        .iter()
        .map(|binding| artifact_key(&binding.artifact_ref))
        .collect::<BTreeSet<_>>();
    let receipt_refs = receipt
        .artifact_refs
        .iter()
        .map(artifact_key)
        .collect::<BTreeSet<_>>();
    if raw_refs.len() != bindings.len()
        || receipt_refs.len() != receipt.artifact_refs.len()
        || raw_refs != receipt_refs
    {
        return Err(stale_authority(
            "probe raw Artifacts do not match receipt Artifacts",
        ));
    }
    Ok(())
}

fn validate_completeness_bindings(
    completeness: &ProbeEvidenceCompleteness,
    raw_streams: &[ProbeRawStreamBinding],
    receipt: &ProbeExecutionReceipt,
) -> Result<(), ProbeEvidenceError> {
    let has_truncation_reason = completeness
        .reasons
        .contains(&ProbeEvidenceIncompleteReason::OutputTruncated);
    let has_invalid_utf8_reason = completeness
        .reasons
        .contains(&ProbeEvidenceIncompleteReason::InvalidUtf8);
    let has_invalid_utf8_stream = raw_streams
        .iter()
        .any(|binding| binding.encoding == ProbeRawStreamEncoding::InvalidUtf8);
    if receipt.output_truncated != has_truncation_reason
        || has_invalid_utf8_stream != has_invalid_utf8_reason
    {
        return Err(invalid_probe(
            "probe evidence completeness does not match raw receipt facts",
        ));
    }
    Ok(())
}

fn normalize_stack_clusters(
    version: &ProbeStackParserVersion,
    raw_streams: &[ProbeRawStreamInput<'_>],
    workspace_root: &Path,
    reasons: &mut BTreeSet<IncompleteReasonKey>,
) -> Result<Vec<ProbeStackCluster>, ProbeEvidenceError> {
    let mut clusters: BTreeMap<String, (Vec<ProbeStackFrame>, i64, BTreeSet<u8>)> = BTreeMap::new();
    for input in raw_streams {
        let Ok(text) = std::str::from_utf8(input.bytes) else {
            continue;
        };
        match parse_stack_occurrences(version, text, workspace_root) {
            Ok(parsed) => {
                if parsed.too_many_frames {
                    reasons.insert(IncompleteReasonKey::TooManyStackFrames);
                }
                for frames in parsed.occurrences {
                    let stack_digest = derive_stack_digest(&frames)?;
                    if !clusters.contains_key(&stack_digest.0)
                        && clusters.len() == MAX_STACK_CLUSTERS
                    {
                        reasons.insert(IncompleteReasonKey::TooManyStackClusters);
                        continue;
                    }
                    let entry = clusters
                        .entry(stack_digest.0.clone())
                        .or_insert_with(|| (frames, 0, BTreeSet::new()));
                    entry.1 = entry
                        .1
                        .checked_add(1)
                        .ok_or_else(|| invalid_probe("stack occurrence count overflowed"))?;
                    if entry.1 > MAX_OCCURRENCES {
                        return Err(invalid_probe("stack occurrence count is invalid"));
                    }
                    entry.2.insert(raw_stream_rank(&input.stream));
                }
            }
            Err(()) => {
                reasons.insert(IncompleteReasonKey::InvalidPayload);
            }
        }
    }
    clusters
        .into_iter()
        .map(|(digest, (frames, occurrence_count, stream_ranks))| {
            let source_streams = stream_ranks
                .into_iter()
                .map(|rank| {
                    if rank == 0 {
                        ProbeRawStream::Stdout
                    } else {
                        ProbeRawStream::Stderr
                    }
                })
                .collect::<Vec<_>>();
            let summary = format!(
                "stack cluster: {} frames, {} occurrences",
                frames.len(),
                occurrence_count
            );
            Ok(ProbeStackCluster {
                stack_digest: Sha256Digest(digest),
                frames,
                occurrence_count,
                source_streams,
                summary,
            })
        })
        .collect()
}

struct ParsedStacks {
    occurrences: Vec<Vec<ProbeStackFrame>>,
    too_many_frames: bool,
}

fn parse_stack_occurrences(
    version: &ProbeStackParserVersion,
    text: &str,
    workspace_root: &Path,
) -> Result<ParsedStacks, ()> {
    let sections = match version {
        ProbeStackParserVersion::RustV1 => sections_at_marker(text, "stack backtrace:"),
        ProbeStackParserVersion::NodeV1 => node_stack_sections(text),
        ProbeStackParserVersion::PythonV1 => {
            sections_at_marker(text, "Traceback (most recent call last):")
        }
    };
    if sections.is_empty() {
        let markerless_frames = match version {
            ProbeStackParserVersion::RustV1 => text.lines().any(|line| {
                line.trim()
                    .split_once(':')
                    .is_some_and(|(ordinal, function)| {
                        !ordinal.is_empty()
                            && ordinal.bytes().all(|byte| byte.is_ascii_digit())
                            && !function.trim().is_empty()
                    })
            }),
            ProbeStackParserVersion::NodeV1 => {
                text.lines().any(|line| line.trim().starts_with("at "))
            }
            ProbeStackParserVersion::PythonV1 => {
                text.lines().any(|line| line.trim().starts_with("File \""))
            }
        };
        if markerless_frames {
            return Err(());
        }
        return Ok(ParsedStacks {
            occurrences: Vec::new(),
            too_many_frames: false,
        });
    }
    let mut occurrences = Vec::with_capacity(sections.len());
    let mut too_many_frames = false;
    for section in sections {
        let mut frames = match version {
            ProbeStackParserVersion::RustV1 => parse_rust_stack(section, workspace_root),
            ProbeStackParserVersion::NodeV1 => parse_node_stack(section, workspace_root),
            ProbeStackParserVersion::PythonV1 => parse_python_stack(section, workspace_root),
        };
        if frames.is_empty() {
            return Err(());
        }
        if frames.len() > MAX_STACK_FRAMES {
            frames.truncate(MAX_STACK_FRAMES);
            too_many_frames = true;
        }
        occurrences.push(frames);
    }
    Ok(ParsedStacks {
        occurrences,
        too_many_frames,
    })
}

fn sections_at_marker<'a>(text: &'a str, marker: &str) -> Vec<&'a str> {
    let mut offset = 0_usize;
    let mut starts = Vec::new();
    for line in text.split_inclusive('\n') {
        if line.trim() == marker {
            starts.push(offset);
        }
        offset += line.len();
    }
    starts
        .iter()
        .enumerate()
        .map(|(index, start)| {
            let end = starts.get(index + 1).copied().unwrap_or(text.len());
            &text[*start..end]
        })
        .collect()
}

fn node_stack_sections(text: &str) -> Vec<&str> {
    let mut starts = Vec::new();
    let mut offset = 0_usize;
    let lines = text.split_inclusive('\n').collect::<Vec<_>>();
    for (index, line) in lines.iter().enumerate() {
        let next_is_frame = lines
            .get(index + 1)
            .is_some_and(|next| next.trim().starts_with("at "));
        if next_is_frame && node_stack_header(line.trim()) {
            starts.push(offset);
        }
        offset += line.len();
    }
    starts
        .iter()
        .enumerate()
        .map(|(index, start)| {
            let end = starts.get(index + 1).copied().unwrap_or(text.len());
            &text[*start..end]
        })
        .collect()
}

fn node_stack_header(value: &str) -> bool {
    [
        "Error",
        "TypeError",
        "RangeError",
        "ReferenceError",
        "SyntaxError",
        "URIError",
        "EvalError",
        "AggregateError",
    ]
    .iter()
    .any(|name| {
        value
            .strip_prefix(name)
            .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(':'))
    })
}

fn parse_rust_stack(text: &str, workspace_root: &Path) -> Vec<ProbeStackFrame> {
    let mut frames = Vec::new();
    let mut pending_function: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some((ordinal, function)) = trimmed.split_once(':')
            && !ordinal.is_empty()
            && ordinal.bytes().all(|byte| byte.is_ascii_digit())
            && !function.trim().is_empty()
        {
            if let Some(previous) = pending_function.take() {
                frames.push(stack_frame(Some(previous), None, None, None));
            }
            pending_function = bounded_function(function.trim());
            continue;
        }
        if let Some(location) = trimmed.strip_prefix("at ")
            && let Some(function) = pending_function.take()
        {
            let (path, line, column) = parse_location(location, workspace_root);
            frames.push(stack_frame(Some(function), path, line, column));
        }
    }
    if let Some(function) = pending_function {
        frames.push(stack_frame(Some(function), None, None, None));
    }
    frames
}

fn parse_node_stack(text: &str, workspace_root: &Path) -> Vec<ProbeStackFrame> {
    text.lines()
        .filter_map(|line| {
            let raw = line.trim().strip_prefix("at ")?.trim();
            let (function, location) = if let Some(open) = raw.rfind(" (") {
                let location = raw.get(open + 2..)?.strip_suffix(')')?;
                (bounded_function(raw[..open].trim()), location)
            } else {
                (None, raw.strip_prefix("async ").unwrap_or(raw))
            };
            let location = location.strip_prefix("file://").unwrap_or(location);
            let (path, line, column) = parse_location(location, workspace_root);
            if function.is_none() && path.is_none() && line.is_none() && column.is_none() {
                None
            } else {
                Some(stack_frame(function, path, line, column))
            }
        })
        .collect()
}

fn parse_python_stack(text: &str, workspace_root: &Path) -> Vec<ProbeStackFrame> {
    text.lines()
        .filter_map(|line| {
            let raw = line.trim().strip_prefix("File \"")?;
            let (path_text, rest) = raw.split_once("\", line ")?;
            let (line_text, function_text) = rest.split_once(", in ")?;
            let line_number = line_text
                .parse::<i64>()
                .ok()
                .filter(|value| valid_line_number(*value));
            let path = normalize_stack_path(path_text, workspace_root);
            let function = bounded_function(function_text.trim());
            Some(stack_frame(function, path, line_number, None))
        })
        .collect()
}

fn parse_location(
    location: &str,
    workspace_root: &Path,
) -> (Option<String>, Option<i64>, Option<i64>) {
    let mut parts = location.rsplitn(3, ':');
    let column = parts
        .next()
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| valid_line_number(*value));
    let line = parts
        .next()
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| valid_line_number(*value));
    let path = parts
        .next()
        .and_then(|value| normalize_stack_path(value, workspace_root));
    (path, line, column)
}

fn normalize_stack_path(value: &str, workspace_root: &Path) -> Option<String> {
    let value = value.strip_prefix("file://").unwrap_or(value);
    let candidate = Path::new(value);
    let relative = if candidate.is_absolute() {
        candidate.strip_prefix(workspace_root).ok()?
    } else {
        candidate
    };
    if relative.as_os_str().is_empty()
        || relative.components().any(|component| {
            !matches!(component, Component::Normal(_))
                || component
                    .as_os_str()
                    .to_string_lossy()
                    .contains(['\0', '\r', '\n'])
        })
    {
        return None;
    }
    let normalized = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    (normalized.chars().count() <= 4_096).then_some(normalized)
}

fn bounded_function(value: &str) -> Option<String> {
    (!value.is_empty() && value.chars().count() <= 500 && !value.contains(['\0', '\r', '\n']))
        .then(|| value.to_owned())
}

fn stack_frame(
    function: Option<String>,
    path: Option<String>,
    line: Option<i64>,
    column: Option<i64>,
) -> ProbeStackFrame {
    ProbeStackFrame {
        column,
        function,
        line,
        path,
    }
}

fn derive_stack_digest(frames: &[ProbeStackFrame]) -> Result<Sha256Digest, ProbeEvidenceError> {
    let mut digest = FramedDigest::new(STACK_DIGEST_DOMAIN);
    digest.list_len(frames.len())?;
    for frame in frames {
        digest.json(frame)?;
    }
    Ok(digest.finish())
}

fn validate_stack_clusters(clusters: &[ProbeStackCluster]) -> Result<(), ProbeEvidenceError> {
    if clusters.len() > MAX_STACK_CLUSTERS {
        return Err(invalid_probe("stack cluster count exceeds its limit"));
    }
    let mut previous_digest: Option<&str> = None;
    for cluster in clusters {
        if cluster.frames.is_empty()
            || cluster.frames.len() > MAX_STACK_FRAMES
            || !(1..=MAX_OCCURRENCES).contains(&cluster.occurrence_count)
            || cluster.source_streams.is_empty()
            || cluster.source_streams.len() > 2
            || !bounded_single_line(&cluster.summary, 500)
        {
            return Err(invalid_probe("stack cluster shape is invalid"));
        }
        let mut prior_rank = None;
        for stream in &cluster.source_streams {
            let rank = raw_stream_rank(stream);
            if prior_rank.is_some_and(|prior| prior >= rank) {
                return Err(invalid_probe("stack source stream order is invalid"));
            }
            prior_rank = Some(rank);
        }
        for frame in &cluster.frames {
            validate_stack_frame(frame)?;
        }
        let expected_digest = derive_stack_digest(&cluster.frames)?;
        let expected_summary = format!(
            "stack cluster: {} frames, {} occurrences",
            cluster.frames.len(),
            cluster.occurrence_count
        );
        if cluster.stack_digest != expected_digest || cluster.summary != expected_summary {
            return Err(artifact_mismatch(
                "stack cluster derived fields are invalid",
            ));
        }
        if previous_digest.is_some_and(|previous| previous >= cluster.stack_digest.0.as_str()) {
            return Err(invalid_probe("stack cluster order is invalid"));
        }
        previous_digest = Some(&cluster.stack_digest.0);
    }
    Ok(())
}

fn validate_stack_frame(frame: &ProbeStackFrame) -> Result<(), ProbeEvidenceError> {
    if frame.function.is_none()
        && frame.path.is_none()
        && frame.line.is_none()
        && frame.column.is_none()
        || frame
            .function
            .as_ref()
            .is_some_and(|value| !bounded_single_line(value, 500))
        || frame
            .path
            .as_ref()
            .is_some_and(|value| !canonical_relative_path(value))
        || frame.line.is_some_and(|value| !valid_line_number(value))
        || frame.column.is_some_and(|value| !valid_line_number(value))
    {
        return Err(invalid_probe("stack frame is invalid"));
    }
    Ok(())
}

fn validate_diagnostics(
    diagnostics: &[ProbeDiagnosticOccurrence],
) -> Result<(), ProbeEvidenceError> {
    if diagnostics.len() > 4_096 {
        return Err(invalid_probe("diagnostic count exceeds its limit"));
    }
    let mut previous: Option<&str> = None;
    for occurrence in diagnostics {
        let diagnostic = &occurrence.diagnostic;
        validate_normalized_diagnostic(diagnostic)
            .map_err(|_| invalid_probe("normalized diagnostic is invalid"))?;
        if !(1..=MAX_OCCURRENCES).contains(&occurrence.occurrence_count)
            || previous.is_some_and(|value| value >= diagnostic.diagnostic_id.0.as_str())
        {
            return Err(invalid_probe("normalized diagnostic order is invalid"));
        }
        previous = Some(&diagnostic.diagnostic_id.0);
    }
    Ok(())
}

fn validate_failed_tests(
    failed_tests: &[ProbeFailedTest],
    diagnostics: &[ProbeDiagnosticOccurrence],
) -> Result<(), ProbeEvidenceError> {
    if failed_tests.len() > 512 {
        return Err(invalid_probe("failed test evidence exceeds its limit"));
    }
    let expected = diagnostics
        .iter()
        .filter(|occurrence| occurrence.diagnostic.category == DiagnosticCategory::TestFailure)
        .map(|occurrence| ProbeFailedTest {
            diagnostic: occurrence.diagnostic.clone(),
            occurrence_count: occurrence.occurrence_count,
        })
        .collect::<Vec<_>>();
    if failed_tests != expected.as_slice() {
        return Err(invalid_probe("failed test evidence is invalid"));
    }
    Ok(())
}

fn validate_baseline_selection(
    baseline: &ValidatedProbeBaselineInput,
    profile: &ValidatedProbeNormalizerProfile,
) -> Result<(), ProbeEvidenceError> {
    match (
        profile.profile.diagnostic_parser_version.is_some(),
        &baseline.kind,
    ) {
        (false, ProbeBaselineInputKind::NotApplicable)
        | (
            true,
            ProbeBaselineInputKind::Unavailable
            | ProbeBaselineInputKind::Comparable(_)
            | ProbeBaselineInputKind::Incompatible(_),
        ) => Ok(()),
        (false, _) => Err(invalid_probe(
            "baseline evidence requires a selected diagnostic parser",
        )),
        (true, ProbeBaselineInputKind::NotApplicable) => Err(invalid_probe(
            "diagnostic normalization requires an explicit baseline state",
        )),
    }
}

fn build_probe_baseline_evidence(
    input: &ValidatedProbeBaselineInput,
    profile: &ValidatedProbeNormalizerProfile,
    identity: &DebugProbeIdentity,
    probe_definition_digest: &Sha256Digest,
    diagnostics: &[NormalizedDiagnostic],
    diagnostic_parse_complete: bool,
) -> Result<ProbeBaselineEvidence, ProbeEvidenceError> {
    let empty = || ProbeBaselineEvidence {
        baseline_bundle_artifact_ref: None,
        baseline_bundle_digest: None,
        baseline_environment_digest: None,
        baseline_probe_definition_digest: None,
        baseline_profile_digest: None,
        comparison: None,
        state: ProbeBaselineState::NotApplicable,
    };
    match &input.kind {
        ProbeBaselineInputKind::NotApplicable => Ok(empty()),
        ProbeBaselineInputKind::Unavailable => Ok(ProbeBaselineEvidence {
            state: ProbeBaselineState::Unavailable,
            ..empty()
        }),
        ProbeBaselineInputKind::Incompatible(authority) => Ok(baseline_from_authority(
            ProbeBaselineState::Incompatible,
            authority,
            None,
        )),
        ProbeBaselineInputKind::Comparable(prior) if !diagnostic_parse_complete => Ok(
            baseline_from_authority(ProbeBaselineState::ResultIncomplete, &prior.authority, None),
        ),
        ProbeBaselineInputKind::Comparable(prior) => {
            if prior.authority.environment_digest != identity.environment_digest
                || prior.authority.probe_definition_digest != *probe_definition_digest
                || prior.authority.profile_digest != profile.profile.profile_digest
            {
                return Err(stale_authority(
                    "selected probe baseline authority is stale",
                ));
            }
            let Some(parser_version) = profile.profile.diagnostic_parser_version.clone() else {
                return Err(invalid_probe(
                    "diagnostic baseline requires a selected diagnostic parser",
                ));
            };
            let baseline = build_diagnostic_baseline(
                prior.revision.clone(),
                &[DiagnosticParseBatch {
                    parser_version: parser_version.clone(),
                    diagnostics: prior.diagnostics.clone(),
                }],
            )
            .map_err(|_| invalid_probe("selected diagnostic baseline is invalid"))?;
            let result = build_diagnostic_baseline(
                identity.workspace_revision.clone(),
                &[DiagnosticParseBatch {
                    parser_version,
                    diagnostics: diagnostics.to_vec(),
                }],
            )
            .map_err(|_| invalid_probe("normalized diagnostic result is invalid"))?;
            let comparison = compare_diagnostic_baselines(&baseline, &result)
                .map_err(|_| invalid_probe("diagnostic baseline comparison failed"))?;
            Ok(baseline_from_authority(
                ProbeBaselineState::Available,
                &prior.authority,
                Some(comparison),
            ))
        }
    }
}

fn baseline_from_authority(
    state: ProbeBaselineState,
    authority: &BaselineAuthorityBinding,
    comparison: Option<DiagnosticBaselineComparison>,
) -> ProbeBaselineEvidence {
    ProbeBaselineEvidence {
        baseline_bundle_artifact_ref: Some(authority.bundle_artifact_ref.clone()),
        baseline_bundle_digest: Some(authority.bundle_digest.clone()),
        baseline_environment_digest: Some(authority.environment_digest.clone()),
        baseline_probe_definition_digest: Some(authority.probe_definition_digest.clone()),
        baseline_profile_digest: Some(authority.profile_digest.clone()),
        comparison,
        state,
    }
}

fn validate_baseline(
    baseline: &ProbeBaselineEvidence,
    identity: &DebugProbeIdentity,
    probe_definition_digest: &Sha256Digest,
    profile: &ProbeNormalizerProfile,
    diagnostics: &[ProbeDiagnosticOccurrence],
    completeness: &ProbeEvidenceCompleteness,
) -> Result<(), ProbeEvidenceError> {
    if matches!(baseline.state, ProbeBaselineState::NotApplicable)
        != profile.diagnostic_parser_version.is_none()
    {
        return Err(invalid_probe(
            "probe diagnostic profile and baseline state are inconsistent",
        ));
    }
    match baseline.state {
        ProbeBaselineState::NotApplicable => {
            if baseline.comparison.is_some() || !all_baseline_bindings_absent(baseline) {
                return Err(invalid_probe("not-applicable baseline evidence is invalid"));
            }
            require_no_baseline_reason(completeness)
        }
        ProbeBaselineState::Unavailable => {
            if baseline.comparison.is_some() || !all_baseline_bindings_absent(baseline) {
                return Err(invalid_probe("unavailable baseline evidence is invalid"));
            }
            require_reason_present(
                completeness,
                &ProbeEvidenceIncompleteReason::BaselineUnavailable,
            )?;
            require_reason_absent(
                completeness,
                &ProbeEvidenceIncompleteReason::BaselineIncompatible,
            )
        }
        ProbeBaselineState::Available => validate_available_baseline(
            baseline,
            identity,
            probe_definition_digest,
            profile,
            diagnostics,
            completeness,
        ),
        ProbeBaselineState::Incompatible => validate_incompatible_baseline(
            baseline,
            identity,
            probe_definition_digest,
            profile,
            completeness,
        ),
        ProbeBaselineState::ResultIncomplete => validate_result_incomplete_baseline(
            baseline,
            identity,
            probe_definition_digest,
            profile,
            completeness,
        ),
    }
}

fn validate_available_baseline(
    baseline: &ProbeBaselineEvidence,
    identity: &DebugProbeIdentity,
    probe_definition_digest: &Sha256Digest,
    profile: &ProbeNormalizerProfile,
    diagnostics: &[ProbeDiagnosticOccurrence],
    completeness: &ProbeEvidenceCompleteness,
) -> Result<(), ProbeEvidenceError> {
    let Some(comparison) = baseline.comparison.as_ref() else {
        return Err(invalid_probe("available baseline comparison is missing"));
    };
    let (Some(environment), Some(definition), Some(profile_digest), Some(artifact), Some(digest)) = (
        baseline.baseline_environment_digest.as_ref(),
        baseline.baseline_probe_definition_digest.as_ref(),
        baseline.baseline_profile_digest.as_ref(),
        baseline.baseline_bundle_artifact_ref.as_ref(),
        baseline.baseline_bundle_digest.as_ref(),
    ) else {
        return Err(invalid_probe("available baseline authority is incomplete"));
    };
    validate_artifact_reference(artifact)?;
    if !valid_sha256_digest(digest)
        || environment != &identity.environment_digest
        || definition != probe_definition_digest
        || profile_digest != &profile.profile_digest
        || comparison.result_revision != identity.workspace_revision
    {
        return Err(stale_authority("available baseline authority is stale"));
    }
    validate_comparison_against_diagnostics(comparison, profile, diagnostics)?;
    require_no_baseline_reason(completeness)
}

fn validate_incompatible_baseline(
    baseline: &ProbeBaselineEvidence,
    identity: &DebugProbeIdentity,
    probe_definition_digest: &Sha256Digest,
    profile: &ProbeNormalizerProfile,
    completeness: &ProbeEvidenceCompleteness,
) -> Result<(), ProbeEvidenceError> {
    if baseline.comparison.is_some() {
        return Err(invalid_probe(
            "incompatible baseline comparison must be absent",
        ));
    }
    let (Some(environment), Some(definition), Some(profile_digest), Some(artifact), Some(digest)) = (
        baseline.baseline_environment_digest.as_ref(),
        baseline.baseline_probe_definition_digest.as_ref(),
        baseline.baseline_profile_digest.as_ref(),
        baseline.baseline_bundle_artifact_ref.as_ref(),
        baseline.baseline_bundle_digest.as_ref(),
    ) else {
        return Err(invalid_probe(
            "incompatible baseline authority is incomplete",
        ));
    };
    validate_artifact_reference(artifact)?;
    if !valid_sha256_digest(digest)
        || (environment == &identity.environment_digest
            && definition == probe_definition_digest
            && profile_digest == &profile.profile_digest)
    {
        return Err(invalid_probe("incompatible baseline authority is invalid"));
    }
    require_reason_present(
        completeness,
        &ProbeEvidenceIncompleteReason::BaselineIncompatible,
    )?;
    require_reason_absent(
        completeness,
        &ProbeEvidenceIncompleteReason::BaselineUnavailable,
    )
}

fn validate_result_incomplete_baseline(
    baseline: &ProbeBaselineEvidence,
    identity: &DebugProbeIdentity,
    probe_definition_digest: &Sha256Digest,
    profile: &ProbeNormalizerProfile,
    completeness: &ProbeEvidenceCompleteness,
) -> Result<(), ProbeEvidenceError> {
    if baseline.comparison.is_some() {
        return Err(invalid_probe(
            "result-incomplete baseline comparison must be absent",
        ));
    }
    let (Some(environment), Some(definition), Some(profile_digest), Some(artifact), Some(digest)) = (
        baseline.baseline_environment_digest.as_ref(),
        baseline.baseline_probe_definition_digest.as_ref(),
        baseline.baseline_profile_digest.as_ref(),
        baseline.baseline_bundle_artifact_ref.as_ref(),
        baseline.baseline_bundle_digest.as_ref(),
    ) else {
        return Err(invalid_probe(
            "result-incomplete baseline authority is incomplete",
        ));
    };
    validate_artifact_reference(artifact)?;
    if !valid_sha256_digest(digest)
        || environment != &identity.environment_digest
        || definition != probe_definition_digest
        || profile_digest != &profile.profile_digest
        || completeness.status != ProbeEvidenceCompletenessStatus::Incomplete
    {
        return Err(stale_authority(
            "result-incomplete baseline authority is stale",
        ));
    }
    require_no_baseline_reason(completeness)
}

fn require_no_baseline_reason(
    completeness: &ProbeEvidenceCompleteness,
) -> Result<(), ProbeEvidenceError> {
    require_reason_absent(
        completeness,
        &ProbeEvidenceIncompleteReason::BaselineUnavailable,
    )?;
    require_reason_absent(
        completeness,
        &ProbeEvidenceIncompleteReason::BaselineIncompatible,
    )
}

fn validate_comparison_against_diagnostics(
    comparison: &DiagnosticBaselineComparison,
    profile: &ProbeNormalizerProfile,
    diagnostics: &[ProbeDiagnosticOccurrence],
) -> Result<(), ProbeEvidenceError> {
    let Some(parser_version) = profile.diagnostic_parser_version.clone() else {
        return Err(invalid_probe(
            "diagnostic baseline requires a selected diagnostic parser",
        ));
    };
    let baseline_diagnostics = comparison
        .entries
        .iter()
        .filter(|entry| {
            matches!(
                entry.status,
                DiagnosticChangeStatus::Resolved | DiagnosticChangeStatus::Unchanged
            )
        })
        .map(|entry| entry.diagnostic.clone())
        .collect::<Vec<_>>();
    let baseline = build_diagnostic_baseline(
        comparison.base_revision.clone(),
        &[DiagnosticParseBatch {
            parser_version: parser_version.clone(),
            diagnostics: baseline_diagnostics,
        }],
    )
    .map_err(|_| invalid_probe("diagnostic baseline comparison is invalid"))?;
    let result = build_diagnostic_baseline(
        comparison.result_revision.clone(),
        &[DiagnosticParseBatch {
            parser_version,
            diagnostics: diagnostics
                .iter()
                .map(|occurrence| occurrence.diagnostic.clone())
                .collect(),
        }],
    )
    .map_err(|_| invalid_probe("diagnostic result baseline is invalid"))?;
    let expected = compare_diagnostic_baselines(&baseline, &result)
        .map_err(|_| invalid_probe("diagnostic baseline comparison is invalid"))?;
    if &expected != comparison {
        return Err(invalid_probe("diagnostic baseline comparison is invalid"));
    }
    Ok(())
}

fn validate_workspace_delta(delta: &ProbeWorkspaceDeltaEvidence) -> Result<(), ProbeEvidenceError> {
    if delta.state != ProbeWorkspaceDeltaState::NotApplicable || !delta.changed_files.is_empty() {
        return Err(invalid_probe("probe workspace delta state is invalid"));
    }
    Ok(())
}

fn validate_completeness(
    completeness: &ProbeEvidenceCompleteness,
) -> Result<(), ProbeEvidenceError> {
    if completeness.reasons.len() > 8 {
        return Err(invalid_probe("probe evidence completeness is invalid"));
    }
    let ranks = completeness
        .reasons
        .iter()
        .map(incomplete_reason_rank)
        .collect::<Vec<_>>();
    if ranks.windows(2).any(|pair| pair[0] >= pair[1])
        || matches!(
            (&completeness.status, completeness.reasons.is_empty()),
            (ProbeEvidenceCompletenessStatus::Complete, false)
                | (ProbeEvidenceCompletenessStatus::Incomplete, true)
        )
    {
        return Err(invalid_probe("probe evidence completeness is invalid"));
    }
    Ok(())
}

fn validate_target_hypotheses(
    targets: &[DebugHypothesisId],
    intent: &ValidatedProbeExecutionIntent,
) -> Result<(), ProbeEvidenceError> {
    let mut expected = intent.intent().spec.target_hypothesis_ids.clone();
    expected.sort_by(|left, right| left.0.cmp(&right.0));
    if targets != expected.as_slice()
        || targets
            .windows(2)
            .any(|pair| pair[0].0.as_str() >= pair[1].0.as_str())
    {
        return Err(stale_authority(
            "probe evidence hypothesis targets are stale",
        ));
    }
    Ok(())
}

fn validate_target_hypothesis_shape(
    targets: &[DebugHypothesisId],
) -> Result<(), ProbeEvidenceError> {
    if targets.is_empty()
        || targets.len() > 16
        || targets
            .iter()
            .any(|target| !prefixed_ulid(&target.0, "hyp_"))
        || targets
            .windows(2)
            .any(|pair| pair[0].0.as_str() >= pair[1].0.as_str())
    {
        return Err(invalid_probe(
            "probe evidence hypothesis target order is invalid",
        ));
    }
    Ok(())
}

fn completeness_from_keys(keys: &BTreeSet<IncompleteReasonKey>) -> ProbeEvidenceCompleteness {
    let reasons = keys
        .iter()
        .copied()
        .map(IncompleteReasonKey::wire)
        .collect();
    ProbeEvidenceCompleteness {
        status: if keys.is_empty() {
            ProbeEvidenceCompletenessStatus::Complete
        } else {
            ProbeEvidenceCompletenessStatus::Incomplete
        },
        reasons,
    }
}

fn add_baseline_reason(
    baseline: &ProbeBaselineEvidence,
    reasons: &mut BTreeSet<IncompleteReasonKey>,
) {
    match baseline.state {
        ProbeBaselineState::Unavailable => {
            reasons.insert(IncompleteReasonKey::BaselineUnavailable);
        }
        ProbeBaselineState::Incompatible => {
            reasons.insert(IncompleteReasonKey::BaselineIncompatible);
        }
        ProbeBaselineState::NotApplicable
        | ProbeBaselineState::Available
        | ProbeBaselineState::ResultIncomplete => {}
    }
}

fn incomplete_reason_for_diagnostic_error(code: DiagnosticParseErrorCode) -> IncompleteReasonKey {
    match code {
        DiagnosticParseErrorCode::InputTooLarge => IncompleteReasonKey::DiagnosticInputTooLarge,
        DiagnosticParseErrorCode::TruncatedInput => IncompleteReasonKey::OutputTruncated,
        DiagnosticParseErrorCode::InvalidUtf8 => IncompleteReasonKey::InvalidUtf8,
        DiagnosticParseErrorCode::TooManyDiagnostics => IncompleteReasonKey::TooManyDiagnostics,
        DiagnosticParseErrorCode::InvalidPath => IncompleteReasonKey::InvalidPath,
        DiagnosticParseErrorCode::InvalidPayload | DiagnosticParseErrorCode::InvalidBaseline => {
            IncompleteReasonKey::InvalidPayload
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum IncompleteReasonKey {
    OutputTruncated,
    InvalidUtf8,
    DiagnosticInputTooLarge,
    InvalidPayload,
    TooManyDiagnostics,
    InvalidPath,
    TooManyStackFrames,
    TooManyStackClusters,
    BaselineUnavailable,
    BaselineIncompatible,
}

impl IncompleteReasonKey {
    const fn wire(self) -> ProbeEvidenceIncompleteReason {
        match self {
            Self::OutputTruncated => ProbeEvidenceIncompleteReason::OutputTruncated,
            Self::InvalidUtf8 => ProbeEvidenceIncompleteReason::InvalidUtf8,
            Self::DiagnosticInputTooLarge => ProbeEvidenceIncompleteReason::DiagnosticInputTooLarge,
            Self::InvalidPayload => ProbeEvidenceIncompleteReason::InvalidPayload,
            Self::TooManyDiagnostics => ProbeEvidenceIncompleteReason::TooManyDiagnostics,
            Self::InvalidPath => ProbeEvidenceIncompleteReason::InvalidPath,
            Self::TooManyStackFrames => ProbeEvidenceIncompleteReason::TooManyStackFrames,
            Self::TooManyStackClusters => ProbeEvidenceIncompleteReason::TooManyStackClusters,
            Self::BaselineUnavailable => ProbeEvidenceIncompleteReason::BaselineUnavailable,
            Self::BaselineIncompatible => ProbeEvidenceIncompleteReason::BaselineIncompatible,
        }
    }
}

fn incomplete_reason_rank(reason: &ProbeEvidenceIncompleteReason) -> u8 {
    match reason {
        ProbeEvidenceIncompleteReason::OutputTruncated => 0,
        ProbeEvidenceIncompleteReason::InvalidUtf8 => 1,
        ProbeEvidenceIncompleteReason::DiagnosticInputTooLarge => 2,
        ProbeEvidenceIncompleteReason::InvalidPayload => 3,
        ProbeEvidenceIncompleteReason::TooManyDiagnostics => 4,
        ProbeEvidenceIncompleteReason::InvalidPath => 5,
        ProbeEvidenceIncompleteReason::TooManyStackFrames => 6,
        ProbeEvidenceIncompleteReason::TooManyStackClusters => 7,
        ProbeEvidenceIncompleteReason::BaselineUnavailable => 8,
        ProbeEvidenceIncompleteReason::BaselineIncompatible => 9,
    }
}

fn require_reason_present(
    completeness: &ProbeEvidenceCompleteness,
    reason: &ProbeEvidenceIncompleteReason,
) -> Result<(), ProbeEvidenceError> {
    if !completeness.reasons.contains(reason) {
        return Err(invalid_probe(
            "probe evidence completeness reason is missing",
        ));
    }
    Ok(())
}

fn require_reason_absent(
    completeness: &ProbeEvidenceCompleteness,
    reason: &ProbeEvidenceIncompleteReason,
) -> Result<(), ProbeEvidenceError> {
    if completeness.reasons.contains(reason) {
        return Err(invalid_probe(
            "probe evidence completeness reason is unexpected",
        ));
    }
    Ok(())
}

fn all_baseline_bindings_absent(baseline: &ProbeBaselineEvidence) -> bool {
    baseline.baseline_environment_digest.is_none()
        && baseline.baseline_probe_definition_digest.is_none()
        && baseline.baseline_profile_digest.is_none()
        && baseline.baseline_bundle_artifact_ref.is_none()
        && baseline.baseline_bundle_digest.is_none()
}

fn validate_artifact_reference(reference: &ArtifactReference) -> Result<(), ProbeEvidenceError> {
    if !prefixed_ulid(&reference.artifact_id.0, "art_") || !valid_sha256_digest(&reference.digest) {
        return Err(artifact_mismatch("probe Artifact reference is invalid"));
    }
    Ok(())
}

fn artifact_key(reference: &ArtifactReference) -> (String, String) {
    (reference.artifact_id.0.clone(), reference.digest.0.clone())
}

fn raw_stream_rank(stream: &ProbeRawStream) -> u8 {
    match stream {
        ProbeRawStream::Stdout => 0,
        ProbeRawStream::Stderr => 1,
    }
}

fn raw_stream_tag(stream: &ProbeRawStream) -> &'static str {
    match stream {
        ProbeRawStream::Stdout => "stdout",
        ProbeRawStream::Stderr => "stderr",
    }
}

fn normalizer_version_tag(version: &ProbeNormalizerVersion) -> &'static str {
    match version {
        ProbeNormalizerVersion::L0L1V1 => "l0_l1_v1",
    }
}

fn diagnostic_parser_version_tag(version: &DiagnosticParserVersion) -> &'static str {
    match version {
        DiagnosticParserVersion::EslintJsonV1 => "eslint_json_v1",
        DiagnosticParserVersion::TypescriptV1 => "typescript_v1",
        DiagnosticParserVersion::CargoJsonV1 => "cargo_json_v1",
        DiagnosticParserVersion::GoTestJsonV1 => "go_test_json_v1",
        DiagnosticParserVersion::JunitXmlV1 => "junit_xml_v1",
        DiagnosticParserVersion::PytestJsonV1 => "pytest_json_v1",
    }
}

fn stack_parser_version_tag(version: &ProbeStackParserVersion) -> &'static str {
    match version {
        ProbeStackParserVersion::RustV1 => "rust_v1",
        ProbeStackParserVersion::NodeV1 => "node_v1",
        ProbeStackParserVersion::PythonV1 => "python_v1",
    }
}

fn valid_line_number(value: i64) -> bool {
    (1..=i64::from(i32::MAX)).contains(&value)
}

fn canonical_relative_path(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= 4_096
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.contains(['\0', '\r', '\n', '\\'])
        && value
            .split('/')
            .all(|component| !component.is_empty() && !matches!(component, "." | ".."))
}

fn bounded_single_line(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.chars().count() <= maximum && !value.contains(['\0', '\r', '\n'])
}

fn valid_sha256_digest(digest: &Sha256Digest) -> bool {
    digest.0.strip_prefix("sha256:").is_some_and(|value| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}

fn prefixed_ulid(value: &str, prefix: &str) -> bool {
    value.len() == prefix.len() + 26
        && value.starts_with(prefix)
        && value[prefix.len()..].bytes().all(|byte| {
            byte.is_ascii_digit()
                || matches!(byte, b'A'..=b'H' | b'J'..=b'N' | b'P'..=b'T' | b'V'..=b'Z')
        })
}

fn count_i64(value: usize) -> Result<i64, ProbeEvidenceError> {
    i64::try_from(value).map_err(|_| invalid_probe("probe evidence count overflowed"))
}

fn sha256_digest(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

const fn invalid_probe(message: &'static str) -> ProbeEvidenceError {
    ProbeEvidenceError {
        code: DebugProbeErrorCode::InvalidProbe,
        message,
    }
}

const fn stale_authority(message: &'static str) -> ProbeEvidenceError {
    ProbeEvidenceError {
        code: DebugProbeErrorCode::StaleAuthority,
        message,
    }
}

const fn artifact_mismatch(message: &'static str) -> ProbeEvidenceError {
    ProbeEvidenceError {
        code: DebugProbeErrorCode::ArtifactDigestMismatch,
        message,
    }
}

struct FramedDigest(Sha256);

impl FramedDigest {
    fn new(domain: &[u8]) -> Self {
        let mut digest = Sha256::new();
        digest.update(domain);
        Self(digest)
    }

    fn bytes(&mut self, value: &[u8]) {
        self.0
            .update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
        self.0.update(value);
    }

    fn text(&mut self, value: &str) -> Result<(), ProbeEvidenceError> {
        if value.len() > usize::MAX / 2 {
            return Err(invalid_probe("probe evidence digest input is invalid"));
        }
        self.bytes(value.as_bytes());
        Ok(())
    }

    fn optional_text(&mut self, value: Option<&str>) -> Result<(), ProbeEvidenceError> {
        if let Some(value) = value {
            self.0.update([1]);
            self.text(value)
        } else {
            self.0.update([0]);
            Ok(())
        }
    }

    fn i64(&mut self, value: i64) {
        self.0.update(value.to_be_bytes());
    }

    fn list_len(&mut self, value: usize) -> Result<(), ProbeEvidenceError> {
        let value = u64::try_from(value)
            .map_err(|_| invalid_probe("probe evidence list length overflowed"))?;
        self.0.update(value.to_be_bytes());
        Ok(())
    }

    fn json<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), ProbeEvidenceError> {
        let bytes = serde_json::to_vec(value)
            .map_err(|_| invalid_probe("probe evidence digest serialization failed"))?;
        self.bytes(&bytes);
        Ok(())
    }

    fn finish(self) -> Sha256Digest {
        Sha256Digest(format!("sha256:{:x}", self.0.finalize()))
    }
}
