// SPDX-License-Identifier: Apache-2.0

//! Pure, replayable reducer for the canonical `DebugProbe` hypothesis Ledger.
//!
//! D3 evidence projections stay neutral. The model explicitly supplies an
//! evidence polarity and proposed state/confidence, while this module owns all
//! authority, provenance, transition, monotonicity, digest and replay checks.
//! Stale evidence is rejected before an event is built; callers may retain it
//! in a separate audit journal, but it never advances this Ledger chain.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::Serialize;
use sha2::{Digest as _, Sha256};
use winwincode_domain::{DebugHypothesisId, ExecutionSequence, Sha256Digest};

use crate::debug_probe_contract::{
    ValidatedDebugProbePlan, validate_debug_probe_round_authority, validate_probe_round_receipt,
};
use crate::generated::{
    ArtifactReference, DebugConfirmedFact, DebugHypothesis, DebugHypothesisEvidence,
    DebugHypothesisEvidenceAssessment, DebugHypothesisEvidencePolarity, DebugHypothesisLedger,
    DebugHypothesisLedgerEvent, DebugHypothesisLedgerEventKind, DebugHypothesisLedgerSeed,
    DebugHypothesisLedgerUpdate, DebugHypothesisMutation, DebugHypothesisMutationKind,
    DebugHypothesisRoundEvidence, DebugHypothesisStatus, DebugProbeRoundAuthority,
    DebugReproductionRecipe, DebugReproductionRecipeStep, DebugReproductionRecipeUpdate,
    DebugSessionStatus, DebugUnresolvedQuestion, HypothesisEvidenceCandidate,
    ProbeEvidenceCompletenessStatus, ProbeEvidenceSummary, ProbeRoundReceipt,
    ProbeRoundReceiptReference, ProbeRoundReceiptStatus,
};
use crate::probe_result_normalizer::{ProbeEvidenceProjection, derive_hypothesis_evidence_digest};

const ROUND_RECEIPT_DIGEST_DOMAIN: &[u8] = b"winwincode.debug-probe.round-receipt.v1\0";
const ROUND_EVIDENCE_DIGEST_DOMAIN: &[u8] =
    b"winwincode.debug-probe.hypothesis-round-evidence.v1\0";
const LEDGER_DIGEST_DOMAIN: &[u8] = b"winwincode.debug-probe.hypothesis-ledger.v1\0";
const LEDGER_EVENT_DIGEST_DOMAIN: &[u8] = b"winwincode.debug-probe.hypothesis-ledger-event.v1\0";
const FACT_DIGEST_DOMAIN: &[u8] = b"winwincode.debug-probe.confirmed-fact.v1\0";
const RECIPE_STEP_DIGEST_DOMAIN: &[u8] = b"winwincode.debug-probe.reproduction-step.v1\0";
const RECIPE_DIGEST_DOMAIN: &[u8] = b"winwincode.debug-probe.reproduction-recipe.v1\0";
const QUESTION_DIGEST_DOMAIN: &[u8] = b"winwincode.debug-probe.question.v1\0";

/// Stable failure categories exposed without echoing model or Artifact text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DebugHypothesisLedgerErrorKind {
    InvalidInput,
    InvalidDigest,
    InvalidSeed,
    InvalidTransition,
    MissingCurrentEvidence,
    EvidenceConflict,
    StaleAuthority,
    ReplayConflict,
    SequenceGap,
    Serialization,
}

/// One bounded, secret-safe Ledger failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DebugHypothesisLedgerError {
    kind: DebugHypothesisLedgerErrorKind,
    message: &'static str,
}

impl DebugHypothesisLedgerError {
    /// Returns the stable failure category.
    #[must_use]
    pub const fn kind(&self) -> DebugHypothesisLedgerErrorKind {
        self.kind
    }

    /// Returns a bounded message that never includes model or Artifact text.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        self.message
    }
}

impl fmt::Display for DebugHypothesisLedgerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for DebugHypothesisLedgerError {}

/// A terminal round receipt and its exact D3 projections, sealed by the host.
#[derive(Clone, Debug)]
pub struct ValidatedDebugHypothesisRoundEvidence {
    cut: DebugHypothesisRoundEvidence,
}

impl ValidatedDebugHypothesisRoundEvidence {
    /// Returns the validated terminal round receipt.
    #[must_use]
    pub const fn receipt(&self) -> &ProbeRoundReceipt {
        &self.cut.receipt
    }

    /// Returns the authority-bound durable receipt reference.
    #[must_use]
    pub const fn receipt_reference(&self) -> &ProbeRoundReceiptReference {
        &self.cut.source_round_receipt
    }

    /// Returns the canonical D3 evidence cut retained for replay.
    #[must_use]
    pub const fn cut(&self) -> &DebugHypothesisRoundEvidence {
        &self.cut
    }

    fn candidate_completeness(
        &self,
        expected: &HypothesisEvidenceCandidate,
    ) -> Option<&ProbeEvidenceCompletenessStatus> {
        self.cut.evidence_summaries.iter().find_map(|summary| {
            (summary.identity == expected.evidence.identity
                && summary.bundle_artifact_ref == expected.evidence.artifact_ref)
                .then_some(&summary.completeness.status)
        })
    }

    fn is_complete_round(&self) -> bool {
        self.cut.receipt.status == ProbeRoundReceiptStatus::Completed
            && self.cut.evidence_summaries.len()
                == expected_evidence_identity_set(&self.cut.receipt).len()
            && self.cut.evidence_summaries.iter().all(|summary| {
                summary.completeness.status == ProbeEvidenceCompletenessStatus::Complete
            })
    }
}

/// A Ledger projection that can only be created by initialization or replay.
#[derive(Clone, Debug)]
pub struct ValidatedDebugHypothesisLedger {
    ledger: DebugHypothesisLedger,
}

impl ValidatedDebugHypothesisLedger {
    /// Returns the current canonical Ledger projection.
    #[must_use]
    pub const fn ledger(&self) -> &DebugHypothesisLedger {
        &self.ledger
    }
}

/// One applied, hash-chained Ledger event.
#[derive(Clone, Debug)]
pub struct ValidatedDebugHypothesisLedgerEvent {
    event: DebugHypothesisLedgerEvent,
}

impl ValidatedDebugHypothesisLedgerEvent {
    /// Returns the canonical hash-chained event.
    #[must_use]
    pub const fn event(&self) -> &DebugHypothesisLedgerEvent {
        &self.event
    }
}

/// Result of a current-round update. A duplicate returns the original event
/// and leaves all reducer state unchanged.
#[derive(Clone, Debug)]
pub enum DebugHypothesisLedgerApplyOutcome {
    Applied(ValidatedDebugHypothesisLedgerEvent),
    Duplicate(ValidatedDebugHypothesisLedgerEvent),
}

impl DebugHypothesisLedgerApplyOutcome {
    /// Returns the original or newly applied canonical event.
    #[must_use]
    pub const fn event(&self) -> &ValidatedDebugHypothesisLedgerEvent {
        match self {
            Self::Applied(event) | Self::Duplicate(event) => event,
        }
    }

    /// Reports whether the exact receipt and request were already applied.
    #[must_use]
    pub const fn is_duplicate(&self) -> bool {
        matches!(self, Self::Duplicate(_))
    }
}

/// Stateful pure reducer. Persistence remains caller-owned; recovery replays
/// the caller's canonical event bytes through [`Self::replay`].
#[derive(Clone, Debug)]
pub struct DebugHypothesisLedgerReducer {
    ledger: ValidatedDebugHypothesisLedger,
    events: Vec<ValidatedDebugHypothesisLedgerEvent>,
    receipts: BTreeMap<String, usize>,
    requests: BTreeMap<String, usize>,
}

impl DebugHypothesisLedgerReducer {
    /// Creates the only evidence-free event allowed by the contract.
    ///
    /// Every seed is forced to Active/0 with empty evidence. Confirmed or
    /// rejected state and non-zero confidence therefore first become possible
    /// only through an exact current-round update.
    ///
    /// # Errors
    ///
    /// Returns a bounded error when the seed authority, identity, digest,
    /// ordering, bounds, or evidence-free state is not canonical.
    pub fn initialize(
        mut seed: DebugHypothesisLedgerSeed,
    ) -> Result<Self, DebugHypothesisLedgerError> {
        validate_authority(&seed.authority)?;
        if seed.hypotheses.is_empty() || seed.hypotheses.len() > 64 {
            return Err(invalid_seed("Ledger seed hypothesis count is invalid"));
        }
        if seed.unresolved_questions.len() > 64 {
            return Err(invalid_seed("Ledger seed question count is invalid"));
        }
        canonicalize_hypotheses(&mut seed.hypotheses);
        canonicalize_questions(&mut seed.unresolved_questions);
        ensure_unique_by(
            seed.hypotheses.iter().map(|value| &value.hypothesis_id.0),
            "Ledger seed hypothesis identity is duplicated",
        )?;
        ensure_unique_by(
            seed.unresolved_questions
                .iter()
                .map(|value| &value.question_digest.0),
            "Ledger seed question identity is duplicated",
        )?;
        for hypothesis in &seed.hypotheses {
            validate_seed_hypothesis(hypothesis, &seed.authority)?;
        }
        for question in &seed.unresolved_questions {
            validate_question(question, &seed.authority.round_id)?;
        }

        let mutations = seed
            .hypotheses
            .iter()
            .map(|hypothesis| DebugHypothesisMutation {
                assessments: Vec::new(),
                confidence_bps: hypothesis.confidence_bps,
                hypothesis_id: hypothesis.hypothesis_id.clone(),
                kind: DebugHypothesisMutationKind::Create,
                previous_confidence_bps: None,
                previous_status: None,
                status: hypothesis.status.clone(),
                summary: hypothesis.summary.clone(),
            })
            .collect::<Vec<_>>();
        let mut ledger = DebugHypothesisLedger {
            authority: seed.authority.clone(),
            confirmed_facts: Vec::new(),
            event_sequence: ExecutionSequence(1),
            hypotheses: seed.hypotheses,
            last_event_digest: zero_digest(),
            latest_applied_round_receipt: None,
            ledger_digest: zero_digest(),
            reproduction_recipe: None,
            schema_version: 1,
            session_status: DebugSessionStatus::Active,
            unresolved_questions: seed.unresolved_questions.clone(),
            updated_at: seed.created_at.clone(),
        };
        ledger.ledger_digest = derive_debug_hypothesis_ledger_digest(&ledger)?;
        let mut event = DebugHypothesisLedgerEvent {
            authority: seed.authority,
            confirmed_facts: Vec::new(),
            event_digest: zero_digest(),
            kind: DebugHypothesisLedgerEventKind::Initialized,
            mutations,
            occurred_at: seed.created_at,
            opened_questions: seed.unresolved_questions,
            previous_event_digest: None,
            previous_ledger_digest: None,
            reproduction_recipe_update: None,
            resolved_question_digests: Vec::new(),
            resulting_ledger_digest: ledger.ledger_digest.clone(),
            schema_version: 1,
            sequence: ExecutionSequence(1),
            session_status: DebugSessionStatus::Active,
            source_context_digest: None,
            source_request_digest: None,
            source_round_receipt: None,
        };
        event.event_digest = derive_debug_hypothesis_ledger_event_digest(&event)?;
        ledger.last_event_digest = event.event_digest.clone();
        validate_ledger_projection(&ledger)?;

        Ok(Self {
            ledger: ValidatedDebugHypothesisLedger { ledger },
            events: vec![ValidatedDebugHypothesisLedgerEvent { event }],
            receipts: BTreeMap::new(),
            requests: BTreeMap::new(),
        })
    }

    /// Rebuilds a reducer from canonical applied facts. Stale audit records are
    /// not accepted by this API because they are outside the current chain.
    ///
    /// # Errors
    ///
    /// Returns a bounded error when an event, evidence cut, sequence, digest,
    /// or replayed projection is missing or non-canonical.
    pub fn replay(
        events: &[DebugHypothesisLedgerEvent],
        round_evidence: &[ValidatedDebugHypothesisRoundEvidence],
    ) -> Result<Self, DebugHypothesisLedgerError> {
        let first = events
            .first()
            .ok_or_else(|| replay_conflict("Ledger replay requires an initialization event"))?;
        let seed = seed_from_event(first)?;
        let mut reducer = Self::initialize(seed)?;
        if reducer.events[0].event != *first {
            return Err(replay_conflict(
                "Ledger initialization event is not canonical",
            ));
        }
        for expected in &events[1..] {
            if expected.kind != DebugHypothesisLedgerEventKind::RoundApplied {
                return Err(replay_conflict(
                    "Ledger replay contains a non-applied event",
                ));
            }
            let reference = expected.source_round_receipt.as_ref().ok_or_else(|| {
                replay_conflict("Applied Ledger event has no source round receipt")
            })?;
            let evidence = round_evidence
                .iter()
                .find(|item| item.receipt_reference() == reference)
                .ok_or_else(|| {
                    replay_conflict("Applied Ledger event source evidence is unavailable")
                })?;
            let update = update_from_event(expected)?;
            let outcome = reducer.apply_round(update, evidence, &expected.authority)?;
            if outcome.is_duplicate() || outcome.event().event() != expected {
                return Err(replay_conflict("Ledger replay event is not canonical"));
            }
        }
        Ok(reducer)
    }

    /// Applies one model-proposed update against exact current-round evidence.
    ///
    /// The expected authority must come from the caller's Prepared operation
    /// journal and is checked again at commit. A mismatch returns
    /// `StaleAuthority` before any reducer field is changed.
    ///
    /// # Errors
    ///
    /// Returns a bounded error when authority, replay identity, current
    /// evidence, a transition, a digest, or a collection bound is invalid.
    pub fn apply_round(
        &mut self,
        mut update: DebugHypothesisLedgerUpdate,
        evidence: &ValidatedDebugHypothesisRoundEvidence,
        expected_active_authority: &DebugProbeRoundAuthority,
    ) -> Result<DebugHypothesisLedgerApplyOutcome, DebugHypothesisLedgerError> {
        canonicalize_update(&mut update);
        if let Some(original) =
            self.preflight_round_update(&update, evidence, expected_active_authority)?
        {
            return Ok(DebugHypothesisLedgerApplyOutcome::Duplicate(original));
        }
        validate_update_bounds(&update)?;

        let previous = self.ledger.ledger.clone();
        let mut next = previous.clone();
        let mut round_support = Vec::new();
        for mutation in &update.mutations {
            apply_hypothesis_mutation(&mut next, mutation, evidence, &mut round_support)?;
        }
        apply_confirmed_facts(
            &mut next,
            &update.confirmed_facts,
            evidence,
            &round_support,
            &expected_active_authority.round_id,
        )?;
        apply_recipe_update(
            &mut next,
            update.reproduction_recipe_update.as_ref(),
            evidence,
            &round_support,
            &expected_active_authority.round_id,
        )?;
        apply_question_changes(
            &mut next,
            &update.opened_questions,
            &update.resolved_question_digests,
            &expected_active_authority.round_id,
        )?;
        validate_session_transition(
            &previous.session_status,
            &update.session_status,
            &next.hypotheses,
            evidence.is_complete_round(),
        )?;
        next.session_status.clone_from(&update.session_status);

        let changed = next.hypotheses != previous.hypotheses
            || next.confirmed_facts != previous.confirmed_facts
            || next.reproduction_recipe != previous.reproduction_recipe
            || next.unresolved_questions != previous.unresolved_questions
            || next.session_status != previous.session_status;
        if !changed {
            return Err(invalid("Ledger round update is empty"));
        }
        self.commit_round_update(update, &previous, next, expected_active_authority)
    }

    fn preflight_round_update(
        &self,
        update: &DebugHypothesisLedgerUpdate,
        evidence: &ValidatedDebugHypothesisRoundEvidence,
        expected_active_authority: &DebugProbeRoundAuthority,
    ) -> Result<Option<ValidatedDebugHypothesisLedgerEvent>, DebugHypothesisLedgerError> {
        validate_sha256(&update.source_context_digest)?;
        validate_sha256(&update.source_request_digest)?;
        if let Some(index) = self
            .receipts
            .get(&update.source_round_receipt.receipt_digest.0)
            .copied()
        {
            let original = &self.events[index];
            if event_matches_update(original.event(), update) {
                return Ok(Some(original.clone()));
            }
            return Err(replay_conflict(
                "Probe round receipt was replayed with different Ledger content",
            ));
        }
        if self.requests.contains_key(&update.source_request_digest.0) {
            return Err(replay_conflict(
                "Ledger source request digest was reused for another event",
            ));
        }
        validate_authority(expected_active_authority)?;
        if evidence.receipt_reference() != &update.source_round_receipt
            || &update.source_round_receipt.authority != expected_active_authority
        {
            return Err(stale("Ledger update source authority is no longer active"));
        }
        validate_session_scope(&self.ledger.ledger.authority, expected_active_authority)?;
        if update.previous_ledger_digest != self.ledger.ledger.ledger_digest {
            return Err(replay_conflict(
                "Ledger update previous digest does not match current projection",
            ));
        }
        if update.occurred_at.0 < evidence.cut.receipt.finished_at.0
            || update.occurred_at.0 < self.ledger.ledger.updated_at.0
        {
            return Err(invalid(
                "Ledger update timestamp precedes its durable inputs",
            ));
        }
        Ok(None)
    }

    fn commit_round_update(
        &mut self,
        update: DebugHypothesisLedgerUpdate,
        previous: &DebugHypothesisLedger,
        mut next: DebugHypothesisLedger,
        authority: &DebugProbeRoundAuthority,
    ) -> Result<DebugHypothesisLedgerApplyOutcome, DebugHypothesisLedgerError> {
        canonicalize_ledger_collections(&mut next);
        let sequence = previous
            .event_sequence
            .0
            .checked_add(1)
            .ok_or_else(|| sequence("Ledger event sequence overflowed"))?;
        next.authority = authority.clone();
        next.event_sequence = ExecutionSequence(sequence);
        next.latest_applied_round_receipt = Some(update.source_round_receipt.clone());
        next.updated_at = update.occurred_at.clone();
        next.ledger_digest = derive_debug_hypothesis_ledger_digest(&next)?;
        let receipt_digest = update.source_round_receipt.receipt_digest.0.clone();
        let request_digest = update.source_request_digest.0.clone();
        let mut event = round_event_from_update(update, previous, &next, sequence, authority);
        event.event_digest = derive_debug_hypothesis_ledger_event_digest(&event)?;
        next.last_event_digest = event.event_digest.clone();
        validate_ledger_projection(&next)?;

        let event = ValidatedDebugHypothesisLedgerEvent { event };
        let index = self.events.len();
        self.receipts.insert(receipt_digest, index);
        self.requests.insert(request_digest, index);
        self.ledger = ValidatedDebugHypothesisLedger { ledger: next };
        self.events.push(event.clone());
        Ok(DebugHypothesisLedgerApplyOutcome::Applied(event))
    }

    /// Returns the current validated Ledger projection.
    #[must_use]
    pub const fn ledger(&self) -> &ValidatedDebugHypothesisLedger {
        &self.ledger
    }

    /// Returns every canonical current-chain event in sequence order.
    #[must_use]
    pub fn events(&self) -> &[ValidatedDebugHypothesisLedgerEvent] {
        &self.events
    }
}

fn round_event_from_update(
    update: DebugHypothesisLedgerUpdate,
    previous: &DebugHypothesisLedger,
    next: &DebugHypothesisLedger,
    sequence: i64,
    authority: &DebugProbeRoundAuthority,
) -> DebugHypothesisLedgerEvent {
    DebugHypothesisLedgerEvent {
        authority: authority.clone(),
        confirmed_facts: update.confirmed_facts,
        event_digest: zero_digest(),
        kind: DebugHypothesisLedgerEventKind::RoundApplied,
        mutations: update.mutations,
        occurred_at: update.occurred_at,
        opened_questions: update.opened_questions,
        previous_event_digest: Some(previous.last_event_digest.clone()),
        previous_ledger_digest: Some(previous.ledger_digest.clone()),
        reproduction_recipe_update: update.reproduction_recipe_update,
        resolved_question_digests: update.resolved_question_digests,
        resulting_ledger_digest: next.ledger_digest.clone(),
        schema_version: 1,
        sequence: ExecutionSequence(sequence),
        session_status: update.session_status,
        source_context_digest: Some(update.source_context_digest),
        source_request_digest: Some(update.source_request_digest),
        source_round_receipt: Some(update.source_round_receipt),
    }
}

/// Seals one terminal round and the exact D3 projections used by the model.
///
/// # Errors
///
/// Returns a bounded error when the receipt, Artifact binding, projection
/// coverage, candidate derivation, authority, or semantic digest is invalid.
pub fn seal_debug_hypothesis_round_evidence(
    plan: &ValidatedDebugProbePlan,
    receipt: &ProbeRoundReceipt,
    receipt_artifact_ref: ArtifactReference,
    projections: &[ProbeEvidenceProjection],
) -> Result<ValidatedDebugHypothesisRoundEvidence, DebugHypothesisLedgerError> {
    validate_probe_round_receipt(receipt, plan)
        .map_err(|_| invalid("Probe round receipt does not match its sealed plan"))?;
    validate_artifact_reference(&receipt_artifact_ref)?;
    let bytes = canonical_probe_round_receipt_bytes(receipt)?;
    if sha256_bytes(&bytes) != receipt_artifact_ref.digest {
        return Err(digest("Probe round receipt Artifact digest is invalid"));
    }

    let receipt_identities = expected_evidence_identity_set(receipt);
    let mut projection_identities = BTreeSet::new();
    for projection in projections {
        let summary = projection.summary();
        let identity = &summary.identity;
        if identity.debug_session_id != receipt.authority.debug_session_id
            || identity.job_id != receipt.authority.job_id
            || identity.attempt != receipt.authority.attempt
            || identity.lease_id != receipt.authority.lease_id
            || identity.fencing_token != receipt.authority.fencing_token
            || identity.session_identity != receipt.authority.session_identity
            || identity.repository_id != receipt.authority.repository_id
            || identity.round_id != receipt.authority.round_id
            || identity.workspace_revision != receipt.authority.workspace_revision
            || identity.environment_digest != receipt.authority.environment_digest
            || !receipt_identities.contains(&identity.probe_execution_id.0)
            || !projection_identities.insert(identity.probe_execution_id.0.clone())
        {
            return Err(invalid(
                "Probe evidence projection does not belong to the exact round receipt",
            ));
        }
        let probe = plan
            .probe_by_id(&identity.probe_id)
            .ok_or_else(|| invalid("Probe evidence projection has an unknown probe"))?;
        for candidate in projection.candidates() {
            if candidate.evidence.identity != *identity
                || candidate.evidence.artifact_ref != summary.bundle_artifact_ref
                || candidate.evidence.recorded_at != summary.recorded_at
                || !probe
                    .spec()
                    .target_hypothesis_ids
                    .contains(&candidate.target_hypothesis_id)
                || candidate.evidence.evidence_digest
                    != derive_hypothesis_evidence_digest(
                        identity,
                        &candidate.target_hypothesis_id,
                        &summary.bundle_digest,
                    )
                    .map_err(|_| digest("Probe hypothesis evidence digest is invalid"))?
            {
                return Err(digest(
                    "Probe hypothesis evidence candidate is not the exact D3 projection",
                ));
            }
        }
    }
    if projection_identities != receipt_identities {
        return Err(missing_evidence(
            "Probe evidence projections do not cover every receipt probe exactly once",
        ));
    }
    let reference = ProbeRoundReceiptReference {
        authority: receipt.authority.clone(),
        plan_digest: receipt.plan_digest.clone(),
        receipt_artifact_ref,
        receipt_digest: derive_probe_round_receipt_digest(receipt)?,
    };
    let mut summaries = projections
        .iter()
        .map(|value| value.summary().clone())
        .collect::<Vec<_>>();
    let mut candidates = projections
        .iter()
        .flat_map(|value| value.candidates().iter().cloned())
        .collect::<Vec<_>>();
    canonicalize_evidence_cut(&mut summaries, &mut candidates);
    let cut = DebugHypothesisRoundEvidence {
        evidence_candidates: candidates,
        evidence_cut_digest: zero_digest(),
        evidence_summaries: summaries,
        receipt: receipt.clone(),
        schema_version: 1,
        source_round_receipt: reference,
    };
    let mut cut = cut;
    cut.evidence_cut_digest = derive_debug_hypothesis_round_evidence_digest(&cut)?;
    validate_round_evidence_cut(&cut, plan)?;
    Ok(ValidatedDebugHypothesisRoundEvidence { cut })
}

/// Returns the exact canonical bytes persisted with a Prepared context.
///
/// # Panics
///
/// Generated wire values contain no map keys or custom serializers, so JSON
/// serialization is infallible. A panic indicates a generated-contract bug.
#[must_use]
pub fn canonical_debug_hypothesis_round_evidence_bytes(
    evidence: &ValidatedDebugHypothesisRoundEvidence,
) -> Vec<u8> {
    serde_json::to_vec(evidence.cut())
        .expect("generated hypothesis round evidence serialization is infallible")
}

/// Reopens one durable current-round evidence cut without rerunning a probe or
/// D3 normalizer. The exact sealed plan is required to revalidate its receipt.
///
/// # Errors
///
/// Returns a bounded error when bytes are malformed or non-canonical, the
/// Prepared digest differs, or any receipt/projection invariant fails.
pub fn reopen_debug_hypothesis_round_evidence(
    bytes: &[u8],
    plan: &ValidatedDebugProbePlan,
    expected_evidence_cut_digest: &Sha256Digest,
) -> Result<ValidatedDebugHypothesisRoundEvidence, DebugHypothesisLedgerError> {
    let cut = serde_json::from_slice::<DebugHypothesisRoundEvidence>(bytes)
        .map_err(|_| invalid("Hypothesis round evidence bytes are invalid"))?;
    if serde_json::to_vec(&cut)
        .map_err(|_| serialization("Hypothesis round evidence serialization failed"))?
        != bytes
    {
        return Err(invalid("Hypothesis round evidence bytes are not canonical"));
    }
    if &cut.evidence_cut_digest != expected_evidence_cut_digest {
        return Err(digest(
            "Hypothesis round evidence differs from the Prepared journal digest",
        ));
    }
    validate_round_evidence_cut(&cut, plan)?;
    Ok(ValidatedDebugHypothesisRoundEvidence { cut })
}

/// Derives the semantic digest of a durable evidence cut, excluding only its
/// own `evidenceCutDigest` field.
///
/// # Errors
///
/// Returns a serialization error if a generated field cannot be encoded.
pub fn derive_debug_hypothesis_round_evidence_digest(
    cut: &DebugHypothesisRoundEvidence,
) -> Result<Sha256Digest, DebugHypothesisLedgerError> {
    let mut digest = FramedDigest::new(ROUND_EVIDENCE_DIGEST_DOMAIN);
    digest.json(b"schemaVersion", &cut.schema_version)?;
    digest.json(b"receipt", &cut.receipt)?;
    digest.json(b"sourceRoundReceipt", &cut.source_round_receipt)?;
    digest.json(b"evidenceSummaries", &cut.evidence_summaries)?;
    digest.json(b"evidenceCandidates", &cut.evidence_candidates)?;
    Ok(digest.finish())
}

/// Returns the exact generated JSON bytes used by the receipt Artifact.
///
/// # Errors
///
/// Returns a serialization error if the generated receipt cannot be encoded.
pub fn canonical_probe_round_receipt_bytes(
    receipt: &ProbeRoundReceipt,
) -> Result<Vec<u8>, DebugHypothesisLedgerError> {
    serde_json::to_vec(receipt)
        .map_err(|_| serialization("Probe round receipt serialization failed"))
}

/// Derives the semantic digest for one validated terminal round receipt.
///
/// # Errors
///
/// Returns a serialization error if the generated receipt cannot be encoded.
pub fn derive_probe_round_receipt_digest(
    receipt: &ProbeRoundReceipt,
) -> Result<Sha256Digest, DebugHypothesisLedgerError> {
    let mut digest = FramedDigest::new(ROUND_RECEIPT_DIGEST_DOMAIN);
    digest.json(b"receipt", receipt)?;
    Ok(digest.finish())
}

/// Derives the canonical projection digest. `ledgerDigest` and
/// `lastEventDigest` are excluded to avoid a circular event/snapshot hash.
///
/// # Errors
///
/// Returns a serialization error if a generated projection field cannot be
/// encoded.
pub fn derive_debug_hypothesis_ledger_digest(
    ledger: &DebugHypothesisLedger,
) -> Result<Sha256Digest, DebugHypothesisLedgerError> {
    let mut digest = FramedDigest::new(LEDGER_DIGEST_DOMAIN);
    digest.json(b"schemaVersion", &ledger.schema_version)?;
    digest.json(b"authority", &ledger.authority)?;
    digest.json(b"sessionStatus", &ledger.session_status)?;
    digest.json(b"eventSequence", &ledger.event_sequence)?;
    digest.json(
        b"latestAppliedRoundReceipt",
        &ledger.latest_applied_round_receipt,
    )?;
    digest.json(b"hypotheses", &ledger.hypotheses)?;
    digest.json(b"confirmedFacts", &ledger.confirmed_facts)?;
    digest.json(b"reproductionRecipe", &ledger.reproduction_recipe)?;
    digest.json(b"unresolvedQuestions", &ledger.unresolved_questions)?;
    digest.json(b"updatedAt", &ledger.updated_at)?;
    Ok(digest.finish())
}

/// Derives the canonical applied-event digest, excluding only `eventDigest`.
///
/// # Errors
///
/// Returns a serialization error if a generated event field cannot be encoded.
pub fn derive_debug_hypothesis_ledger_event_digest(
    event: &DebugHypothesisLedgerEvent,
) -> Result<Sha256Digest, DebugHypothesisLedgerError> {
    let mut digest = FramedDigest::new(LEDGER_EVENT_DIGEST_DOMAIN);
    digest.json(b"schemaVersion", &event.schema_version)?;
    digest.json(b"kind", &event.kind)?;
    digest.json(b"sequence", &event.sequence)?;
    digest.json(b"authority", &event.authority)?;
    digest.json(b"previousEventDigest", &event.previous_event_digest)?;
    digest.json(b"previousLedgerDigest", &event.previous_ledger_digest)?;
    digest.json(b"sourceRoundReceipt", &event.source_round_receipt)?;
    digest.json(b"sourceContextDigest", &event.source_context_digest)?;
    digest.json(b"sourceRequestDigest", &event.source_request_digest)?;
    digest.json(b"mutations", &event.mutations)?;
    digest.json(b"confirmedFacts", &event.confirmed_facts)?;
    digest.json(
        b"reproductionRecipeUpdate",
        &event.reproduction_recipe_update,
    )?;
    digest.json(b"openedQuestions", &event.opened_questions)?;
    digest.json(b"resolvedQuestionDigests", &event.resolved_question_digests)?;
    digest.json(b"sessionStatus", &event.session_status)?;
    digest.json(b"resultingLedgerDigest", &event.resulting_ledger_digest)?;
    digest.json(b"occurredAt", &event.occurred_at)?;
    Ok(digest.finish())
}

/// Returns the exact canonical JSON bytes of a validated Ledger projection.
///
/// # Panics
///
/// Generated wire values contain no map keys or custom serializers, so JSON
/// serialization is infallible. A panic indicates a generated-contract bug.
#[must_use]
pub fn canonical_debug_hypothesis_ledger_bytes(ledger: &ValidatedDebugHypothesisLedger) -> Vec<u8> {
    serde_json::to_vec(ledger.ledger()).expect("generated Ledger serialization is infallible")
}

/// Returns the exact canonical JSON bytes of a validated Ledger event.
///
/// # Panics
///
/// Generated wire values contain no map keys or custom serializers, so JSON
/// serialization is infallible. A panic indicates a generated-contract bug.
#[must_use]
pub fn canonical_debug_hypothesis_ledger_event_bytes(
    event: &ValidatedDebugHypothesisLedgerEvent,
) -> Vec<u8> {
    serde_json::to_vec(event.event()).expect("generated Ledger event serialization is infallible")
}

/// Decodes exact canonical bytes for later replay. Semantic validation still
/// occurs only when the event is passed to [`DebugHypothesisLedgerReducer::replay`].
///
/// # Errors
///
/// Returns a bounded error when bytes are malformed or not the one canonical
/// JSON encoding of the event.
pub fn decode_canonical_debug_hypothesis_ledger_event(
    bytes: &[u8],
) -> Result<DebugHypothesisLedgerEvent, DebugHypothesisLedgerError> {
    let event = serde_json::from_slice::<DebugHypothesisLedgerEvent>(bytes)
        .map_err(|_| invalid("Ledger event bytes are invalid"))?;
    if serde_json::to_vec(&event).map_err(|_| serialization("Ledger event serialization failed"))?
        != bytes
    {
        return Err(invalid("Ledger event bytes are not canonical"));
    }
    Ok(event)
}

/// Derives a content digest for one confirmed fact.
///
/// # Errors
///
/// Returns a serialization error if a generated fact field cannot be encoded.
pub fn derive_debug_confirmed_fact_digest(
    fact: &DebugConfirmedFact,
) -> Result<Sha256Digest, DebugHypothesisLedgerError> {
    let mut evidence = fact.evidence.clone();
    evidence.sort_by(|left, right| candidate_sort_key(left).cmp(&candidate_sort_key(right)));
    let mut digest = FramedDigest::new(FACT_DIGEST_DOMAIN);
    digest.json(b"summary", &fact.summary)?;
    digest.json(b"evidence", &evidence)?;
    digest.json(b"confirmedRoundId", &fact.confirmed_round_id)?;
    Ok(digest.finish())
}

/// Derives a content digest for one reproduction step.
///
/// # Errors
///
/// Returns a serialization error if a generated step field cannot be encoded.
pub fn derive_debug_reproduction_recipe_step_digest(
    step: &DebugReproductionRecipeStep,
) -> Result<Sha256Digest, DebugHypothesisLedgerError> {
    let mut digest = FramedDigest::new(RECIPE_STEP_DIGEST_DOMAIN);
    digest.json(b"summary", &step.summary)?;
    digest.json(b"probeDefinitionDigest", &step.probe_definition_digest)?;
    Ok(digest.finish())
}

/// Derives a content digest for one ordered reproduction recipe.
///
/// # Errors
///
/// Returns a serialization error if a generated recipe field cannot be encoded.
pub fn derive_debug_reproduction_recipe_digest(
    recipe: &DebugReproductionRecipe,
) -> Result<Sha256Digest, DebugHypothesisLedgerError> {
    let mut evidence = recipe.evidence.clone();
    evidence.sort_by(|left, right| candidate_sort_key(left).cmp(&candidate_sort_key(right)));
    let mut digest = FramedDigest::new(RECIPE_DIGEST_DOMAIN);
    digest.json(b"steps", &recipe.steps)?;
    digest.json(b"evidence", &evidence)?;
    digest.json(b"lastUpdatedRoundId", &recipe.last_updated_round_id)?;
    Ok(digest.finish())
}

/// Derives a content digest for one unresolved question.
///
/// # Errors
///
/// Returns a serialization error if a generated question cannot be encoded.
pub fn derive_debug_unresolved_question_digest(
    question: &DebugUnresolvedQuestion,
) -> Result<Sha256Digest, DebugHypothesisLedgerError> {
    let mut digest = FramedDigest::new(QUESTION_DIGEST_DOMAIN);
    digest.json(b"summary", &question.summary)?;
    digest.json(b"openedRoundId", &question.opened_round_id)?;
    Ok(digest.finish())
}

fn apply_hypothesis_mutation(
    ledger: &mut DebugHypothesisLedger,
    mutation: &DebugHypothesisMutation,
    evidence: &ValidatedDebugHypothesisRoundEvidence,
    round_support: &mut Vec<HypothesisEvidenceCandidate>,
) -> Result<(), DebugHypothesisLedgerError> {
    validate_summary(&mutation.summary)?;
    if !(0..=10_000).contains(&mutation.confidence_bps) {
        return Err(invalid(
            "Hypothesis confidence is outside basis-point bounds",
        ));
    }
    let existing_index = ledger
        .hypotheses
        .iter()
        .position(|value| value.hypothesis_id == mutation.hypothesis_id);
    let (previous_status, previous_confidence, mut hypothesis) = match mutation.kind {
        DebugHypothesisMutationKind::Create => {
            if existing_index.is_some()
                || mutation.previous_status.is_some()
                || mutation.previous_confidence_bps.is_some()
                || mutation.assessments.is_empty()
                || mutation.status != DebugHypothesisStatus::Active
            {
                return Err(invalid_transition(
                    "Evidence-backed hypothesis creation has invalid previous or next state",
                ));
            }
            (
                DebugHypothesisStatus::Active,
                0,
                DebugHypothesis {
                    confidence_bps: 0,
                    contradicting_evidence: Vec::new(),
                    created_round_id: evidence.cut.source_round_receipt.authority.round_id.clone(),
                    hypothesis_id: mutation.hypothesis_id.clone(),
                    last_updated_round_id: evidence
                        .cut
                        .source_round_receipt
                        .authority
                        .round_id
                        .clone(),
                    status: DebugHypothesisStatus::Active,
                    summary: mutation.summary.clone(),
                    supporting_evidence: Vec::new(),
                },
            )
        }
        DebugHypothesisMutationKind::Update => {
            let index = existing_index.ok_or_else(|| {
                invalid_transition("Hypothesis update references an unknown hypothesis")
            })?;
            let value = ledger.hypotheses[index].clone();
            if mutation.previous_status.as_ref() != Some(&value.status)
                || mutation.previous_confidence_bps != Some(value.confidence_bps)
                || value.status != DebugHypothesisStatus::Active
            {
                return Err(invalid_transition(
                    "Hypothesis update previous or terminal state is invalid",
                ));
            }
            (value.status.clone(), value.confidence_bps, value)
        }
    };

    let assessments =
        apply_evidence_assessments(&mut hypothesis, mutation, evidence, round_support)?;
    validate_hypothesis_transition(
        mutation,
        &previous_status,
        previous_confidence,
        &assessments,
        evidence.is_complete_round(),
    )?;
    hypothesis.summary.clone_from(&mutation.summary);
    hypothesis.status = mutation.status.clone();
    hypothesis.confidence_bps = mutation.confidence_bps;
    hypothesis.last_updated_round_id = evidence.cut.source_round_receipt.authority.round_id.clone();
    canonicalize_evidence(&mut hypothesis.supporting_evidence);
    canonicalize_evidence(&mut hypothesis.contradicting_evidence);
    if hypothesis.supporting_evidence.len() > 64 || hypothesis.contradicting_evidence.len() > 64 {
        return Err(invalid(
            "Hypothesis evidence count exceeds canonical bounds",
        ));
    }
    if let Some(index) = existing_index {
        ledger.hypotheses[index] = hypothesis;
    } else {
        ledger.hypotheses.push(hypothesis);
    }
    Ok(())
}

#[derive(Clone, Copy, Default)]
enum AssessmentDisposition {
    #[default]
    None,
    Supports,
    Contradicts,
    Mixed,
}

impl AssessmentDisposition {
    const fn with_support(self) -> Self {
        match self {
            Self::None | Self::Supports => Self::Supports,
            Self::Contradicts | Self::Mixed => Self::Mixed,
        }
    }

    const fn with_contradiction(self) -> Self {
        match self {
            Self::None | Self::Contradicts => Self::Contradicts,
            Self::Supports | Self::Mixed => Self::Mixed,
        }
    }
}

#[derive(Default)]
struct AssessmentKinds {
    disposition: AssessmentDisposition,
    complete_support: bool,
    complete_contradiction: bool,
}

fn apply_evidence_assessments(
    hypothesis: &mut DebugHypothesis,
    mutation: &DebugHypothesisMutation,
    evidence: &ValidatedDebugHypothesisRoundEvidence,
    round_support: &mut Vec<HypothesisEvidenceCandidate>,
) -> Result<AssessmentKinds, DebugHypothesisLedgerError> {
    let mut kinds = AssessmentKinds::default();
    let mut seen = BTreeSet::new();
    for assessment in &mutation.assessments {
        if assessment.candidate.target_hypothesis_id != mutation.hypothesis_id
            || !seen.insert(candidate_key(&assessment.candidate)?)
        {
            return Err(evidence_conflict(
                "Hypothesis assessment target or uniqueness is invalid",
            ));
        }
        let completeness = evidence
            .candidate_completeness(&assessment.candidate)
            .ok_or_else(|| {
                missing_evidence("Hypothesis assessment is not exact current evidence")
            })?;
        let value = assessment.candidate.evidence.clone();
        match assessment.polarity {
            DebugHypothesisEvidencePolarity::Supports => {
                if contains_evidence(&hypothesis.contradicting_evidence, &value)
                    || contains_evidence(&hypothesis.supporting_evidence, &value)
                {
                    return Err(evidence_conflict(
                        "Hypothesis evidence was already assessed or has conflicting polarity",
                    ));
                }
                kinds.disposition = kinds.disposition.with_support();
                kinds.complete_support |=
                    *completeness == ProbeEvidenceCompletenessStatus::Complete;
                hypothesis.supporting_evidence.push(value);
                round_support.push(assessment.candidate.clone());
            }
            DebugHypothesisEvidencePolarity::Contradicts => {
                if contains_evidence(&hypothesis.supporting_evidence, &value)
                    || contains_evidence(&hypothesis.contradicting_evidence, &value)
                {
                    return Err(evidence_conflict(
                        "Hypothesis evidence was already assessed or has conflicting polarity",
                    ));
                }
                kinds.disposition = kinds.disposition.with_contradiction();
                kinds.complete_contradiction |=
                    *completeness == ProbeEvidenceCompletenessStatus::Complete;
                hypothesis.contradicting_evidence.push(value);
            }
        }
    }
    Ok(kinds)
}

fn validate_hypothesis_transition(
    mutation: &DebugHypothesisMutation,
    previous_status: &DebugHypothesisStatus,
    previous_confidence: i64,
    assessments: &AssessmentKinds,
    complete_round: bool,
) -> Result<(), DebugHypothesisLedgerError> {
    if mutation.assessments.is_empty()
        && (mutation.status != *previous_status || mutation.confidence_bps != previous_confidence)
    {
        return Err(missing_evidence(
            "Hypothesis state or confidence changed without current evidence",
        ));
    }
    match assessments.disposition {
        AssessmentDisposition::Supports if mutation.confidence_bps < previous_confidence => {
            return Err(invalid_transition(
                "Supporting evidence cannot lower hypothesis confidence",
            ));
        }
        AssessmentDisposition::Contradicts if mutation.confidence_bps > previous_confidence => {
            return Err(invalid_transition(
                "Contradicting evidence cannot raise hypothesis confidence",
            ));
        }
        AssessmentDisposition::Mixed if mutation.confidence_bps != previous_confidence => {
            return Err(invalid_transition(
                "Mixed evidence cannot move hypothesis confidence automatically",
            ));
        }
        AssessmentDisposition::None
        | AssessmentDisposition::Supports
        | AssessmentDisposition::Contradicts
        | AssessmentDisposition::Mixed => {}
    }
    match mutation.status {
        DebugHypothesisStatus::Active if mutation.confidence_bps < 10_000 => Ok(()),
        DebugHypothesisStatus::Confirmed
            if mutation.confidence_bps == 10_000
                && assessments.complete_support
                && complete_round =>
        {
            Ok(())
        }
        DebugHypothesisStatus::Rejected
            if mutation.confidence_bps == 0
                && assessments.complete_contradiction
                && complete_round =>
        {
            Ok(())
        }
        _ => Err(invalid_transition(
            "Hypothesis terminal state, endpoint confidence or complete evidence is invalid",
        )),
    }
}

fn apply_confirmed_facts(
    ledger: &mut DebugHypothesisLedger,
    facts: &[DebugConfirmedFact],
    evidence: &ValidatedDebugHypothesisRoundEvidence,
    round_support: &[HypothesisEvidenceCandidate],
    round_id: &winwincode_domain::ProbeRoundId,
) -> Result<(), DebugHypothesisLedgerError> {
    if !facts.is_empty() && !evidence.is_complete_round() {
        return Err(missing_evidence(
            "Confirmed facts require a complete terminal round",
        ));
    }
    for fact in facts {
        validate_summary(&fact.summary)?;
        if fact.confirmed_round_id != *round_id
            || fact.evidence.is_empty()
            || fact.evidence.len() > 16
            || !is_candidates_canonical(&fact.evidence)
            || fact.fact_digest != derive_debug_confirmed_fact_digest(fact)?
            || ledger
                .confirmed_facts
                .iter()
                .any(|value| value.fact_digest == fact.fact_digest)
        {
            return Err(invalid(
                "Confirmed fact identity or round binding is invalid",
            ));
        }
        for candidate in &fact.evidence {
            if evidence.candidate_completeness(candidate)
                != Some(&ProbeEvidenceCompletenessStatus::Complete)
                || !round_support.contains(candidate)
            {
                return Err(missing_evidence(
                    "Confirmed fact lacks exact complete supporting evidence",
                ));
            }
        }
        ledger.confirmed_facts.push(fact.clone());
    }
    if ledger.confirmed_facts.len() > 128 {
        return Err(invalid("Confirmed fact count exceeds canonical bounds"));
    }
    Ok(())
}

fn apply_recipe_update(
    ledger: &mut DebugHypothesisLedger,
    update: Option<&DebugReproductionRecipeUpdate>,
    evidence: &ValidatedDebugHypothesisRoundEvidence,
    round_support: &[HypothesisEvidenceCandidate],
    round_id: &winwincode_domain::ProbeRoundId,
) -> Result<(), DebugHypothesisLedgerError> {
    let Some(update) = update else { return Ok(()) };
    if !evidence.is_complete_round() {
        return Err(missing_evidence(
            "Reproduction recipe requires a complete terminal round",
        ));
    }
    if update.previous_recipe_digest
        != ledger
            .reproduction_recipe
            .as_ref()
            .map(|value| value.recipe_digest.clone())
    {
        return Err(replay_conflict(
            "Reproduction recipe previous digest is stale",
        ));
    }
    validate_recipe(&update.recipe, evidence, round_support, round_id)?;
    if let Some(recipe) = &mut ledger.reproduction_recipe {
        recipe.clone_from(&update.recipe);
    } else {
        ledger.reproduction_recipe = Some(update.recipe.clone());
    }
    Ok(())
}

fn validate_recipe(
    recipe: &DebugReproductionRecipe,
    evidence: &ValidatedDebugHypothesisRoundEvidence,
    round_support: &[HypothesisEvidenceCandidate],
    round_id: &winwincode_domain::ProbeRoundId,
) -> Result<(), DebugHypothesisLedgerError> {
    if recipe.steps.is_empty()
        || recipe.steps.len() > 32
        || recipe.evidence.is_empty()
        || recipe.evidence.len() > 16
        || !is_candidates_canonical(&recipe.evidence)
        || recipe.last_updated_round_id != *round_id
    {
        return Err(invalid("Reproduction recipe bounds or round are invalid"));
    }
    let mut steps = BTreeSet::new();
    for step in &recipe.steps {
        validate_summary(&step.summary)?;
        if step.step_digest != derive_debug_reproduction_recipe_step_digest(step)?
            || !steps.insert(step.step_digest.0.clone())
        {
            return Err(digest("Reproduction recipe step digest is invalid"));
        }
        if let Some(definition) = &step.probe_definition_digest {
            validate_sha256(definition)?;
        }
    }
    for candidate in &recipe.evidence {
        if evidence.candidate_completeness(candidate)
            != Some(&ProbeEvidenceCompletenessStatus::Complete)
            || !round_support.contains(candidate)
        {
            return Err(missing_evidence(
                "Reproduction recipe lacks complete current supporting evidence",
            ));
        }
    }
    if recipe.recipe_digest != derive_debug_reproduction_recipe_digest(recipe)? {
        return Err(digest("Reproduction recipe digest is invalid"));
    }
    Ok(())
}

fn apply_question_changes(
    ledger: &mut DebugHypothesisLedger,
    opened: &[DebugUnresolvedQuestion],
    resolved: &[Sha256Digest],
    round_id: &winwincode_domain::ProbeRoundId,
) -> Result<(), DebugHypothesisLedgerError> {
    let opened_ids = opened
        .iter()
        .map(|value| value.question_digest.0.clone())
        .collect::<BTreeSet<_>>();
    let resolved_ids = resolved
        .iter()
        .map(|value| value.0.clone())
        .collect::<BTreeSet<_>>();
    if opened_ids.len() != opened.len()
        || resolved_ids.len() != resolved.len()
        || !opened_ids.is_disjoint(&resolved_ids)
    {
        return Err(invalid("Question changes are duplicated or contradictory"));
    }
    for question in opened {
        validate_question(question, round_id)?;
        if ledger
            .unresolved_questions
            .iter()
            .any(|value| value.question_digest == question.question_digest)
        {
            return Err(invalid("Opened question already exists"));
        }
        ledger.unresolved_questions.push(question.clone());
    }
    for digest in resolved {
        validate_sha256(digest)?;
        let index = ledger
            .unresolved_questions
            .iter()
            .position(|value| value.question_digest == *digest)
            .ok_or_else(|| invalid("Resolved question does not exist"))?;
        ledger.unresolved_questions.remove(index);
    }
    if ledger.unresolved_questions.len() > 64 {
        return Err(invalid(
            "Unresolved question count exceeds canonical bounds",
        ));
    }
    Ok(())
}

fn validate_session_transition(
    previous: &DebugSessionStatus,
    next: &DebugSessionStatus,
    hypotheses: &[DebugHypothesis],
    complete_round: bool,
) -> Result<(), DebugHypothesisLedgerError> {
    let legal = match previous {
        DebugSessionStatus::Active => matches!(
            next,
            DebugSessionStatus::Active | DebugSessionStatus::RootCauseIdentified
        ),
        DebugSessionStatus::RootCauseIdentified => matches!(
            next,
            DebugSessionStatus::RootCauseIdentified
                | DebugSessionStatus::TransitionedToDelegatedBatch
                | DebugSessionStatus::Completed
        ),
        DebugSessionStatus::TransitionedToDelegatedBatch
        | DebugSessionStatus::Completed
        | DebugSessionStatus::Cancelled
        | DebugSessionStatus::Failed => false,
    };
    if !legal {
        return Err(invalid_transition("Debug session transition is invalid"));
    }
    if matches!(
        next,
        DebugSessionStatus::RootCauseIdentified
            | DebugSessionStatus::TransitionedToDelegatedBatch
            | DebugSessionStatus::Completed
    ) && (!complete_round
        || !hypotheses
            .iter()
            .any(|value| value.status == DebugHypothesisStatus::Confirmed))
    {
        return Err(invalid_transition(
            "Debug session terminal progress requires a confirmed hypothesis",
        ));
    }
    Ok(())
}

fn validate_seed_hypothesis(
    hypothesis: &DebugHypothesis,
    authority: &DebugProbeRoundAuthority,
) -> Result<(), DebugHypothesisLedgerError> {
    validate_hypothesis_id(&hypothesis.hypothesis_id)?;
    validate_summary(&hypothesis.summary)?;
    if hypothesis.status != DebugHypothesisStatus::Active
        || hypothesis.confidence_bps != 0
        || !hypothesis.supporting_evidence.is_empty()
        || !hypothesis.contradicting_evidence.is_empty()
        || hypothesis.created_round_id != authority.round_id
        || hypothesis.last_updated_round_id != authority.round_id
    {
        return Err(invalid_seed(
            "Ledger seed must contain only active zero-confidence hypotheses",
        ));
    }
    Ok(())
}

fn validate_ledger_projection(
    ledger: &DebugHypothesisLedger,
) -> Result<(), DebugHypothesisLedgerError> {
    if ledger.schema_version != 1 || ledger.event_sequence.0 < 1 {
        return Err(invalid("Ledger projection version or sequence is invalid"));
    }
    validate_authority(&ledger.authority)?;
    validate_sha256(&ledger.last_event_digest)?;
    if derive_debug_hypothesis_ledger_digest(ledger)? != ledger.ledger_digest {
        return Err(digest("Ledger projection digest is invalid"));
    }
    if !is_hypotheses_canonical(&ledger.hypotheses)
        || !is_facts_canonical(&ledger.confirmed_facts)
        || !is_questions_canonical(&ledger.unresolved_questions)
        || ledger
            .confirmed_facts
            .iter()
            .any(|fact| !is_candidates_canonical(&fact.evidence))
        || ledger
            .reproduction_recipe
            .as_ref()
            .is_some_and(|recipe| !is_candidates_canonical(&recipe.evidence))
    {
        return Err(invalid(
            "Ledger projection collection order is not canonical",
        ));
    }
    Ok(())
}

fn validate_update_bounds(
    update: &DebugHypothesisLedgerUpdate,
) -> Result<(), DebugHypothesisLedgerError> {
    if update.mutations.len() > 64
        || update.confirmed_facts.len() > 32
        || update.opened_questions.len() > 32
        || update.resolved_question_digests.len() > 32
    {
        return Err(invalid("Ledger update exceeds canonical collection bounds"));
    }
    ensure_unique_by(
        update.mutations.iter().map(|value| &value.hypothesis_id.0),
        "Ledger update mutates one hypothesis more than once",
    )?;
    if update
        .mutations
        .iter()
        .any(|value| value.assessments.len() > 16)
    {
        return Err(invalid(
            "Hypothesis assessment count exceeds canonical bounds",
        ));
    }
    Ok(())
}

fn validate_question(
    question: &DebugUnresolvedQuestion,
    expected_round: &winwincode_domain::ProbeRoundId,
) -> Result<(), DebugHypothesisLedgerError> {
    validate_summary(&question.summary)?;
    if question.opened_round_id != *expected_round
        || question.last_updated_round_id != *expected_round
        || question.question_digest != derive_debug_unresolved_question_digest(question)?
    {
        return Err(digest("Unresolved question digest or round is invalid"));
    }
    Ok(())
}

fn seed_from_event(
    event: &DebugHypothesisLedgerEvent,
) -> Result<DebugHypothesisLedgerSeed, DebugHypothesisLedgerError> {
    if event.schema_version != 1
        || event.kind != DebugHypothesisLedgerEventKind::Initialized
        || event.sequence.0 != 1
        || event.previous_event_digest.is_some()
        || event.previous_ledger_digest.is_some()
        || event.source_round_receipt.is_some()
        || event.source_context_digest.is_some()
        || event.source_request_digest.is_some()
        || !event.confirmed_facts.is_empty()
        || event.reproduction_recipe_update.is_some()
        || !event.resolved_question_digests.is_empty()
        || event.session_status != DebugSessionStatus::Active
        || derive_debug_hypothesis_ledger_event_digest(event)? != event.event_digest
    {
        return Err(replay_conflict(
            "Ledger initialization event shape is invalid",
        ));
    }
    let hypotheses = event
        .mutations
        .iter()
        .map(|mutation| {
            if mutation.kind != DebugHypothesisMutationKind::Create
                || mutation.previous_status.is_some()
                || mutation.previous_confidence_bps.is_some()
                || !mutation.assessments.is_empty()
            {
                return Err(replay_conflict("Ledger initialization mutation is invalid"));
            }
            Ok(DebugHypothesis {
                confidence_bps: mutation.confidence_bps,
                contradicting_evidence: Vec::new(),
                created_round_id: event.authority.round_id.clone(),
                hypothesis_id: mutation.hypothesis_id.clone(),
                last_updated_round_id: event.authority.round_id.clone(),
                status: mutation.status.clone(),
                summary: mutation.summary.clone(),
                supporting_evidence: Vec::new(),
            })
        })
        .collect::<Result<Vec<_>, DebugHypothesisLedgerError>>()?;
    Ok(DebugHypothesisLedgerSeed {
        authority: event.authority.clone(),
        created_at: event.occurred_at.clone(),
        hypotheses,
        unresolved_questions: event.opened_questions.clone(),
    })
}

fn update_from_event(
    event: &DebugHypothesisLedgerEvent,
) -> Result<DebugHypothesisLedgerUpdate, DebugHypothesisLedgerError> {
    if event.schema_version != 1
        || event.kind != DebugHypothesisLedgerEventKind::RoundApplied
        || derive_debug_hypothesis_ledger_event_digest(event)? != event.event_digest
    {
        return Err(replay_conflict(
            "Applied Ledger event digest or kind is invalid",
        ));
    }
    Ok(DebugHypothesisLedgerUpdate {
        confirmed_facts: event.confirmed_facts.clone(),
        mutations: event.mutations.clone(),
        occurred_at: event.occurred_at.clone(),
        opened_questions: event.opened_questions.clone(),
        previous_ledger_digest: event
            .previous_ledger_digest
            .clone()
            .ok_or_else(|| replay_conflict("Applied event has no previous Ledger digest"))?,
        reproduction_recipe_update: event.reproduction_recipe_update.clone(),
        resolved_question_digests: event.resolved_question_digests.clone(),
        session_status: event.session_status.clone(),
        source_context_digest: event
            .source_context_digest
            .clone()
            .ok_or_else(|| replay_conflict("Applied event has no source context digest"))?,
        source_request_digest: event
            .source_request_digest
            .clone()
            .ok_or_else(|| replay_conflict("Applied event has no source request digest"))?,
        source_round_receipt: event
            .source_round_receipt
            .clone()
            .ok_or_else(|| replay_conflict("Applied event has no source round receipt"))?,
    })
}

fn event_matches_update(
    event: &DebugHypothesisLedgerEvent,
    update: &DebugHypothesisLedgerUpdate,
) -> bool {
    event.kind == DebugHypothesisLedgerEventKind::RoundApplied
        && event.authority == update.source_round_receipt.authority
        && event.previous_ledger_digest.as_ref() == Some(&update.previous_ledger_digest)
        && event.source_round_receipt.as_ref() == Some(&update.source_round_receipt)
        && event.source_context_digest.as_ref() == Some(&update.source_context_digest)
        && event.source_request_digest.as_ref() == Some(&update.source_request_digest)
        && event.mutations == update.mutations
        && event.confirmed_facts == update.confirmed_facts
        && event.reproduction_recipe_update == update.reproduction_recipe_update
        && event.opened_questions == update.opened_questions
        && event.resolved_question_digests == update.resolved_question_digests
        && event.session_status == update.session_status
        && event.occurred_at == update.occurred_at
}

fn validate_session_scope(
    previous: &DebugProbeRoundAuthority,
    current: &DebugProbeRoundAuthority,
) -> Result<(), DebugHypothesisLedgerError> {
    if previous.debug_session_id != current.debug_session_id
        || previous.session_identity != current.session_identity
        || previous.repository_id != current.repository_id
        || previous.workspace_revision != current.workspace_revision
        || previous.environment_digest != current.environment_digest
    {
        return Err(stale(
            "Probe evidence session, repository, revision or environment is stale",
        ));
    }
    Ok(())
}

fn validate_round_evidence_cut(
    cut: &DebugHypothesisRoundEvidence,
    plan: &ValidatedDebugProbePlan,
) -> Result<(), DebugHypothesisLedgerError> {
    if cut.schema_version != 1
        || cut.evidence_summaries.len() > 32
        || cut.evidence_candidates.len() > 512
    {
        return Err(invalid("Hypothesis round evidence bounds are invalid"));
    }
    if cut.evidence_cut_digest != derive_debug_hypothesis_round_evidence_digest(cut)? {
        return Err(digest("Hypothesis round evidence cut digest is invalid"));
    }
    validate_probe_round_receipt(&cut.receipt, plan)
        .map_err(|_| invalid("Hypothesis round evidence receipt is invalid"))?;
    let reference = &cut.source_round_receipt;
    if reference.authority != cut.receipt.authority
        || reference.plan_digest != cut.receipt.plan_digest
        || reference.receipt_digest != derive_probe_round_receipt_digest(&cut.receipt)?
    {
        return Err(digest(
            "Hypothesis round evidence receipt reference is invalid",
        ));
    }
    validate_artifact_reference(&reference.receipt_artifact_ref)?;
    if sha256_bytes(&canonical_probe_round_receipt_bytes(&cut.receipt)?)
        != reference.receipt_artifact_ref.digest
    {
        return Err(digest(
            "Hypothesis round evidence receipt Artifact digest is invalid",
        ));
    }
    if !is_summaries_canonical(&cut.evidence_summaries)
        || !is_candidates_canonical(&cut.evidence_candidates)
    {
        return Err(invalid(
            "Hypothesis round evidence collection order is not canonical",
        ));
    }
    let receipt_identities = expected_evidence_identity_set(&cut.receipt);
    let summary_identities = cut
        .evidence_summaries
        .iter()
        .map(|value| value.identity.probe_execution_id.0.clone())
        .collect::<BTreeSet<_>>();
    if summary_identities.len() != cut.evidence_summaries.len()
        || summary_identities != receipt_identities
    {
        return Err(missing_evidence(
            "Hypothesis round evidence summaries do not exactly cover the receipt",
        ));
    }
    for summary in &cut.evidence_summaries {
        if !receipt_identities.contains(&summary.identity.probe_execution_id.0) {
            return Err(invalid(
                "Hypothesis evidence summary is outside the source receipt",
            ));
        }
    }
    let mut expected_candidates = Vec::new();
    for summary in &cut.evidence_summaries {
        let probe = plan
            .probe_by_id(&summary.identity.probe_id)
            .ok_or_else(|| invalid("Hypothesis evidence summary probe is unknown"))?;
        for target in &probe.spec().target_hypothesis_ids {
            expected_candidates.push(HypothesisEvidenceCandidate {
                evidence: DebugHypothesisEvidence {
                    artifact_ref: summary.bundle_artifact_ref.clone(),
                    evidence_digest: derive_hypothesis_evidence_digest(
                        &summary.identity,
                        target,
                        &summary.bundle_digest,
                    )
                    .map_err(|_| digest("Hypothesis evidence candidate digest is invalid"))?,
                    identity: summary.identity.clone(),
                    recorded_at: summary.recorded_at.clone(),
                },
                target_hypothesis_id: target.clone(),
            });
        }
    }
    expected_candidates
        .sort_by(|left, right| candidate_sort_key(left).cmp(&candidate_sort_key(right)));
    if cut.evidence_candidates != expected_candidates {
        return Err(missing_evidence(
            "Hypothesis evidence candidate set is not the exact D3 projection cut",
        ));
    }
    Ok(())
}

fn expected_evidence_identity_set(receipt: &ProbeRoundReceipt) -> BTreeSet<String> {
    receipt
        .probe_receipts
        .iter()
        .filter(|value| !value.artifact_refs.is_empty() || value.output_bytes != 0)
        .map(|value| value.identity.probe_execution_id.0.clone())
        .collect()
}

fn validate_authority(
    authority: &DebugProbeRoundAuthority,
) -> Result<(), DebugHypothesisLedgerError> {
    validate_debug_probe_round_authority(authority, authority)
        .map_err(|_| invalid("DebugProbe round authority is invalid"))
}

fn validate_artifact_reference(
    reference: &ArtifactReference,
) -> Result<(), DebugHypothesisLedgerError> {
    if !prefixed_ulid(&reference.artifact_id.0, "art_") {
        return Err(invalid("Artifact reference identity is invalid"));
    }
    validate_sha256(&reference.digest)
}

fn validate_hypothesis_id(value: &DebugHypothesisId) -> Result<(), DebugHypothesisLedgerError> {
    if prefixed_ulid(&value.0, "hyp_") {
        Ok(())
    } else {
        Err(invalid("Hypothesis identity is invalid"))
    }
}

fn prefixed_ulid(value: &str, prefix: &str) -> bool {
    value.len() == prefix.len() + 26
        && value.starts_with(prefix)
        && value[prefix.len()..].bytes().all(|byte| {
            byte.is_ascii_digit()
                || matches!(
                    byte,
                    b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z'
                )
        })
}

fn validate_sha256(value: &Sha256Digest) -> Result<(), DebugHypothesisLedgerError> {
    let Some(hex) = value.0.strip_prefix("sha256:") else {
        return Err(digest("SHA-256 digest is invalid"));
    };
    if hex.len() == 64
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Ok(())
    } else {
        Err(digest("SHA-256 digest is invalid"))
    }
}

fn validate_summary(value: &str) -> Result<(), DebugHypothesisLedgerError> {
    let count = value.chars().count();
    if (1..=500).contains(&count)
        && !value
            .chars()
            .any(|character| matches!(character, '\0' | '\r' | '\n'))
    {
        Ok(())
    } else {
        Err(invalid("Ledger summary is empty, multiline or too long"))
    }
}

fn contains_evidence(
    values: &[DebugHypothesisEvidence],
    expected: &DebugHypothesisEvidence,
) -> bool {
    values.iter().any(|value| value == expected)
}

fn candidate_key(
    candidate: &HypothesisEvidenceCandidate,
) -> Result<String, DebugHypothesisLedgerError> {
    serde_json::to_string(candidate)
        .map_err(|_| serialization("Hypothesis evidence candidate serialization failed"))
}

fn canonicalize_update(update: &mut DebugHypothesisLedgerUpdate) {
    update
        .mutations
        .sort_by(|left, right| left.hypothesis_id.0.cmp(&right.hypothesis_id.0));
    for mutation in &mut update.mutations {
        mutation
            .assessments
            .sort_by(|left, right| assessment_sort_key(left).cmp(&assessment_sort_key(right)));
    }
    for fact in &mut update.confirmed_facts {
        fact.evidence
            .sort_by(|left, right| candidate_sort_key(left).cmp(&candidate_sort_key(right)));
    }
    update
        .confirmed_facts
        .sort_by(|left, right| left.fact_digest.0.cmp(&right.fact_digest.0));
    if let Some(recipe) = &mut update.reproduction_recipe_update {
        recipe
            .recipe
            .evidence
            .sort_by(|left, right| candidate_sort_key(left).cmp(&candidate_sort_key(right)));
    }
    update
        .opened_questions
        .sort_by(|left, right| left.question_digest.0.cmp(&right.question_digest.0));
    update
        .resolved_question_digests
        .sort_by(|left, right| left.0.cmp(&right.0));
}

fn assessment_sort_key(value: &DebugHypothesisEvidenceAssessment) -> (&str, &str, u8) {
    (
        &value.candidate.target_hypothesis_id.0,
        &value.candidate.evidence.evidence_digest.0,
        match value.polarity {
            DebugHypothesisEvidencePolarity::Supports => 0,
            DebugHypothesisEvidencePolarity::Contradicts => 1,
        },
    )
}

fn canonicalize_ledger_collections(ledger: &mut DebugHypothesisLedger) {
    canonicalize_hypotheses(&mut ledger.hypotheses);
    for fact in &mut ledger.confirmed_facts {
        fact.evidence
            .sort_by(|left, right| candidate_sort_key(left).cmp(&candidate_sort_key(right)));
    }
    ledger
        .confirmed_facts
        .sort_by(|left, right| left.fact_digest.0.cmp(&right.fact_digest.0));
    if let Some(recipe) = &mut ledger.reproduction_recipe {
        recipe
            .evidence
            .sort_by(|left, right| candidate_sort_key(left).cmp(&candidate_sort_key(right)));
    }
    canonicalize_questions(&mut ledger.unresolved_questions);
}

fn canonicalize_hypotheses(values: &mut [DebugHypothesis]) {
    for value in values.iter_mut() {
        canonicalize_evidence(&mut value.supporting_evidence);
        canonicalize_evidence(&mut value.contradicting_evidence);
    }
    values.sort_by(|left, right| left.hypothesis_id.0.cmp(&right.hypothesis_id.0));
}

fn canonicalize_evidence(values: &mut [DebugHypothesisEvidence]) {
    values.sort_by(|left, right| {
        (&left.evidence_digest.0, &left.artifact_ref.artifact_id.0)
            .cmp(&(&right.evidence_digest.0, &right.artifact_ref.artifact_id.0))
    });
}

fn canonicalize_questions(values: &mut [DebugUnresolvedQuestion]) {
    values.sort_by(|left, right| left.question_digest.0.cmp(&right.question_digest.0));
}

fn canonicalize_evidence_cut(
    summaries: &mut [ProbeEvidenceSummary],
    candidates: &mut [HypothesisEvidenceCandidate],
) {
    summaries.sort_by(|left, right| {
        left.identity
            .probe_execution_id
            .0
            .cmp(&right.identity.probe_execution_id.0)
    });
    candidates.sort_by(|left, right| candidate_sort_key(left).cmp(&candidate_sort_key(right)));
}

fn candidate_sort_key(value: &HypothesisEvidenceCandidate) -> (&str, &str, &str) {
    (
        &value.target_hypothesis_id.0,
        &value.evidence.evidence_digest.0,
        &value.evidence.artifact_ref.artifact_id.0,
    )
}

fn is_summaries_canonical(values: &[ProbeEvidenceSummary]) -> bool {
    values
        .windows(2)
        .all(|pair| pair[0].identity.probe_execution_id.0 < pair[1].identity.probe_execution_id.0)
}

fn is_candidates_canonical(values: &[HypothesisEvidenceCandidate]) -> bool {
    values
        .windows(2)
        .all(|pair| candidate_sort_key(&pair[0]) < candidate_sort_key(&pair[1]))
}

fn is_hypotheses_canonical(values: &[DebugHypothesis]) -> bool {
    values
        .windows(2)
        .all(|pair| pair[0].hypothesis_id.0 < pair[1].hypothesis_id.0)
        && values.iter().all(|value| {
            value
                .supporting_evidence
                .windows(2)
                .all(|pair| evidence_sort_key(&pair[0]) < evidence_sort_key(&pair[1]))
                && value
                    .contradicting_evidence
                    .windows(2)
                    .all(|pair| evidence_sort_key(&pair[0]) < evidence_sort_key(&pair[1]))
        })
}

fn evidence_sort_key(value: &DebugHypothesisEvidence) -> (&str, &str) {
    (&value.evidence_digest.0, &value.artifact_ref.artifact_id.0)
}

fn is_facts_canonical(values: &[DebugConfirmedFact]) -> bool {
    values
        .windows(2)
        .all(|pair| pair[0].fact_digest.0 < pair[1].fact_digest.0)
}

fn is_questions_canonical(values: &[DebugUnresolvedQuestion]) -> bool {
    values
        .windows(2)
        .all(|pair| pair[0].question_digest.0 < pair[1].question_digest.0)
}

fn ensure_unique_by<'a>(
    values: impl Iterator<Item = &'a String>,
    message: &'static str,
) -> Result<(), DebugHypothesisLedgerError> {
    let mut seen = BTreeSet::new();
    for value in values {
        if !seen.insert(value) {
            return Err(invalid(message));
        }
    }
    Ok(())
}

struct FramedDigest(Sha256);

impl FramedDigest {
    fn new(domain: &[u8]) -> Self {
        let mut value = Sha256::new();
        value.update(domain);
        Self(value)
    }

    fn json<T: Serialize + ?Sized>(
        &mut self,
        name: &[u8],
        value: &T,
    ) -> Result<(), DebugHypothesisLedgerError> {
        let bytes = serde_json::to_vec(value)
            .map_err(|_| serialization("Ledger digest serialization failed"))?;
        self.bytes(name);
        self.bytes(&bytes);
        Ok(())
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.0.update((bytes.len() as u64).to_be_bytes());
        self.0.update(bytes);
    }

    fn finish(self) -> Sha256Digest {
        Sha256Digest(format!("sha256:{:x}", self.0.finalize()))
    }
}

fn sha256_bytes(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn zero_digest() -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", "0".repeat(64)))
}

const fn error(
    kind: DebugHypothesisLedgerErrorKind,
    message: &'static str,
) -> DebugHypothesisLedgerError {
    DebugHypothesisLedgerError { kind, message }
}

const fn invalid(message: &'static str) -> DebugHypothesisLedgerError {
    error(DebugHypothesisLedgerErrorKind::InvalidInput, message)
}

const fn invalid_seed(message: &'static str) -> DebugHypothesisLedgerError {
    error(DebugHypothesisLedgerErrorKind::InvalidSeed, message)
}

const fn invalid_transition(message: &'static str) -> DebugHypothesisLedgerError {
    error(DebugHypothesisLedgerErrorKind::InvalidTransition, message)
}

const fn missing_evidence(message: &'static str) -> DebugHypothesisLedgerError {
    error(
        DebugHypothesisLedgerErrorKind::MissingCurrentEvidence,
        message,
    )
}

const fn evidence_conflict(message: &'static str) -> DebugHypothesisLedgerError {
    error(DebugHypothesisLedgerErrorKind::EvidenceConflict, message)
}

const fn stale(message: &'static str) -> DebugHypothesisLedgerError {
    error(DebugHypothesisLedgerErrorKind::StaleAuthority, message)
}

const fn replay_conflict(message: &'static str) -> DebugHypothesisLedgerError {
    error(DebugHypothesisLedgerErrorKind::ReplayConflict, message)
}

const fn sequence(message: &'static str) -> DebugHypothesisLedgerError {
    error(DebugHypothesisLedgerErrorKind::SequenceGap, message)
}

const fn digest(message: &'static str) -> DebugHypothesisLedgerError {
    error(DebugHypothesisLedgerErrorKind::InvalidDigest, message)
}

const fn serialization(message: &'static str) -> DebugHypothesisLedgerError {
    error(DebugHypothesisLedgerErrorKind::Serialization, message)
}
