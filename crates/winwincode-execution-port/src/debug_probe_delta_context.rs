// SPDX-License-Identifier: Apache-2.0

//! Deterministic, bounded context deltas for one strong-model debug round.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::Serialize;
use sha2::{Digest as _, Sha256};
use winwincode_domain::{DebugHypothesisId, Sha256Digest};

use crate::debug_hypothesis_ledger::{
    ValidatedDebugHypothesisLedger, ValidatedDebugHypothesisRoundEvidence,
};
use crate::generated::{
    ArtifactReference, DebugConfirmedFact, DebugContextSafetyProfile,
    DebugContextSafetyScannerVersion, DebugContextSnippet, DebugDeltaContextBudget,
    DebugDeltaContextEstimatorVersion, DebugHypothesis, DebugHypothesisChange,
    DebugHypothesisEvidence, DebugHypothesisLedger, DebugProbeDeltaContext,
    DebugProbeRoundAuthority, DebugQuestionChange, DebugQuestionChangeKind,
    DebugReproductionRecipe, DebugUnresolvedQuestion, HypothesisEvidenceCandidate,
    ProbeEvidenceSummary,
};
use crate::probe_result_normalizer::derive_hypothesis_evidence_digest;

const BUDGET_HASH_DOMAIN: &[u8] = b"winwincode.debug-delta-context-budget.v1\0";
const CONTEXT_HASH_DOMAIN: &[u8] = b"winwincode.debug-probe-delta-context.v1\0";
const REQUEST_HASH_DOMAIN: &[u8] = b"winwincode.debug-probe.delta-request.v1\0";
const MAX_SERIALIZED_BYTES: usize = 32_768;
const MAX_ESTIMATED_TOKENS: usize = 8_192;
const MAX_SNIPPETS: usize = 8;
const MAX_SNIPPET_BYTES: usize = 2_000;
const MAX_TOTAL_SNIPPET_BYTES: usize = 16_000;

/// Host-owned scanner used by the production context assembly boundary.
///
/// This is an authority port, not a caller-selected policy. Production code
/// must wire its single registered scanner implementation and persist the
/// returned profile with the prepared context.
pub trait DebugContextSafetyScanner {
    /// Returns the immutable scanner policy bound into the context.
    fn profile(&self) -> &DebugContextSafetyProfile;

    /// Checks one bounded value without retaining it in an error.
    ///
    /// # Errors
    ///
    /// Returns a closed, content-free reason when the value is unsafe.
    fn validate(&self, text: &str) -> Result<(), ContextSafetyScanError>;
}

/// Content-free scanner failures safe to retain in host logs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextSafetyScanError {
    /// The value contains credential-shaped or otherwise sensitive material.
    SensitiveMaterial,
    /// The value identifies unbounded raw output or conversation history.
    RawContextMaterial,
}

impl fmt::Display for ContextSafetyScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SensitiveMaterial => "context text contains sensitive material",
            Self::RawContextMaterial => "context text contains raw context material",
        })
    }
}

impl std::error::Error for ContextSafetyScanError {}

/// Untrusted snippet fields accepted only by [`seal_debug_context_snippet`].
#[derive(Clone, Debug, PartialEq)]
pub struct DebugContextSnippetInput {
    /// Exact bounded snippet content; it is never truncated by the sealer.
    pub content: String,
    /// Inclusive ending source line when a line range is known.
    pub end_line: Option<i64>,
    /// Repository-relative source path when known.
    pub path: Option<String>,
    /// Artifact containing the source bytes.
    pub source_artifact_ref: ArtifactReference,
    /// Semantic digest of the D3 evidence that selected this snippet.
    pub source_evidence_digest: Sha256Digest,
    /// Inclusive starting source line when a line range is known.
    pub start_line: Option<i64>,
}

/// Opaque proof that one complete snippet passed bounds, binding and scanning.
#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedDebugContextSnippet {
    snippet: DebugContextSnippet,
}

/// Exact trusted inputs used to prepare one strong-model Delta Context.
pub struct DebugProbeDeltaContextInput<'a> {
    /// Latest committed Ledger projection before the model call.
    pub current_ledger: &'a ValidatedDebugHypothesisLedger,
    /// Previously prepared context cursor, absent only on the first round.
    pub previous_context: Option<&'a ValidatedDebugProbeDeltaContext>,
    /// Ledger snapshot named by `previous_context`, absent only on first round.
    pub previous_ledger: Option<&'a ValidatedDebugHypothesisLedger>,
    /// Current terminal probe receipt plus its exact D3 projections.
    pub round_evidence: &'a ValidatedDebugHypothesisRoundEvidence,
    /// Host exact-read snippets; the builder performs final source rebinding.
    pub snippets: &'a [ValidatedDebugContextSnippet],
}

/// Opaque exact context and the canonical bytes persisted by the Control Plane.
#[derive(Clone, Debug)]
pub struct ValidatedDebugProbeDeltaContext {
    context: DebugProbeDeltaContext,
    canonical_bytes: Vec<u8>,
}

impl ValidatedDebugProbeDeltaContext {
    /// Borrows the typed context.
    #[must_use]
    pub const fn context(&self) -> &DebugProbeDeltaContext {
        &self.context
    }

    /// Borrows the exact canonical bytes sent to the model and retained in Prepared.
    #[must_use]
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }

    /// Returns the typed context and exact canonical bytes.
    #[must_use]
    pub fn into_parts(self) -> (DebugProbeDeltaContext, Vec<u8>) {
        (self.context, self.canonical_bytes)
    }
}

impl ValidatedDebugContextSnippet {
    /// Borrows the exact generated value safe to place in a Delta Context.
    #[must_use]
    pub const fn snippet(&self) -> &DebugContextSnippet {
        &self.snippet
    }
}

/// Validates and seals one whole snippet without truncating its UTF-8 bytes.
///
/// # Errors
///
/// Returns a bounded error for malformed source binding, unsafe text, an
/// invalid scanner profile, or content above the fixed per-item limit.
pub fn seal_debug_context_snippet(
    input: DebugContextSnippetInput,
    scanner: &impl DebugContextSafetyScanner,
) -> Result<ValidatedDebugContextSnippet, DebugDeltaContextError> {
    validate_safety_profile(scanner.profile())?;
    validate_artifact_reference(&input.source_artifact_ref)?;
    validate_digest(&input.source_evidence_digest)?;
    validate_snippet_location(input.path.as_deref(), input.start_line, input.end_line)?;
    validate_context_text(&input.content, MAX_SNIPPET_BYTES, scanner)?;
    if let Some(path) = input.path.as_deref() {
        validate_portable_path(path)?;
        validate_context_text(path, 4_096, scanner)?;
    }

    let content_digest = Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(input.content.as_bytes())
    ));
    Ok(ValidatedDebugContextSnippet {
        snippet: DebugContextSnippet {
            content: input.content,
            content_digest,
            end_line: input.end_line,
            path: input.path,
            safety_profile: scanner.profile().clone(),
            source_artifact_ref: input.source_artifact_ref,
            source_evidence_digest: input.source_evidence_digest,
            start_line: input.start_line,
        },
    })
}

/// Builds the deterministic delta between the acknowledged Ledger cursor and
/// the latest committed Ledger, then adds only current sealed D3 evidence and
/// whole host-selected snippets that fit the fixed budget.
///
/// # Errors
///
/// Fails closed on cursor or authority drift, unsafe text, inexact evidence,
/// non-canonical collections, or required context above the hard token bound.
pub fn prepare_debug_probe_delta_context(
    input: &DebugProbeDeltaContextInput<'_>,
    scanner: &impl DebugContextSafetyScanner,
) -> Result<ValidatedDebugProbeDeltaContext, DebugDeltaContextError> {
    validate_safety_profile(scanner.profile())?;
    let current = input.current_ledger.ledger();
    let evidence = input.round_evidence.cut();
    validate_session_scope(&current.authority, &evidence.source_round_receipt.authority)?;
    validate_cursor(input.previous_ledger, input.previous_context, current)?;

    let hypothesis_changes = derive_hypothesis_changes(
        input
            .previous_ledger
            .map(ValidatedDebugHypothesisLedger::ledger),
        &current.hypotheses,
    )?;
    let confirmed_facts = derive_confirmed_facts(
        input
            .previous_ledger
            .map(ValidatedDebugHypothesisLedger::ledger),
        &current.confirmed_facts,
    )?;
    let reproduction_recipe = derive_reproduction_recipe(
        input
            .previous_ledger
            .map(ValidatedDebugHypothesisLedger::ledger),
        current.reproduction_recipe.as_ref(),
    );
    let question_changes = derive_question_changes(
        input
            .previous_ledger
            .map(ValidatedDebugHypothesisLedger::ledger),
        &current.unresolved_questions,
    )?;

    let mut summaries = evidence.evidence_summaries.clone();
    let mut candidates = evidence.evidence_candidates.clone();
    canonicalize_summaries(&mut summaries);
    canonicalize_candidates(&mut candidates)?;
    validate_new_evidence(
        &summaries,
        &candidates,
        &evidence.source_round_receipt.authority,
    )?;

    let previous_context_digest = input
        .previous_context
        .map(|value| value.context.context_digest.clone());
    let previous_ledger_digest = input
        .previous_ledger
        .map(|value| value.ledger().ledger_digest.clone());
    let source_request_digest = derive_debug_probe_delta_source_request_digest(
        &evidence.source_round_receipt,
        &current.ledger_digest,
        previous_context_digest.as_ref(),
    )?;
    let mut context = DebugProbeDeltaContext {
        authority: evidence.source_round_receipt.authority.clone(),
        budget: canonical_debug_delta_context_budget(),
        confirmed_facts,
        context_digest: zero_digest(),
        estimated_token_count: 0,
        hypothesis_changes,
        ledger_digest: current.ledger_digest.clone(),
        new_evidence_candidates: candidates,
        new_evidence_summaries: summaries,
        omitted_snippet_count: i64::try_from(input.snippets.len())
            .map_err(|_| DebugDeltaContextError::ContextTooLarge)?,
        previous_context_digest,
        previous_ledger_digest,
        question_changes,
        reproduction_recipe,
        safety_profile: scanner.profile().clone(),
        schema_version: 1,
        serialized_byte_count: 0,
        snippets: Vec::new(),
        source_event_digest: current.last_event_digest.clone(),
        source_request_digest,
        source_round_receipt: evidence.source_round_receipt.clone(),
    };

    validate_context_text_fields(&context, scanner)?;
    let snippets = canonical_snippets(input.snippets, &context, scanner)?;
    seal_with_selected_snippets(&mut context, &snippets)
}

/// Reopens exact Prepared bytes without rerunning selection, truncation, or
/// the current scanner policy. The stored safety profile remains digest-bound;
/// fresh scanning happened before these exact bytes entered Prepared.
///
/// # Errors
///
/// Rejects non-canonical JSON, a journal digest mismatch, an invalid stored
/// profile, malformed text, collection-order drift, or inconsistent counts.
pub fn reopen_debug_probe_delta_context(
    bytes: &[u8],
    expected_context_digest: &Sha256Digest,
) -> Result<ValidatedDebugProbeDeltaContext, DebugDeltaContextError> {
    if bytes.len() > MAX_SERIALIZED_BYTES || bytes.len() > MAX_ESTIMATED_TOKENS {
        return Err(DebugDeltaContextError::ContextTooLarge);
    }
    let context = serde_json::from_slice::<DebugProbeDeltaContext>(bytes)
        .map_err(|_| DebugDeltaContextError::InvalidPayload)?;
    let canonical = serialize_context(&context)?;
    if canonical != bytes {
        return Err(DebugDeltaContextError::NonCanonicalPayload);
    }
    if &context.context_digest != expected_context_digest {
        return Err(DebugDeltaContextError::DigestMismatch);
    }
    validate_sealed_context(&context, bytes.len())?;
    Ok(ValidatedDebugProbeDeltaContext {
        context,
        canonical_bytes: canonical,
    })
}

/// Verifies that an exact Prepared context names the same durable D3 evidence
/// cut that the Ledger reducer will consume at commit.
///
/// # Errors
///
/// Returns `InvalidInput` when the receipt reference, bounded summaries, or
/// target-bound candidates differ from the sealed durable evidence cut.
pub fn validate_debug_probe_delta_context_evidence(
    prepared: &ValidatedDebugProbeDeltaContext,
    evidence: &ValidatedDebugHypothesisRoundEvidence,
) -> Result<(), DebugDeltaContextError> {
    let context = prepared.context();
    let cut = evidence.cut();
    if context.source_round_receipt != cut.source_round_receipt
        || context.new_evidence_summaries != cut.evidence_summaries
        || context.new_evidence_candidates != cut.evidence_candidates
    {
        return Err(DebugDeltaContextError::InvalidInput);
    }
    Ok(())
}

/// Derives the semantic context digest without trusting its digest or counts.
///
/// # Errors
///
/// Returns `Serialization` if a generated field cannot be encoded as JSON.
pub fn derive_debug_probe_delta_context_digest(
    context: &DebugProbeDeltaContext,
) -> Result<Sha256Digest, DebugDeltaContextError> {
    let mut digest = FramedDigest::new(CONTEXT_HASH_DOMAIN);
    digest.json(b"schemaVersion", &context.schema_version)?;
    digest.json(b"authority", &context.authority)?;
    digest.json(b"sourceRoundReceipt", &context.source_round_receipt)?;
    digest.json(b"previousLedgerDigest", &context.previous_ledger_digest)?;
    digest.json(b"ledgerDigest", &context.ledger_digest)?;
    digest.json(b"sourceEventDigest", &context.source_event_digest)?;
    digest.json(b"sourceRequestDigest", &context.source_request_digest)?;
    digest.json(b"previousContextDigest", &context.previous_context_digest)?;
    digest.json(b"hypothesisChanges", &context.hypothesis_changes)?;
    digest.json(b"newEvidenceSummaries", &context.new_evidence_summaries)?;
    digest.json(b"newEvidenceCandidates", &context.new_evidence_candidates)?;
    digest.json(b"confirmedFacts", &context.confirmed_facts)?;
    digest.json(b"reproductionRecipe", &context.reproduction_recipe)?;
    digest.json(b"questionChanges", &context.question_changes)?;
    digest.json(b"snippets", &context.snippets)?;
    digest.json(b"omittedSnippetCount", &context.omitted_snippet_count)?;
    digest.json(b"budget", &context.budget)?;
    digest.json(b"safetyProfile", &context.safety_profile)?;
    Ok(digest.finish())
}

/// Derives the request idempotency digest before the context digest exists.
///
/// # Errors
///
/// Returns `Serialization` if one generated source field cannot be encoded.
pub fn derive_debug_probe_delta_source_request_digest(
    source_round_receipt: &crate::generated::ProbeRoundReceiptReference,
    current_ledger_digest: &Sha256Digest,
    previous_context_digest: Option<&Sha256Digest>,
) -> Result<Sha256Digest, DebugDeltaContextError> {
    let mut digest = FramedDigest::new(REQUEST_HASH_DOMAIN);
    digest.json(b"schemaVersion", &1_i64)?;
    digest.json(b"sourceRoundReceipt", source_round_receipt)?;
    digest.json(b"currentLedgerDigest", current_ledger_digest)?;
    digest.json(b"previousContextDigest", &previous_context_digest)?;
    Ok(digest.finish())
}

fn validate_cursor(
    previous_ledger: Option<&ValidatedDebugHypothesisLedger>,
    previous_context: Option<&ValidatedDebugProbeDeltaContext>,
    current: &DebugHypothesisLedger,
) -> Result<(), DebugDeltaContextError> {
    validate_digest(&current.ledger_digest)?;
    validate_digest(&current.last_event_digest)?;
    match (previous_ledger, previous_context) {
        (None, None) => {
            if current.event_sequence.0 != 1 || current.latest_applied_round_receipt.is_some() {
                return Err(DebugDeltaContextError::CursorMismatch);
            }
        }
        (Some(previous), Some(context)) => {
            let previous = previous.ledger();
            if context.context.ledger_digest != previous.ledger_digest
                || previous.event_sequence.0 > current.event_sequence.0
            {
                return Err(DebugDeltaContextError::CursorMismatch);
            }
            validate_session_scope(&previous.authority, &current.authority)?;
        }
        _ => return Err(DebugDeltaContextError::CursorMismatch),
    }
    Ok(())
}

fn derive_hypothesis_changes(
    previous: Option<&DebugHypothesisLedger>,
    current: &[DebugHypothesis],
) -> Result<Vec<DebugHypothesisChange>, DebugDeltaContextError> {
    let previous_by_id = previous
        .map(|ledger| {
            ledger
                .hypotheses
                .iter()
                .map(|value| (value.hypothesis_id.0.as_str(), value))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    if previous_by_id
        .keys()
        .any(|id| !current.iter().any(|value| value.hypothesis_id.0 == **id))
    {
        return Err(DebugDeltaContextError::InvalidInput);
    }

    let mut changes = Vec::new();
    for hypothesis in current {
        let prior = previous_by_id
            .get(hypothesis.hypothesis_id.0.as_str())
            .copied();
        let added_supporting = added_evidence(
            &hypothesis.hypothesis_id,
            &hypothesis.supporting_evidence,
            prior.map_or(&[], |value| value.supporting_evidence.as_slice()),
        )?;
        let added_contradicting = added_evidence(
            &hypothesis.hypothesis_id,
            &hypothesis.contradicting_evidence,
            prior.map_or(&[], |value| value.contradicting_evidence.as_slice()),
        )?;
        let has_delta = prior.is_none_or(|value| {
            value.summary != hypothesis.summary
                || value.status != hypothesis.status
                || value.confidence_bps != hypothesis.confidence_bps
                || !added_supporting.is_empty()
                || !added_contradicting.is_empty()
        });
        if has_delta {
            changes.push(DebugHypothesisChange {
                added_contradicting_evidence: added_contradicting,
                added_supporting_evidence: added_supporting,
                confidence_bps: hypothesis.confidence_bps,
                hypothesis_id: hypothesis.hypothesis_id.clone(),
                previous_confidence_bps: prior.map(|value| value.confidence_bps),
                previous_status: prior.map(|value| value.status.clone()),
                status: hypothesis.status.clone(),
                summary: hypothesis.summary.clone(),
            });
        }
    }
    changes.sort_by(|left, right| left.hypothesis_id.0.cmp(&right.hypothesis_id.0));
    Ok(changes)
}

fn added_evidence(
    target: &DebugHypothesisId,
    current: &[DebugHypothesisEvidence],
    previous: &[DebugHypothesisEvidence],
) -> Result<Vec<HypothesisEvidenceCandidate>, DebugDeltaContextError> {
    if previous
        .iter()
        .any(|value| !current.iter().any(|candidate| candidate == value))
    {
        return Err(DebugDeltaContextError::InvalidInput);
    }
    let mut added = current
        .iter()
        .filter(|value| !previous.iter().any(|candidate| candidate == *value))
        .map(|value| HypothesisEvidenceCandidate {
            evidence: value.clone(),
            target_hypothesis_id: target.clone(),
        })
        .collect::<Vec<_>>();
    canonicalize_candidates(&mut added)?;
    Ok(added)
}

fn derive_confirmed_facts(
    previous: Option<&DebugHypothesisLedger>,
    current: &[DebugConfirmedFact],
) -> Result<Vec<DebugConfirmedFact>, DebugDeltaContextError> {
    let prior = previous.map_or(&[][..], |ledger| ledger.confirmed_facts.as_slice());
    if prior.iter().any(|value| {
        !current
            .iter()
            .any(|candidate| candidate.fact_digest == value.fact_digest && candidate == value)
    }) {
        return Err(DebugDeltaContextError::InvalidInput);
    }
    let mut added = current
        .iter()
        .filter(|value| {
            !prior
                .iter()
                .any(|candidate| candidate.fact_digest == value.fact_digest)
        })
        .cloned()
        .collect::<Vec<_>>();
    added.sort_by(|left, right| left.fact_digest.0.cmp(&right.fact_digest.0));
    Ok(added)
}

fn derive_reproduction_recipe(
    previous: Option<&DebugHypothesisLedger>,
    current: Option<&DebugReproductionRecipe>,
) -> Option<DebugReproductionRecipe> {
    let prior = previous.and_then(|ledger| ledger.reproduction_recipe.as_ref());
    current
        .filter(|value| prior.is_none_or(|old| old.recipe_digest != value.recipe_digest))
        .cloned()
}

fn derive_question_changes(
    previous: Option<&DebugHypothesisLedger>,
    current: &[DebugUnresolvedQuestion],
) -> Result<Vec<DebugQuestionChange>, DebugDeltaContextError> {
    let prior = previous.map_or(&[][..], |ledger| ledger.unresolved_questions.as_slice());
    let mut changes = current
        .iter()
        .filter(|value| {
            !prior
                .iter()
                .any(|old| old.question_digest == value.question_digest)
        })
        .map(|value| DebugQuestionChange {
            kind: DebugQuestionChangeKind::Opened,
            question_digest: value.question_digest.clone(),
            summary: value.summary.clone(),
        })
        .collect::<Vec<_>>();
    for value in prior.iter().filter(|value| {
        !current
            .iter()
            .any(|next| next.question_digest == value.question_digest)
    }) {
        changes.push(DebugQuestionChange {
            kind: DebugQuestionChangeKind::Resolved,
            question_digest: value.question_digest.clone(),
            summary: value.summary.clone(),
        });
    }
    changes.sort_by(|left, right| {
        (&left.question_digest.0, question_kind_rank(&left.kind))
            .cmp(&(&right.question_digest.0, question_kind_rank(&right.kind)))
    });
    if changes.windows(2).any(|pair| {
        pair[0].question_digest == pair[1].question_digest && pair[0].kind == pair[1].kind
    }) {
        return Err(DebugDeltaContextError::InvalidInput);
    }
    Ok(changes)
}

const fn question_kind_rank(kind: &DebugQuestionChangeKind) -> u8 {
    match kind {
        DebugQuestionChangeKind::Opened => 0,
        DebugQuestionChangeKind::Resolved => 1,
    }
}

fn canonicalize_summaries(values: &mut [ProbeEvidenceSummary]) {
    values.sort_by(|left, right| {
        left.identity
            .probe_execution_id
            .0
            .cmp(&right.identity.probe_execution_id.0)
    });
}

fn canonicalize_candidates(
    values: &mut [HypothesisEvidenceCandidate],
) -> Result<(), DebugDeltaContextError> {
    values.sort_by(|left, right| candidate_key(left).cmp(&candidate_key(right)));
    if values
        .windows(2)
        .any(|pair| candidate_key(&pair[0]) == candidate_key(&pair[1]))
    {
        return Err(DebugDeltaContextError::InvalidInput);
    }
    Ok(())
}

fn candidate_key(value: &HypothesisEvidenceCandidate) -> (&str, &str, &str) {
    (
        &value.target_hypothesis_id.0,
        &value.evidence.evidence_digest.0,
        &value.evidence.artifact_ref.artifact_id.0,
    )
}

fn validate_new_evidence(
    summaries: &[ProbeEvidenceSummary],
    candidates: &[HypothesisEvidenceCandidate],
    authority: &DebugProbeRoundAuthority,
) -> Result<(), DebugDeltaContextError> {
    if summaries
        .windows(2)
        .any(|pair| pair[0].identity.probe_execution_id.0 >= pair[1].identity.probe_execution_id.0)
        || candidates
            .windows(2)
            .any(|pair| candidate_key(&pair[0]) >= candidate_key(&pair[1]))
    {
        return Err(DebugDeltaContextError::InvalidInput);
    }
    for summary in summaries {
        validate_probe_identity_authority(summary, authority)?;
        validate_artifact_reference(&summary.bundle_artifact_ref)?;
        validate_digest(&summary.bundle_digest)?;
    }
    for candidate in candidates {
        validate_artifact_reference(&candidate.evidence.artifact_ref)?;
        validate_digest(&candidate.evidence.evidence_digest)?;
        let summary = summaries
            .iter()
            .find(|summary| {
                summary.identity == candidate.evidence.identity
                    && summary.bundle_artifact_ref == candidate.evidence.artifact_ref
            })
            .ok_or(DebugDeltaContextError::InvalidInput)?;
        if candidate.evidence.recorded_at != summary.recorded_at
            || candidate.evidence.evidence_digest
                != derive_hypothesis_evidence_digest(
                    &summary.identity,
                    &candidate.target_hypothesis_id,
                    &summary.bundle_digest,
                )
                .map_err(|_| DebugDeltaContextError::InvalidDigest)?
        {
            return Err(DebugDeltaContextError::InvalidInput);
        }
    }
    Ok(())
}

fn validate_probe_identity_authority(
    summary: &ProbeEvidenceSummary,
    authority: &DebugProbeRoundAuthority,
) -> Result<(), DebugDeltaContextError> {
    let identity = &summary.identity;
    if identity.debug_session_id != authority.debug_session_id
        || identity.job_id != authority.job_id
        || identity.attempt != authority.attempt
        || identity.lease_id != authority.lease_id
        || identity.fencing_token != authority.fencing_token
        || identity.session_identity != authority.session_identity
        || identity.repository_id != authority.repository_id
        || identity.round_id != authority.round_id
        || identity.workspace_revision != authority.workspace_revision
        || identity.environment_digest != authority.environment_digest
    {
        return Err(DebugDeltaContextError::AuthorityMismatch);
    }
    Ok(())
}

fn canonical_snippets(
    values: &[ValidatedDebugContextSnippet],
    context: &DebugProbeDeltaContext,
    scanner: &impl DebugContextSafetyScanner,
) -> Result<Vec<DebugContextSnippet>, DebugDeltaContextError> {
    let mut snippets = values
        .iter()
        .map(|value| value.snippet.clone())
        .collect::<Vec<_>>();
    for snippet in &snippets {
        validate_generated_snippet(snippet, context)?;
        validate_context_text(&snippet.content, MAX_SNIPPET_BYTES, scanner)?;
        if let Some(path) = snippet.path.as_deref() {
            validate_context_text(path, 4_096, scanner)?;
        }
    }
    snippets.sort_by(|left, right| snippet_key(left).cmp(&snippet_key(right)));
    if snippets
        .windows(2)
        .any(|pair| snippet_key(&pair[0]) == snippet_key(&pair[1]))
    {
        return Err(DebugDeltaContextError::InvalidInput);
    }
    Ok(snippets)
}

fn snippet_key(value: &DebugContextSnippet) -> (&str, &str, &str, i64, i64, &str) {
    (
        &value.source_evidence_digest.0,
        &value.source_artifact_ref.artifact_id.0,
        value.path.as_deref().unwrap_or(""),
        value.start_line.unwrap_or(0),
        value.end_line.unwrap_or(0),
        &value.content_digest.0,
    )
}

fn validate_generated_snippet(
    snippet: &DebugContextSnippet,
    context: &DebugProbeDeltaContext,
) -> Result<(), DebugDeltaContextError> {
    if snippet.safety_profile != context.safety_profile {
        return Err(DebugDeltaContextError::InvalidSafetyProfile);
    }
    validate_artifact_reference(&snippet.source_artifact_ref)?;
    validate_digest(&snippet.source_evidence_digest)?;
    validate_digest(&snippet.content_digest)?;
    validate_snippet_location(
        snippet.path.as_deref(),
        snippet.start_line,
        snippet.end_line,
    )?;
    validate_context_text_shape(&snippet.content, MAX_SNIPPET_BYTES)?;
    if let Some(path) = snippet.path.as_deref() {
        validate_portable_path(path)?;
        validate_context_text_shape(path, 4_096)?;
    }
    if snippet.content_digest
        != Sha256Digest(format!(
            "sha256:{:x}",
            Sha256::digest(snippet.content.as_bytes())
        ))
    {
        return Err(DebugDeltaContextError::DigestMismatch);
    }
    if !context.new_evidence_candidates.iter().any(|candidate| {
        candidate.evidence.artifact_ref == snippet.source_artifact_ref
            && candidate.evidence.evidence_digest == snippet.source_evidence_digest
    }) {
        return Err(DebugDeltaContextError::SnippetSourceMismatch);
    }
    Ok(())
}

fn seal_with_selected_snippets(
    context: &mut DebugProbeDeltaContext,
    candidates: &[DebugContextSnippet],
) -> Result<ValidatedDebugProbeDeltaContext, DebugDeltaContextError> {
    let mut prepared = seal_context(context.clone())?;
    let mut selected = Vec::new();
    let mut selected_bytes = 0_usize;
    for candidate in candidates {
        if selected.len() >= MAX_SNIPPETS
            || selected_bytes.saturating_add(candidate.content.len()) > MAX_TOTAL_SNIPPET_BYTES
        {
            continue;
        }
        let mut trial_selected = selected.clone();
        trial_selected.push(candidate.clone());
        let mut trial = context.clone();
        trial.snippets.clone_from(&trial_selected);
        trial.omitted_snippet_count = i64::try_from(candidates.len() - trial.snippets.len())
            .map_err(|_| DebugDeltaContextError::ContextTooLarge)?;
        match seal_context(trial) {
            Ok(value) => {
                selected = trial_selected;
                selected_bytes += candidate.content.len();
                prepared = value;
            }
            Err(DebugDeltaContextError::ContextTooLarge) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(prepared)
}

fn seal_context(
    mut context: DebugProbeDeltaContext,
) -> Result<ValidatedDebugProbeDeltaContext, DebugDeltaContextError> {
    context.context_digest = derive_debug_probe_delta_context_digest(&context)?;
    context.serialized_byte_count = 0;
    context.estimated_token_count = 0;
    for _ in 0..5 {
        let bytes = serialize_context(&context)?;
        if bytes.len() > MAX_SERIALIZED_BYTES || bytes.len() > MAX_ESTIMATED_TOKENS {
            return Err(DebugDeltaContextError::ContextTooLarge);
        }
        let count =
            i64::try_from(bytes.len()).map_err(|_| DebugDeltaContextError::Serialization)?;
        if context.serialized_byte_count == count && context.estimated_token_count == count {
            return Ok(ValidatedDebugProbeDeltaContext {
                context,
                canonical_bytes: bytes,
            });
        }
        context.serialized_byte_count = count;
        context.estimated_token_count = count;
    }
    Err(DebugDeltaContextError::Serialization)
}

fn serialize_context(context: &DebugProbeDeltaContext) -> Result<Vec<u8>, DebugDeltaContextError> {
    serde_json::to_vec(context).map_err(|_| DebugDeltaContextError::Serialization)
}

fn validate_sealed_context(
    context: &DebugProbeDeltaContext,
    exact_byte_count: usize,
) -> Result<(), DebugDeltaContextError> {
    if context.schema_version != 1
        || context.budget != canonical_debug_delta_context_budget()
        || context.source_round_receipt.authority != context.authority
    {
        return Err(DebugDeltaContextError::InvalidInput);
    }
    validate_safety_profile(&context.safety_profile)?;
    let count =
        i64::try_from(exact_byte_count).map_err(|_| DebugDeltaContextError::CountMismatch)?;
    if context.serialized_byte_count != count || context.estimated_token_count != count {
        return Err(DebugDeltaContextError::CountMismatch);
    }
    if context.context_digest != derive_debug_probe_delta_context_digest(context)? {
        return Err(DebugDeltaContextError::DigestMismatch);
    }
    if context.source_request_digest
        != derive_debug_probe_delta_source_request_digest(
            &context.source_round_receipt,
            &context.ledger_digest,
            context.previous_context_digest.as_ref(),
        )?
    {
        return Err(DebugDeltaContextError::DigestMismatch);
    }
    if context.previous_context_digest.is_none() != context.previous_ledger_digest.is_none()
        || context.omitted_snippet_count < 0
        || context.snippets.len() > MAX_SNIPPETS
        || context
            .snippets
            .iter()
            .map(|value| value.content.len())
            .sum::<usize>()
            > MAX_TOTAL_SNIPPET_BYTES
    {
        return Err(DebugDeltaContextError::InvalidInput);
    }
    for digest in [
        &context.ledger_digest,
        &context.source_event_digest,
        &context.source_request_digest,
        &context.context_digest,
        &context.source_round_receipt.plan_digest,
        &context.source_round_receipt.receipt_digest,
    ] {
        validate_digest(digest)?;
    }
    if let Some(digest) = &context.previous_ledger_digest {
        validate_digest(digest)?;
    }
    if let Some(digest) = &context.previous_context_digest {
        validate_digest(digest)?;
    }
    validate_artifact_reference(&context.source_round_receipt.receipt_artifact_ref)?;
    validate_context_collections(context)?;
    validate_context_text_field_shapes(context)?;
    validate_new_evidence(
        &context.new_evidence_summaries,
        &context.new_evidence_candidates,
        &context.authority,
    )?;
    for snippet in &context.snippets {
        validate_generated_snippet(snippet, context)?;
    }
    if context
        .snippets
        .windows(2)
        .any(|pair| snippet_key(&pair[0]) >= snippet_key(&pair[1]))
    {
        return Err(DebugDeltaContextError::InvalidInput);
    }
    Ok(())
}

fn validate_context_collections(
    context: &DebugProbeDeltaContext,
) -> Result<(), DebugDeltaContextError> {
    if context
        .hypothesis_changes
        .windows(2)
        .any(|pair| pair[0].hypothesis_id.0 >= pair[1].hypothesis_id.0)
        || context
            .confirmed_facts
            .windows(2)
            .any(|pair| pair[0].fact_digest.0 >= pair[1].fact_digest.0)
        || context.question_changes.windows(2).any(|pair| {
            (
                &pair[0].question_digest.0,
                question_kind_rank(&pair[0].kind),
            ) >= (
                &pair[1].question_digest.0,
                question_kind_rank(&pair[1].kind),
            )
        })
    {
        return Err(DebugDeltaContextError::InvalidInput);
    }
    for change in &context.hypothesis_changes {
        validate_digest_candidates(&change.added_supporting_evidence)?;
        validate_digest_candidates(&change.added_contradicting_evidence)?;
    }
    for fact in &context.confirmed_facts {
        validate_digest(&fact.fact_digest)?;
        validate_digest_candidates(&fact.evidence)?;
    }
    if let Some(recipe) = &context.reproduction_recipe {
        validate_digest(&recipe.recipe_digest)?;
        validate_digest_candidates(&recipe.evidence)?;
        for step in &recipe.steps {
            validate_digest(&step.step_digest)?;
            if let Some(digest) = &step.probe_definition_digest {
                validate_digest(digest)?;
            }
        }
    }
    for question in &context.question_changes {
        validate_digest(&question.question_digest)?;
    }
    Ok(())
}

fn validate_digest_candidates(
    candidates: &[HypothesisEvidenceCandidate],
) -> Result<(), DebugDeltaContextError> {
    let mut keys = BTreeSet::new();
    for candidate in candidates {
        validate_digest(&candidate.evidence.evidence_digest)?;
        validate_artifact_reference(&candidate.evidence.artifact_ref)?;
        if !keys.insert(candidate_key(candidate)) {
            return Err(DebugDeltaContextError::InvalidInput);
        }
    }
    Ok(())
}

fn validate_context_text_fields(
    context: &DebugProbeDeltaContext,
    scanner: &impl DebugContextSafetyScanner,
) -> Result<(), DebugDeltaContextError> {
    validate_context_text_field_shapes(context)?;
    for change in &context.hypothesis_changes {
        scanner
            .validate(&change.summary)
            .map_err(DebugDeltaContextError::UnsafeText)?;
    }
    for summary in &context.new_evidence_summaries {
        scanner
            .validate(&summary.summary)
            .map_err(DebugDeltaContextError::UnsafeText)?;
    }
    for fact in &context.confirmed_facts {
        scanner
            .validate(&fact.summary)
            .map_err(DebugDeltaContextError::UnsafeText)?;
    }
    if let Some(recipe) = &context.reproduction_recipe {
        for step in &recipe.steps {
            scanner
                .validate(&step.summary)
                .map_err(DebugDeltaContextError::UnsafeText)?;
        }
    }
    for question in &context.question_changes {
        scanner
            .validate(&question.summary)
            .map_err(DebugDeltaContextError::UnsafeText)?;
    }
    Ok(())
}

fn validate_context_text_field_shapes(
    context: &DebugProbeDeltaContext,
) -> Result<(), DebugDeltaContextError> {
    for change in &context.hypothesis_changes {
        validate_context_text_shape(&change.summary, 500)?;
    }
    for summary in &context.new_evidence_summaries {
        validate_context_text_shape(&summary.summary, 500)?;
    }
    for fact in &context.confirmed_facts {
        validate_context_text_shape(&fact.summary, 500)?;
    }
    if let Some(recipe) = &context.reproduction_recipe {
        for step in &recipe.steps {
            validate_context_text_shape(&step.summary, 500)?;
        }
    }
    for question in &context.question_changes {
        validate_context_text_shape(&question.summary, 500)?;
    }
    Ok(())
}

fn validate_session_scope(
    previous: &DebugProbeRoundAuthority,
    current: &DebugProbeRoundAuthority,
) -> Result<(), DebugDeltaContextError> {
    if previous.debug_session_id != current.debug_session_id
        || previous.session_identity != current.session_identity
        || previous.repository_id != current.repository_id
        || previous.workspace_revision != current.workspace_revision
        || previous.environment_digest != current.environment_digest
    {
        return Err(DebugDeltaContextError::AuthorityMismatch);
    }
    Ok(())
}

fn validate_artifact_reference(
    reference: &ArtifactReference,
) -> Result<(), DebugDeltaContextError> {
    if reference.artifact_id.0.len() != 30
        || !reference.artifact_id.0.starts_with("art_")
        || !reference.artifact_id.0[4..]
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'A'..=b'H' | b'J'..=b'N' | b'P'..=b'T' | b'V'..=b'Z'))
    {
        return Err(DebugDeltaContextError::InvalidInput);
    }
    validate_digest(&reference.digest)
}

fn validate_portable_path(path: &str) -> Result<(), DebugDeltaContextError> {
    if path.starts_with('/')
        || path.contains('\\')
        || path.contains(':')
        || path
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(DebugDeltaContextError::InvalidSnippetLocation);
    }
    Ok(())
}

struct FramedDigest(Sha256);

impl FramedDigest {
    fn new(domain: &[u8]) -> Self {
        let mut digest = Sha256::new();
        digest.update(domain);
        Self(digest)
    }

    fn json<T: Serialize + ?Sized>(
        &mut self,
        name: &[u8],
        value: &T,
    ) -> Result<(), DebugDeltaContextError> {
        let bytes = serde_json::to_vec(value).map_err(|_| DebugDeltaContextError::Serialization)?;
        update_bytes(&mut self.0, name, &bytes);
        Ok(())
    }

    fn finish(self) -> Sha256Digest {
        Sha256Digest(format!("sha256:{:x}", self.0.finalize()))
    }
}

fn zero_digest() -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", "0".repeat(64)))
}

/// Returns the single host-owned Delta Context budget.
#[must_use]
pub fn canonical_debug_delta_context_budget() -> DebugDeltaContextBudget {
    let mut budget = DebugDeltaContextBudget {
        estimator_version: DebugDeltaContextEstimatorVersion::Utf8BytesV1,
        max_estimated_tokens: 8_192,
        max_serialized_bytes: 32_768,
        max_snippet_bytes: 2_000,
        max_snippets: 8,
        max_total_snippet_bytes: 16_000,
        policy_digest: Sha256Digest(String::new()),
    };
    budget.policy_digest = derive_debug_delta_context_budget_digest(&budget);
    budget
}

/// Derives the immutable budget-policy digest without trusting its digest field.
#[must_use]
pub fn derive_debug_delta_context_budget_digest(budget: &DebugDeltaContextBudget) -> Sha256Digest {
    let mut digest = Sha256::new();
    digest.update(BUDGET_HASH_DOMAIN);
    update_i64(
        &mut digest,
        b"maxSerializedBytes",
        budget.max_serialized_bytes,
    );
    update_i64(
        &mut digest,
        b"maxEstimatedTokens",
        budget.max_estimated_tokens,
    );
    update_i64(&mut digest, b"maxSnippets", budget.max_snippets);
    update_i64(&mut digest, b"maxSnippetBytes", budget.max_snippet_bytes);
    update_i64(
        &mut digest,
        b"maxTotalSnippetBytes",
        budget.max_total_snippet_bytes,
    );
    update_bytes(&mut digest, b"estimatorVersion", b"utf8_bytes_v1");
    Sha256Digest(format!("sha256:{:x}", digest.finalize()))
}

/// Stable failures returned by Delta Context sealing and exact reopen.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DebugDeltaContextError {
    /// A trusted input does not form one valid canonical context.
    InvalidInput,
    /// A required digest is not canonical lowercase SHA-256.
    InvalidDigest,
    /// The selected scanner profile is not a canonical host profile.
    InvalidSafetyProfile,
    /// A context string is empty or is not one bounded canonical line.
    InvalidText,
    /// A complete item exceeds its fixed UTF-8 byte bound.
    TextTooLarge,
    /// The host scanner rejected content without retaining it.
    UnsafeText(ContextSafetyScanError),
    /// Snippet path and line fields do not form one coherent source location.
    InvalidSnippetLocation,
    /// A snippet is not bound to an exact current D3 evidence candidate.
    SnippetSourceMismatch,
    /// The acknowledged context and Ledger cursor are absent or paired incorrectly.
    CursorMismatch,
    /// Current evidence or a cursor belongs to another debug session snapshot.
    AuthorityMismatch,
    /// Required structured context exceeds the conservative token upper bound.
    ContextTooLarge,
    /// JSON encoding or a derived size cannot be represented.
    Serialization,
    /// Exact Prepared bytes could not be decoded as the generated contract.
    InvalidPayload,
    /// Exact Prepared bytes are not the canonical generated encoding.
    NonCanonicalPayload,
    /// A persisted or recomputed semantic digest differs.
    DigestMismatch,
    /// Persisted byte and conservative token counts differ from exact bytes.
    CountMismatch,
}

impl fmt::Display for DebugDeltaContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidInput => "delta context input is invalid",
            Self::InvalidDigest => "delta context contains an invalid digest",
            Self::InvalidSafetyProfile => "delta context has an invalid safety profile",
            Self::InvalidText => "delta context text is invalid",
            Self::TextTooLarge => "delta context text exceeds its fixed bound",
            Self::UnsafeText(_) => "delta context text did not pass the host scanner",
            Self::InvalidSnippetLocation => "delta context snippet location is invalid",
            Self::SnippetSourceMismatch => {
                "delta context snippet is outside current exact evidence"
            }
            Self::CursorMismatch => "delta context cursor does not match its Ledger snapshot",
            Self::AuthorityMismatch => "delta context authority does not match its sources",
            Self::ContextTooLarge => "delta context exceeds its fixed token bound",
            Self::Serialization => "delta context serialization failed",
            Self::InvalidPayload => "delta context payload is invalid",
            Self::NonCanonicalPayload => "delta context payload is not canonical",
            Self::DigestMismatch => "delta context digest does not match",
            Self::CountMismatch => "delta context size count does not match exact bytes",
        })
    }
}

impl std::error::Error for DebugDeltaContextError {}

fn validate_safety_profile(
    profile: &DebugContextSafetyProfile,
) -> Result<(), DebugDeltaContextError> {
    if profile.scanner_version != DebugContextSafetyScannerVersion::WorkspaceSecretScanV1 {
        return Err(DebugDeltaContextError::InvalidSafetyProfile);
    }
    validate_digest(&profile.scanner_policy_digest)
        .map_err(|_| DebugDeltaContextError::InvalidSafetyProfile)
}

fn validate_context_text(
    text: &str,
    maximum_bytes: usize,
    scanner: &impl DebugContextSafetyScanner,
) -> Result<(), DebugDeltaContextError> {
    validate_context_text_shape(text, maximum_bytes)?;
    scanner
        .validate(text)
        .map_err(DebugDeltaContextError::UnsafeText)
}

fn validate_context_text_shape(
    text: &str,
    maximum_bytes: usize,
) -> Result<(), DebugDeltaContextError> {
    if text.trim().is_empty() || text.chars().any(char::is_control) {
        return Err(DebugDeltaContextError::InvalidText);
    }
    if text.len() > maximum_bytes {
        return Err(DebugDeltaContextError::TextTooLarge);
    }
    Ok(())
}

fn validate_snippet_location(
    path: Option<&str>,
    start_line: Option<i64>,
    end_line: Option<i64>,
) -> Result<(), DebugDeltaContextError> {
    match (path, start_line, end_line) {
        (None, None, None) => Ok(()),
        (Some(path), None, None) if !path.is_empty() => Ok(()),
        (Some(path), Some(start), Some(end))
            if !path.is_empty() && start > 0 && start <= end && end <= i64::from(i32::MAX) =>
        {
            Ok(())
        }
        _ => Err(DebugDeltaContextError::InvalidSnippetLocation),
    }
}

fn validate_digest(digest: &Sha256Digest) -> Result<(), DebugDeltaContextError> {
    let Some(value) = digest.0.strip_prefix("sha256:") else {
        return Err(DebugDeltaContextError::InvalidDigest);
    };
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(DebugDeltaContextError::InvalidDigest);
    }
    Ok(())
}

fn update_i64(digest: &mut Sha256, name: &[u8], value: i64) {
    update_bytes(digest, name, &value.to_be_bytes());
}

fn update_bytes(digest: &mut Sha256, name: &[u8], value: &[u8]) {
    digest.update((name.len() as u64).to_be_bytes());
    digest.update(name);
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}
