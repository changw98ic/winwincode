// SPDX-License-Identifier: Apache-2.0
//!
//! ADR-0037 P6–P11: cross review, evidence-only JEV verdict, tool providers,
//! per-claim budget, dynamic rounds, metrics, and replayable UI trace.

use std::sync::Arc;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::fusion_knowledge::{
    ClaimNode, ClaimState, EvidenceDirection, EvidenceStrength, FusionEvidenceRecord,
};
use crate::fusion_planner::{
    DefaultEvidencePlanner, EvidenceLevel, EvidencePlanner, EvidenceProvider, InvestigationAction,
    InvestigationPlan, ProviderCapability, plan_investigation,
};

// ---------------------------------------------------------------------------
// P6 Cross Review
// ---------------------------------------------------------------------------

/// Blind review of one evidence item. Reviewer never sees the source model name.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrossReviewRequest {
    pub claim_display_key: String,
    pub claim_summary: String,
    pub evidence: Vec<FusionEvidenceRecord>,
    /// Must not include model/provider brand priors beyond evidence.provider id.
    pub hide_provider_names: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CrossReviewOutcome {
    Accept,
    Reject,
    NeedsTool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrossReviewResult {
    pub outcome: CrossReviewOutcome,
    pub accepted_evidence_ids: Vec<String>,
    pub rejected_evidence_ids: Vec<String>,
    pub notes: String,
}

pub trait CrossReviewer: Send + Sync + std::fmt::Debug {
    fn review(
        &self,
        request: CrossReviewRequest,
    ) -> BoxFuture<'static, Result<CrossReviewResult, String>>;
}

/// Default policy: accept DIRECT verified facts; escalate weak/speculative items.
#[derive(Debug, Default, Clone, Copy)]
pub struct ConservativeCrossReviewer;

impl CrossReviewer for ConservativeCrossReviewer {
    fn review(
        &self,
        request: CrossReviewRequest,
    ) -> BoxFuture<'static, Result<CrossReviewResult, String>> {
        Box::pin(async move {
            let mut accepted = Vec::new();
            let mut rejected = Vec::new();
            let mut needs_tool = false;
            for record in &request.evidence {
                if record.invalidated {
                    rejected.push(record.id.clone());
                    continue;
                }
                match (record.verified, record.strength) {
                    (true, EvidenceStrength::Direct) => accepted.push(record.id.clone()),
                    (false, EvidenceStrength::Speculation | EvidenceStrength::WeakInference) => {
                        needs_tool = true;
                        rejected.push(record.id.clone());
                    }
                    _ => needs_tool = true,
                }
            }
            let outcome = if !rejected.is_empty() && accepted.is_empty() {
                if needs_tool {
                    CrossReviewOutcome::NeedsTool
                } else {
                    CrossReviewOutcome::Reject
                }
            } else if needs_tool {
                CrossReviewOutcome::NeedsTool
            } else {
                CrossReviewOutcome::Accept
            };
            Ok(CrossReviewResult {
                outcome,
                accepted_evidence_ids: accepted,
                rejected_evidence_ids: rejected,
                notes: "conservative policy: verified DIRECT accepted; speculation escalated"
                    .to_owned(),
            })
        })
    }
}

// ---------------------------------------------------------------------------
// P7 JEV evidence-only verifier
// ---------------------------------------------------------------------------

/// JEV sees evidence packs only — never vote counts or model brand names.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceOnlyVerdictRequest {
    pub claim_display_key: String,
    pub claim_summary: String,
    pub verified_support: Vec<String>,
    pub verified_counter: Vec<String>,
    pub unverified_support: Vec<String>,
    pub unverified_counter: Vec<String>,
    pub assumptions: Vec<String>,
    pub unknowns: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvidenceVerdict {
    Confirmed,
    Refuted,
    Insufficient,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceOnlyVerdict {
    pub verdict: EvidenceVerdict,
    pub support_evidence: Vec<String>,
    pub rejected_evidence: Vec<String>,
    pub remaining_unknowns: Vec<String>,
    pub confidence: &'static str,
}

pub trait EvidenceVerifier: Send + Sync + std::fmt::Debug {
    fn verify(
        &self,
        request: EvidenceOnlyVerdictRequest,
    ) -> BoxFuture<'static, Result<EvidenceOnlyVerdict, String>>;
}

/// Policy verifier implementing ADR-0037 hard rules (no LLM required).
#[derive(Debug, Default, Clone, Copy)]
pub struct PolicyEvidenceVerifier;

impl EvidenceVerifier for PolicyEvidenceVerifier {
    fn verify(
        &self,
        request: EvidenceOnlyVerdictRequest,
    ) -> BoxFuture<'static, Result<EvidenceOnlyVerdict, String>> {
        Box::pin(async move {
            // REFUTED requires verified counter-evidence (never vote counts).
            if !request.verified_counter.is_empty() && request.verified_support.is_empty() {
                return Ok(EvidenceOnlyVerdict {
                    verdict: EvidenceVerdict::Refuted,
                    support_evidence: Vec::new(),
                    rejected_evidence: request.unverified_support,
                    remaining_unknowns: request.unknowns,
                    confidence: "high",
                });
            }
            if !request.verified_support.is_empty() && request.verified_counter.is_empty() {
                let counter_empty = request.unverified_counter.is_empty();
                return Ok(EvidenceOnlyVerdict {
                    verdict: EvidenceVerdict::Confirmed,
                    support_evidence: request.verified_support,
                    rejected_evidence: request.unverified_counter,
                    remaining_unknowns: request.unknowns,
                    confidence: if counter_empty { "high" } else { "medium" },
                });
            }
            if !request.verified_support.is_empty() && !request.verified_counter.is_empty() {
                return Ok(EvidenceOnlyVerdict {
                    verdict: EvidenceVerdict::Insufficient,
                    support_evidence: request.verified_support,
                    rejected_evidence: Vec::new(),
                    remaining_unknowns: request.unknowns,
                    confidence: "low",
                });
            }
            Ok(EvidenceOnlyVerdict {
                verdict: EvidenceVerdict::Insufficient,
                support_evidence: request.unverified_support,
                rejected_evidence: request.unverified_counter,
                remaining_unknowns: request.unknowns,
                confidence: "low",
            })
        })
    }
}

/// Strip brand/model names before verifier sees the pack (ADR-0037 §R4).
#[must_use]
pub fn build_evidence_only_request(
    claim: &ClaimNode,
    store: &[FusionEvidenceRecord],
) -> EvidenceOnlyVerdictRequest {
    let mut verified_support = Vec::new();
    let mut verified_counter = Vec::new();
    let mut unverified_support = Vec::new();
    let mut unverified_counter = Vec::new();
    for record in store
        .iter()
        .filter(|r| r.claim_id == claim.id && !r.invalidated)
    {
        let facts = record.facts.clone();
        match (record.direction, record.verified) {
            (EvidenceDirection::Support, true) => verified_support.extend(facts),
            (EvidenceDirection::Counter, true) => verified_counter.extend(facts),
            (EvidenceDirection::Support, false) => unverified_support.extend(facts),
            (EvidenceDirection::Counter, false) => unverified_counter.extend(facts),
        }
    }
    EvidenceOnlyVerdictRequest {
        claim_display_key: claim.display_key.clone(),
        claim_summary: claim.summary.clone(),
        verified_support,
        verified_counter,
        unverified_support,
        unverified_counter,
        assumptions: Vec::new(),
        unknowns: claim.unknowns.clone(),
    }
}

// ---------------------------------------------------------------------------
// P8 additional providers (git / test / ast stubs behind one trait)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct GitEvidenceProvider;
#[derive(Debug, Clone, Copy)]
pub struct TestEvidenceProvider;
#[derive(Debug, Clone, Copy)]
pub struct AstEvidenceProvider;

impl EvidenceProvider for GitEvidenceProvider {
    fn provider_id(&self) -> &'static str {
        "git"
    }
    fn can_handle(&self, action: &InvestigationAction) -> bool {
        action.provider == "git"
    }
    fn investigate(
        &self,
        action: &InvestigationAction,
        claim: &ClaimNode,
    ) -> BoxFuture<'static, Result<Vec<FusionEvidenceRecord>, String>> {
        let capability = action.capability.clone();
        let claim_id = claim.id.clone();
        let independence_group = format!("ig:git:{capability}");
        Box::pin(async move {
            // Host backends can replace this placeholder via a custom provider.
            Ok(vec![FusionEvidenceRecord {
                id: format!("ev_git_{claim_id}_{capability}"),
                claim_id,
                provider: "git".to_owned(),
                direction: EvidenceDirection::Support,
                kind: capability,
                strength: EvidenceStrength::WeakInference,
                facts: vec!["git provider backend not bound; treat as queue item".to_owned()],
                source_refs: vec!["git:pending".to_owned()],
                independence_group,
                verified: false,
                invalidated: false,
            }])
        })
    }
}

impl EvidenceProvider for TestEvidenceProvider {
    fn provider_id(&self) -> &'static str {
        "test"
    }
    fn can_handle(&self, action: &InvestigationAction) -> bool {
        action.provider == "test"
    }
    fn investigate(
        &self,
        action: &InvestigationAction,
        claim: &ClaimNode,
    ) -> BoxFuture<'static, Result<Vec<FusionEvidenceRecord>, String>> {
        let capability = action.capability.clone();
        let claim_id = claim.id.clone();
        let independence_group = format!("ig:test:{capability}");
        Box::pin(async move {
            Ok(vec![FusionEvidenceRecord {
                id: format!("ev_test_{claim_id}_{capability}"),
                claim_id,
                provider: "test".to_owned(),
                direction: EvidenceDirection::Support,
                kind: capability,
                strength: EvidenceStrength::WeakInference,
                facts: vec!["test provider backend not bound; treat as queue item".to_owned()],
                source_refs: vec!["test:pending".to_owned()],
                independence_group,
                verified: false,
                invalidated: false,
            }])
        })
    }
}

impl EvidenceProvider for AstEvidenceProvider {
    fn provider_id(&self) -> &'static str {
        "ast"
    }
    fn can_handle(&self, action: &InvestigationAction) -> bool {
        action.provider == "ast" || action.provider == "lsp"
    }
    fn investigate(
        &self,
        action: &InvestigationAction,
        claim: &ClaimNode,
    ) -> BoxFuture<'static, Result<Vec<FusionEvidenceRecord>, String>> {
        let capability = action.capability.clone();
        let claim_id = claim.id.clone();
        let independence_group = format!("ig:ast:{capability}");
        Box::pin(async move {
            Ok(vec![FusionEvidenceRecord {
                id: format!("ev_ast_{claim_id}_{capability}"),
                claim_id,
                provider: "ast".to_owned(),
                direction: EvidenceDirection::Support,
                kind: capability,
                strength: EvidenceStrength::WeakInference,
                facts: vec!["ast/lsp provider backend not bound; treat as queue item".to_owned()],
                source_refs: vec!["ast:pending".to_owned()],
                independence_group,
                verified: false,
                invalidated: false,
            }])
        })
    }
}

// ---------------------------------------------------------------------------
// P9 Budget + dynamic rounds
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvestigationBudget {
    pub max_llm_rounds_per_claim: Option<u8>,
    pub max_tool_escalation: Option<u8>,
    pub max_test_attempts: Option<u8>,
}

impl InvestigationBudget {
    fn limit_reached(limit: Option<u8>, used: u32) -> bool {
        limit.is_some_and(|limit| used >= u32::from(limit))
    }
}

/// Priority ∝ importance × uncertainty × `expected_information_gain` (ordinal).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum ClaimPriority {
    Low,
    Medium,
    High,
    Critical,
}

#[must_use]
pub fn claim_priority(claim: &ClaimNode) -> ClaimPriority {
    let disputed = matches!(
        claim.state,
        ClaimState::Disputed | ClaimState::Investigating | ClaimState::Escalated
    );
    if !disputed {
        return ClaimPriority::Low;
    }
    match (
        claim.supporter_count + claim.opponent_count,
        claim.has_verified_counter,
    ) {
        (n, _) if n >= 4 => ClaimPriority::Critical,
        (n, true) if n >= 2 => ClaimPriority::High,
        (n, false) if n >= 2 => ClaimPriority::High,
        _ => ClaimPriority::Medium,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimRoundTrace {
    pub round: u8,
    pub action: String,
    pub provider: String,
    pub produced_new_evidence: bool,
    pub outcome: crate::fusion_planner::InvestigationOutcome,
    pub executed: bool,
    pub failure_reason: Option<String>,
    pub state_after: ClaimState,
}

/// Non-destructive JEV or verifier outage retained in the investigation report.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JevUnavailableRecord {
    pub code: String,
    pub claim_id: String,
    pub detail: String,
}

/// Terminal condition of one investigation (P0-7) — not the strategy name.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TerminalReason {
    Confirmed,
    Refuted,
    ConsensusConverged,
    EvidenceConverged,
    BudgetExhausted,
    Unresolvable,
    NoActionableConflict,
    AttentionRequired,
}

impl std::fmt::Display for TerminalReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Confirmed => "CONFIRMED",
            Self::Refuted => "REFUTED",
            Self::ConsensusConverged => "CONSENSUS_CONVERGED",
            Self::EvidenceConverged => "EVIDENCE_CONVERGED",
            Self::BudgetExhausted => "BUDGET_EXHAUSTED",
            Self::Unresolvable => "UNRESOLVABLE",
            Self::NoActionableConflict => "NO_ACTIONABLE_CONFLICT",
            Self::AttentionRequired => "ATTENTION_REQUIRED",
        })
    }
}

// ---------------------------------------------------------------------------
// P10 metrics + P11 UI trace
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FusionMetrics {
    pub best_fixed_single_correct: usize,
    pub oracle_union_correct: usize,
    pub fusion_correct: usize,
    pub total_claims: usize,
    pub minority_truth_recovered: usize,
    pub false_consensus: usize,
    pub evidence_yield: usize,
    pub evidence_verified: usize,
    pub unresolved_claims: usize,
}

impl FusionMetrics {
    /// Ratio for report display only; sub-ULP precision on counts is irrelevant.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn oracle_capture_rate(&self) -> Option<f64> {
        if self.oracle_union_correct == 0 {
            return None;
        }
        Some(self.fusion_correct as f64 / self.oracle_union_correct as f64)
    }

    #[must_use]
    pub fn fusion_gain(&self) -> i64 {
        i64::try_from(self.fusion_correct).unwrap_or(i64::MAX)
            - i64::try_from(self.best_fixed_single_correct).unwrap_or(i64::MAX)
    }

    #[must_use]
    pub fn fusion_regret(&self) -> i64 {
        // Errors Fusion introduced beyond the best fixed single baseline.
        let gain = self.fusion_gain();
        if gain < 0 { -gain } else { 0 }
    }
}

/// Replayable claim trace for UI (ADR-0037 §UI Trace).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimTrace {
    pub display_key: String,
    pub state: ClaimState,
    pub supporters: Vec<String>,
    pub opponents: Vec<String>,
    pub evidence_facts: Vec<String>,
    pub rounds: Vec<ClaimRoundTrace>,
    pub verdict: Option<EvidenceVerdict>,
}

#[must_use]
pub fn build_ui_trace(
    claim: &ClaimNode,
    store: &[FusionEvidenceRecord],
    rounds: Vec<ClaimRoundTrace>,
    verdict: Option<EvidenceVerdict>,
) -> ClaimTrace {
    ClaimTrace {
        display_key: claim.display_key.clone(),
        state: claim.state,
        supporters: claim.supporters.clone(),
        opponents: claim.opponents.clone(),
        evidence_facts: store
            .iter()
            .filter(|record| record.claim_id == claim.id)
            .flat_map(|record| record.facts.clone())
            .collect(),
        rounds,
        verdict,
    }
}

// ---------------------------------------------------------------------------
// Dynamic investigation loop (P9 + P4–P8 glue)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvestigationRunReport {
    pub claim_id: String,
    pub priority: ClaimPriority,
    pub rounds: Vec<ClaimRoundTrace>,
    pub plan: InvestigationPlan,
    pub verdict: Option<EvidenceVerdict>,
    pub stop_reason: TerminalReason,
    pub provider_executions: Vec<crate::fusion_planner::ProviderExecutionRecord>,
    pub jev_unavailable: Vec<JevUnavailableRecord>,
}

/// One claim-level dynamic loop: plan → acquire → cross-review → verify → recompute.
/// Runs one plan step and merges its evidence (additive by default).
///
/// Returns the recorded outcome, any provider failure, and whether the store
/// grew. An empty `find_sync` search is still recorded as verified evidence:
/// absence of synchronization is itself a fact (`NEGATIVE_FINDING`).
async fn acquire_step_evidence(
    claim: &mut ClaimNode,
    store: &mut Vec<FusionEvidenceRecord>,
    providers: &[Arc<dyn EvidenceProvider>],
    action: &InvestigationAction,
) -> (
    crate::fusion_planner::InvestigationOutcome,
    Option<String>,
    bool,
) {
    use crate::fusion_planner::InvestigationOutcome;

    let before = store.len();
    let mut failure_reason = None;
    // Bound from the match so every path yields an outcome (no dead default).
    let outcome = match crate::fusion_planner::execute_plan_step(providers, action, claim).await {
        Ok(incoming) => {
            let produced = !incoming.is_empty();
            if !produced && action.capability.contains("find_sync") {
                crate::fusion_planner::merge_evidence(
                    claim,
                    store,
                    vec![FusionEvidenceRecord {
                        id: format!("ev_neg_{}_{}", claim.id, action.capability),
                        claim_id: claim.id.clone(),
                        provider: action.provider.clone(),
                        direction: EvidenceDirection::Support,
                        kind: "negative_finding".to_owned(),
                        strength: EvidenceStrength::StrongInference,
                        facts: vec![format!(
                            "NEGATIVE_FINDING: {} produced no protective structure",
                            action.capability
                        )],
                        source_refs: vec![format!("{}:{}", action.provider, action.capability)],
                        independence_group: format!(
                            "ig:neg:{}:{}",
                            action.provider, action.capability
                        ),
                        verified: true,
                        invalidated: false,
                    }],
                );
                InvestigationOutcome::NegativeFinding
            } else {
                crate::fusion_planner::merge_evidence(claim, store, incoming);
                if store.len() > before {
                    InvestigationOutcome::SupportingEvidence
                } else {
                    InvestigationOutcome::NoRelevantFinding
                }
            }
        }
        Err(error) => {
            failure_reason = Some(error);
            InvestigationOutcome::ProviderFailure
        }
    };
    (outcome, failure_reason, store.len() > before)
}

/// P6 blind cross review; rejected evidence is marked invalidated in place.
async fn cross_review_and_invalidate(
    claim: &ClaimNode,
    store: &mut [FusionEvidenceRecord],
    reviewer: &dyn CrossReviewer,
) -> CrossReviewResult {
    let pack: Vec<FusionEvidenceRecord> = store
        .iter()
        .filter(|record| record.claim_id == claim.id)
        .cloned()
        .collect();
    let review = reviewer
        .review(CrossReviewRequest {
            claim_display_key: claim.display_key.clone(),
            claim_summary: claim.summary.clone(),
            evidence: pack,
            hide_provider_names: true,
        })
        .await
        .unwrap_or(CrossReviewResult {
            outcome: CrossReviewOutcome::NeedsTool,
            accepted_evidence_ids: Vec::new(),
            rejected_evidence_ids: Vec::new(),
            notes: "reviewer unavailable".to_owned(),
        });
    for id in &review.rejected_evidence_ids {
        if let Some(record) = store.iter_mut().find(|record| record.id == *id) {
            record.invalidated = true;
        }
    }
    review
}

/// P7 evidence-only verification (no votes, no model names).
///
/// Returns the terminal marker when the ladder settles. `Refuted` is only
/// reachable with a verified counter (ADR-0037 hard rule).
enum VerificationSettlement {
    Settled(&'static str, EvidenceVerdict),
    Insufficient,
    Unavailable(JevUnavailableRecord),
}

async fn settle_with_evidence_verdict(
    claim: &mut ClaimNode,
    store: &[FusionEvidenceRecord],
    verifier: &dyn EvidenceVerifier,
) -> VerificationSettlement {
    let request = build_evidence_only_request(claim, store);
    let result = match verifier.verify(request).await {
        Ok(result) => result,
        Err(error) => {
            return VerificationSettlement::Unavailable(JevUnavailableRecord {
                code: jev_unavailable_code(&error),
                claim_id: claim.id.clone(),
                detail: error,
            });
        }
    };
    match result.verdict {
        EvidenceVerdict::Confirmed => {
            claim.state = ClaimState::Confirmed;
            VerificationSettlement::Settled("CONFIRMED", EvidenceVerdict::Confirmed)
        }
        EvidenceVerdict::Refuted if claim.has_verified_counter => {
            claim.state = ClaimState::Refuted;
            VerificationSettlement::Settled("REFUTED", EvidenceVerdict::Refuted)
        }
        _ => {
            claim.state = crate::fusion_planner::recompute_state(claim);
            VerificationSettlement::Insufficient
        }
    }
}

pub(crate) fn jev_unavailable_code(error: &str) -> String {
    let normalized = error.to_ascii_uppercase();
    if normalized.contains("TIMEOUT") || normalized.contains("RESOURCE_EXHAUSTED") {
        "JEV_TIMEOUT".to_owned()
    } else if normalized.contains("INVALID_RESPONSE")
        || normalized.contains("DESERIALIZE")
        || normalized.contains("SCHEMA")
    {
        "JEV_INVALID_RESPONSE".to_owned()
    } else if normalized.contains("TRANSPORT")
        || normalized.contains("UNAVAILABLE")
        || normalized.contains("CONNECTION")
    {
        "JEV_TRANSPORT".to_owned()
    } else {
        "JEV_UNAVAILABLE".to_owned()
    }
}

fn requires_attention(error: &str) -> bool {
    let normalized = error.to_ascii_uppercase();
    [
        "PERMISSION",
        "DEPENDENCY",
        "MISSING",
        "MATERIAL",
        "INPUT_REQUIRED",
        "ATTENTION",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

#[allow(
    clippy::too_many_lines,
    reason = "Keep the investigation state transitions and terminal reasons together"
)]
pub async fn investigate_claim(
    claim: &mut ClaimNode,
    store: &mut Vec<FusionEvidenceRecord>,
    providers: &[Arc<dyn EvidenceProvider>],
    reviewer: &dyn CrossReviewer,
    verifier: &dyn EvidenceVerifier,
    planner: &dyn EvidencePlanner,
    budget: InvestigationBudget,
) -> InvestigationRunReport {
    let capabilities: Vec<ProviderCapability> = providers
        .iter()
        .map(|provider| ProviderCapability {
            provider: provider.provider_id().to_owned(),
            level: EvidenceLevel::CodeGraph,
            capability: "*".to_owned(),
            cost_rank: 1,
        })
        .collect();
    let plan = planner.plan(claim, &capabilities);
    let priority = claim_priority(claim);
    let mut rounds: Vec<ClaimRoundTrace> = Vec::new();
    let mut tool_escalation = 0u32;
    let mut llm_rounds = 0u32;
    let mut verdict = None;
    let mut stop_reason = None;
    let mut budget_exhausted = false;
    let mut attention_required = false;
    let mut jev_unavailable = Vec::new();

    claim.state = ClaimState::Investigating;
    for action in &plan.actions {
        if InvestigationBudget::limit_reached(budget.max_tool_escalation, tool_escalation) {
            budget_exhausted = true;
            break;
        }
        if action.level == EvidenceLevel::LlmReasoning {
            if InvestigationBudget::limit_reached(budget.max_llm_rounds_per_claim, llm_rounds) {
                budget_exhausted = true;
                continue;
            }
            llm_rounds += 1;
        }
        if action.level == EvidenceLevel::Test
            && InvestigationBudget::limit_reached(
                budget.max_test_attempts,
                u32::try_from(
                    rounds
                        .iter()
                        .filter(|round| round.action == action.capability)
                        .count(),
                )
                .unwrap_or(u32::MAX),
            )
        {
            budget_exhausted = true;
            continue;
        }

        let (outcome, failure_reason, produced_new_evidence) =
            acquire_step_evidence(claim, store, providers, action).await;
        attention_required |= failure_reason.as_deref().is_some_and(requires_attention);
        if produced_new_evidence {
            tool_escalation += 1;
        }

        let review = cross_review_and_invalidate(claim, store, reviewer).await;
        let settlement = settle_with_evidence_verdict(claim, store, verifier).await;

        rounds.push(ClaimRoundTrace {
            round: u8::try_from(rounds.len() + 1).unwrap_or(u8::MAX),
            action: action.capability.clone(),
            provider: action.provider.clone(),
            produced_new_evidence,
            outcome,
            executed: true,
            failure_reason,
            state_after: claim.state,
        });

        match settlement {
            VerificationSettlement::Settled(reason, found) => {
                stop_reason = Some(match reason {
                    "CONFIRMED" => TerminalReason::Confirmed,
                    "REFUTED" => TerminalReason::Refuted,
                    _ => TerminalReason::EvidenceConverged,
                });
                verdict = Some(found);
                break;
            }
            VerificationSettlement::Unavailable(record) => jev_unavailable.push(record),
            VerificationSettlement::Insufficient => {}
        }

        // A no-increment round never adds weight or stops the investigation.
        // The planner's next source/tool/reproduction action is still meaningful.
        let _ = review.outcome;
    }

    if verdict.is_none() {
        if attention_required || budget_exhausted {
            claim.state = ClaimState::Escalated;
        } else if claim.state == ClaimState::Investigating {
            claim.state = ClaimState::Unresolved;
        }
    }

    let terminal = stop_reason.unwrap_or({
        if attention_required {
            TerminalReason::AttentionRequired
        } else if budget_exhausted {
            TerminalReason::BudgetExhausted
        } else {
            TerminalReason::Unresolvable
        }
    });

    InvestigationRunReport {
        claim_id: claim.id.clone(),
        priority,
        rounds,
        plan,
        verdict,
        stop_reason: terminal,
        provider_executions: Vec::new(),
        jev_unavailable,
    }
}

/// Convenience: plan-only helper used by hosts before spending budget.
#[must_use]
pub fn plan_for_claim(claim: &ClaimNode) -> InvestigationPlan {
    plan_investigation(claim, &[])
}

#[must_use]
pub fn default_planner() -> DefaultEvidencePlanner {
    DefaultEvidencePlanner
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fusion_knowledge::ClaimIdentity;
    use crate::fusion_planner::EvidenceLevel;

    fn disputed() -> ClaimNode {
        ClaimNode {
            id: "c1".to_owned(),
            identity: ClaimIdentity::parse("root:shared-fixture-race"),
            display_key: "claim:root:shared-fixture-race".to_owned(),
            summary: "shared fixture race".to_owned(),
            state: ClaimState::Disputed,
            supporters: vec!["mimo".to_owned()],
            opponents: vec!["glm".to_owned()],
            evidence_ids: Vec::new(),
            counter_evidence_ids: Vec::new(),
            unknowns: Vec::new(),
            supporter_count: 1,
            opponent_count: 1,
            has_verified_counter: false,
        }
    }

    #[test]
    fn jev_verdict_refuted_requires_verified_counter() {
        let insufficient = futures::executor::block_on(PolicyEvidenceVerifier.verify(
            EvidenceOnlyVerdictRequest {
                claim_display_key: "x".to_owned(),
                claim_summary: "x".to_owned(),
                verified_support: Vec::new(),
                verified_counter: vec!["counter fact".to_owned()],
                unverified_support: vec!["guess".to_owned()],
                unverified_counter: Vec::new(),
                assumptions: Vec::new(),
                unknowns: Vec::new(),
            },
        ))
        .expect("verdict");
        // verified_counter alone is not enough if there is also unverified support?
        // Policy: verified_counter && verified_support empty → Refuted.
        assert_eq!(insufficient.verdict, EvidenceVerdict::Refuted);

        let no_counter = futures::executor::block_on(PolicyEvidenceVerifier.verify(
            EvidenceOnlyVerdictRequest {
                claim_display_key: "x".to_owned(),
                claim_summary: "x".to_owned(),
                verified_support: vec!["support".to_owned()],
                verified_counter: Vec::new(),
                unverified_support: Vec::new(),
                unverified_counter: vec!["opinion".to_owned()],
                assumptions: Vec::new(),
                unknowns: Vec::new(),
            },
        ))
        .expect("verdict");
        assert_eq!(no_counter.verdict, EvidenceVerdict::Confirmed);
    }

    #[test]
    fn cross_review_escalates_speculation() {
        let result =
            futures::executor::block_on(ConservativeCrossReviewer.review(CrossReviewRequest {
                claim_display_key: "x".to_owned(),
                claim_summary: "x".to_owned(),
                hide_provider_names: true,
                evidence: vec![FusionEvidenceRecord {
                    id: "ev1".to_owned(),
                    claim_id: "c1".to_owned(),
                    provider: "llm".to_owned(),
                    direction: EvidenceDirection::Support,
                    kind: "guess".to_owned(),
                    strength: EvidenceStrength::Speculation,
                    facts: vec!["maybe".to_owned()],
                    source_refs: vec!["llm".to_owned()],
                    independence_group: "ig".to_owned(),
                    verified: false,
                    invalidated: false,
                }],
            }))
            .expect("review");
        assert_eq!(result.outcome, CrossReviewOutcome::NeedsTool);
    }

    #[test]
    fn investigate_claim_runs_dynamic_loop_without_killing_minority() {
        let mut claim = disputed();
        let mut store = Vec::new();
        let providers: Vec<Arc<dyn EvidenceProvider>> = vec![Arc::new(TestEvidenceProvider)];
        let report = futures::executor::block_on(investigate_claim(
            &mut claim,
            &mut store,
            &providers,
            &ConservativeCrossReviewer,
            &PolicyEvidenceVerifier,
            &DefaultEvidencePlanner,
            InvestigationBudget::default(),
        ));
        assert!(!report.rounds.is_empty());
        assert_ne!(claim.state, ClaimState::Refuted);
        let _ = EvidenceLevel::CodeGraph;
    }

    #[derive(Debug)]
    struct UnavailableVerifier {
        code: &'static str,
    }

    impl EvidenceVerifier for UnavailableVerifier {
        fn verify(
            &self,
            _request: EvidenceOnlyVerdictRequest,
        ) -> BoxFuture<'static, Result<EvidenceOnlyVerdict, String>> {
            let code = self.code;
            Box::pin(async move { Err(code.to_owned()) })
        }
    }

    #[derive(Debug)]
    struct TwoStepPlanner;

    impl EvidencePlanner for TwoStepPlanner {
        fn plan(
            &self,
            claim: &ClaimNode,
            _capabilities: &[ProviderCapability],
        ) -> InvestigationPlan {
            InvestigationPlan {
                claim_id: claim.id.clone(),
                display_key: claim.display_key.clone(),
                modes: vec![
                    crate::fusion_planner::InvestigationMode::EvidenceExpansion,
                    crate::fusion_planner::InvestigationMode::CrossVerification,
                ],
                unknowns: claim.unknowns.clone(),
                actions: vec![
                    InvestigationAction {
                        provider: "independent".to_owned(),
                        level: EvidenceLevel::CodeGraph,
                        question: "read the current call graph".to_owned(),
                        capability: "source.read".to_owned(),
                        expected_information_gain: crate::fusion_planner::InformationGain::Medium,
                    },
                    InvestigationAction {
                        provider: "independent".to_owned(),
                        level: EvidenceLevel::Reproduction,
                        question: "run the targeted reproduction".to_owned(),
                        capability: "reproduction.run".to_owned(),
                        expected_information_gain: crate::fusion_planner::InformationGain::High,
                    },
                ],
            }
        }
    }

    #[derive(Debug)]
    struct TwoStepProvider;

    impl EvidenceProvider for TwoStepProvider {
        fn provider_id(&self) -> &'static str {
            "independent"
        }

        fn can_handle(&self, action: &InvestigationAction) -> bool {
            action.provider == self.provider_id()
        }

        fn investigate(
            &self,
            action: &InvestigationAction,
            claim: &ClaimNode,
        ) -> BoxFuture<'static, Result<Vec<FusionEvidenceRecord>, String>> {
            let increment = action.capability == "reproduction.run";
            let record = FusionEvidenceRecord {
                id: format!("ev_{}_{}", claim.id, action.capability),
                claim_id: claim.id.clone(),
                provider: self.provider_id().to_owned(),
                direction: EvidenceDirection::Support,
                kind: "reproduction".to_owned(),
                strength: EvidenceStrength::Direct,
                facts: vec!["targeted reproduction completed".to_owned()],
                source_refs: vec!["reproduction:targeted".to_owned()],
                independence_group: "ig:reproduction:targeted".to_owned(),
                verified: true,
                invalidated: false,
            };
            Box::pin(async move { Ok(if increment { vec![record] } else { Vec::new() }) })
        }
    }

    #[test]
    fn jev_unavailable_preserves_investigation_state_and_continues_next_action() {
        for code in ["JEV_TIMEOUT", "JEV_TRANSPORT", "JEV_INVALID_RESPONSE"] {
            let mut claim = disputed();
            let original_supporters = claim.supporters.clone();
            let original_opponents = claim.opponents.clone();
            let original_unknowns = claim.unknowns.clone();
            let mut store = vec![FusionEvidenceRecord {
                id: "ev_existing".to_owned(),
                claim_id: claim.id.clone(),
                provider: "existing".to_owned(),
                direction: EvidenceDirection::Support,
                kind: "source".to_owned(),
                strength: EvidenceStrength::StrongInference,
                facts: vec!["existing source fact".to_owned()],
                source_refs: vec!["source:existing".to_owned()],
                independence_group: "ig:existing".to_owned(),
                verified: false,
                invalidated: false,
            }];

            let report = futures::executor::block_on(investigate_claim(
                &mut claim,
                &mut store,
                &[Arc::new(TwoStepProvider)],
                &ConservativeCrossReviewer,
                &UnavailableVerifier { code },
                &TwoStepPlanner,
                InvestigationBudget::default(),
            ));

            assert_eq!(claim.supporters, original_supporters);
            assert_eq!(claim.opponents, original_opponents);
            assert_eq!(claim.unknowns, original_unknowns);
            assert!(store.iter().any(|record| record.id == "ev_existing"));
            assert_eq!(report.rounds.len(), 2);
            assert!(!report.rounds[0].produced_new_evidence);
            assert!(report.rounds[1].produced_new_evidence);
            assert_eq!(report.jev_unavailable.len(), 2);
            assert!(
                report
                    .jev_unavailable
                    .iter()
                    .all(|event| event.code == code)
            );
            assert_eq!(report.stop_reason, TerminalReason::Unresolvable);
        }
    }

    #[test]
    fn unavailable_dependency_is_an_attention_terminal_not_a_false_resolution() {
        #[derive(Debug)]
        struct MissingDependency;

        impl EvidenceProvider for MissingDependency {
            fn provider_id(&self) -> &'static str {
                "missing"
            }

            fn can_handle(&self, _action: &InvestigationAction) -> bool {
                true
            }

            fn investigate(
                &self,
                _action: &InvestigationAction,
                _claim: &ClaimNode,
            ) -> BoxFuture<'static, Result<Vec<FusionEvidenceRecord>, String>> {
                Box::pin(async { Err("DEPENDENCY_UNAVAILABLE: test binary is missing".to_owned()) })
            }
        }

        let mut claim = disputed();
        let mut store = Vec::new();
        let report = futures::executor::block_on(investigate_claim(
            &mut claim,
            &mut store,
            &[Arc::new(MissingDependency)],
            &ConservativeCrossReviewer,
            &PolicyEvidenceVerifier,
            &DefaultEvidencePlanner,
            InvestigationBudget::default(),
        ));

        assert_eq!(report.stop_reason, TerminalReason::AttentionRequired);
        assert_eq!(claim.state, ClaimState::Escalated);
        assert!(
            report
                .rounds
                .iter()
                .any(|round| round.failure_reason.as_deref()
                    == Some("DEPENDENCY_UNAVAILABLE: test binary is missing"))
        );
    }

    #[test]
    fn explicit_budget_can_stop_while_default_allows_every_planned_action() {
        let mut unconstrained_claim = disputed();
        let mut unconstrained_store = Vec::new();
        let unconstrained = futures::executor::block_on(investigate_claim(
            &mut unconstrained_claim,
            &mut unconstrained_store,
            &[Arc::new(TwoStepProvider)],
            &ConservativeCrossReviewer,
            &PolicyEvidenceVerifier,
            &TwoStepPlanner,
            InvestigationBudget::default(),
        ));
        assert_eq!(unconstrained.rounds.len(), 2);

        let mut limited_claim = disputed();
        let mut limited_store = Vec::new();
        let limited = futures::executor::block_on(investigate_claim(
            &mut limited_claim,
            &mut limited_store,
            &[Arc::new(TwoStepProvider)],
            &ConservativeCrossReviewer,
            &PolicyEvidenceVerifier,
            &TwoStepPlanner,
            InvestigationBudget {
                max_llm_rounds_per_claim: None,
                max_tool_escalation: Some(0),
                max_test_attempts: None,
            },
        ));
        assert!(limited.rounds.is_empty());
        assert_eq!(limited.stop_reason, TerminalReason::BudgetExhausted);
        assert_eq!(limited_claim.state, ClaimState::Escalated);
    }

    #[test]
    fn metrics_expose_oracle_capture_and_regret() {
        let metrics = FusionMetrics {
            best_fixed_single_correct: 4,
            oracle_union_correct: 6,
            fusion_correct: 5,
            total_claims: 6,
            minority_truth_recovered: 1,
            false_consensus: 0,
            evidence_yield: 3,
            evidence_verified: 1,
            unresolved_claims: 1,
        };
        assert_eq!(metrics.fusion_gain(), 1);
        assert_eq!(metrics.fusion_regret(), 0);
        assert!((metrics.oracle_capture_rate().unwrap() - 5.0 / 6.0).abs() < 1e-9);
    }
}
