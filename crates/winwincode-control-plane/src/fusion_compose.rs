use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde::Serialize;
use serde_json::{Value, json};
use winwincode_delivery::domain::{
    CanonicalDecision, ComputedDeliveryVerdict, Delivery, DeliveryValidationError, EvidenceRefType,
    FrozenDeliveryCandidate, evidence::ResolvedDeliveryEvidence,
};
use winwincode_fusion::answer_from_frames;
use winwincode_fusion::evidence::{
    ClaimTarget, ClaimVerification, EvidenceLedger, ModelClaim, SourceReceipt, SourceReceiptKind,
    VerificationConclusion,
};
use winwincode_fusion::{
    FusionBudget, FusionCandidate, FusionInput, FusionPanelResult, FusionProviderCandidate,
    FusionProviderRouter, run_blind_panel, validate_panel_result,
};

use winwincode_provider::{JevAttemptFailure, JevObservation, JevRun};

use crate::fusion_adjudication_host::adjudicate_canonical_decision_from_fusion_analysis;
use crate::fusion_analysis::{
    FusionAnalysis, FusionAnalysisError, FusionCandidateClaims, FusionClaim, FusionClaimPosition,
    FusionEvidence, FusionRound, FusionStopReason, analyze_fusion,
};

#[derive(Clone, Debug, PartialEq)]
pub struct ComposedFusionAnswer {
    pub candidate_id: String,
    pub answer: Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FusionRunnerSeat {
    pub candidate_id: String,
    pub routes: Vec<String>,
}

pub trait FusionRunnerAnswerPort: fmt::Debug + Send + Sync {
    fn run_blind_answers(
        &self,
        input: &FusionInput,
        seats: &[FusionRunnerSeat],
    ) -> BoxFuture<'static, Result<Vec<ComposedFusionAnswer>, FusionComposeError>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FusionComposeError {
    Panel(winwincode_fusion::FusionPanelError),
    Analysis(FusionAnalysisError),
    ClaimExtraction {
        candidate_id: String,
        message: String,
    },
    NoSuccessfulAnswers,
    Runner(String),
    MultiRound(String),
}

impl fmt::Display for FusionComposeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Panel(error) => write!(formatter, "fusion panel input rejected: {error}"),
            Self::Analysis(error) => write!(formatter, "fusion analysis rejected: {error:?}"),
            Self::ClaimExtraction {
                candidate_id,
                message,
            } => write!(
                formatter,
                "fusion claim extraction failed for {candidate_id}: {message}"
            ),
            Self::NoSuccessfulAnswers => {
                formatter.write_str("fusion compose had no successful answers")
            }
            Self::Runner(message) => write!(formatter, "fusion runner port failed: {message}"),
            Self::MultiRound(message) => write!(formatter, "fusion multi-round failed: {message}"),
        }
    }
}

impl std::error::Error for FusionComposeError {}

#[derive(Clone, Debug)]
pub struct ComposedFusionOutcome {
    pub panel: Option<FusionPanelResult>,
    pub answers: Vec<ComposedFusionAnswer>,
    pub claims: Vec<FusionCandidateClaims>,
    pub analysis: FusionAnalysis,
}

#[must_use]
pub fn build_fusion_input(
    question: impl Into<String>,
    canonical_context: Value,
    constraints: Vec<String>,
    expected_output_schema: Value,
    provider_candidates: Vec<FusionProviderCandidate>,
    budget: FusionBudget,
) -> FusionInput {
    FusionInput {
        question: question.into(),
        canonical_context,
        constraints,
        expected_output_schema,
        provider_candidates,
        budget,
    }
}

pub use winwincode_fusion::claims::default_claim_output_schema;

pub fn extract_claims_from_answer(
    candidate_id: &str,
    answer: &Value,
) -> Result<FusionCandidateClaims, FusionComposeError> {
    winwincode_fusion::claims::extract_claims_from_answer(candidate_id, answer).map_err(|error| {
        FusionComposeError::ClaimExtraction {
            candidate_id: error.candidate_id,
            message: error.message,
        }
    })
}

fn claims_and_analysis(
    answers: &[ComposedFusionAnswer],
) -> Result<(Vec<FusionCandidateClaims>, FusionAnalysis), FusionComposeError> {
    if answers.is_empty() {
        return Err(FusionComposeError::NoSuccessfulAnswers);
    }
    let mut claims = Vec::with_capacity(answers.len());
    for answer in answers {
        claims.push(extract_claims_from_answer(
            &answer.candidate_id,
            &answer.answer,
        )?);
    }
    let analysis = analyze_fusion(&claims).map_err(FusionComposeError::Analysis)?;
    Ok((claims, analysis))
}

pub async fn compose_blind_panel(
    panel_id: &str,
    input: FusionInput,
    router: Arc<dyn FusionProviderRouter>,
) -> Result<ComposedFusionOutcome, FusionComposeError> {
    let panel = run_blind_panel(panel_id, input.clone(), router)
        .await
        .map_err(FusionComposeError::Panel)?;
    compose_collected_panel(panel_id, &input, panel)
}

pub fn compose_collected_panel(
    panel_id: &str,
    input: &FusionInput,
    panel: FusionPanelResult,
) -> Result<ComposedFusionOutcome, FusionComposeError> {
    validate_panel_result(panel_id, input, &panel).map_err(FusionComposeError::Panel)?;
    let answers = answers_from_panel_candidates(&panel.candidates);
    let (claims, analysis) = claims_and_analysis(&answers)?;
    Ok(ComposedFusionOutcome {
        panel: Some(panel),
        answers,
        claims,
        analysis,
    })
}

#[must_use]
pub fn answers_from_panel_candidates(candidates: &[FusionCandidate]) -> Vec<ComposedFusionAnswer> {
    candidates
        .iter()
        .map(|candidate| ComposedFusionAnswer {
            candidate_id: candidate.audit.candidate_id.clone(),
            answer: candidate.answer.clone(),
        })
        .collect()
}

pub fn compose_independent_answers(
    answers: Vec<ComposedFusionAnswer>,
) -> Result<ComposedFusionOutcome, FusionComposeError> {
    let (claims, analysis) = claims_and_analysis(&answers)?;
    Ok(ComposedFusionOutcome {
        panel: None,
        answers,
        claims,
        analysis,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct InvestigationRequest {
    pub claim_key: String,
    pub summary: String,
    pub question: String,
    pub canonical_context: Value,
    pub constraints: Vec<String>,
    pub support_evidence: Vec<Value>,
    pub oppose_evidence: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InvestigationAnswer {
    pub seat_id: String,
    pub new_evidence: Vec<InvestigationEvidenceItem>,
    pub conclusion: FusionClaimPosition,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InvestigationEvidenceItem {
    pub kind: String,
    pub detail: String,
    pub side: FusionClaimPosition,
    pub source_receipt: SourceReceipt,
    pub claim_verification: ClaimVerification,
}

pub use winwincode_fusion::judge::JudgeRequest;

#[derive(Clone, Debug, PartialEq)]
pub struct JudgeAnswer {
    pub claim_key: String,
    pub outcome: JudgeClaimOutcome,
    pub confidence: u8,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JudgeClaimOutcome {
    ConfirmedSupports,
    ConfirmedOpposes,
    Unresolved,
}

pub trait FusionInvestigationPort: fmt::Debug + Send + Sync {
    fn investigate(
        &self,
        request: InvestigationRequest,
    ) -> BoxFuture<'static, Result<Vec<InvestigationAnswer>, FusionComposeError>>;
}

pub trait FusionJudgePort: fmt::Debug + Send + Sync {
    fn judge(
        &self,
        request: JudgeRequest,
    ) -> BoxFuture<'static, Result<JevRun<JudgeAnswer>, FusionComposeError>>;
}

#[derive(Debug)]
pub struct RuntimeFusionJudge {
    runtime: Arc<winwincode_provider::JevRuntime>,
    options: winwincode_provider::JevExecutionOptions,
}

impl RuntimeFusionJudge {
    pub fn new(
        runtime: Arc<winwincode_provider::JevRuntime>,
        options: winwincode_provider::JevExecutionOptions,
    ) -> Self {
        Self { runtime, options }
    }
}

impl FusionJudgePort for RuntimeFusionJudge {
    fn judge(
        &self,
        request: JudgeRequest,
    ) -> BoxFuture<'static, Result<JevRun<JudgeAnswer>, FusionComposeError>> {
        let runtime = Arc::clone(&self.runtime);
        let options = self.options;
        Box::pin(async move {
            let premise = request
                .premise()
                .map_err(|error| FusionComposeError::MultiRound(error.to_string()))?;
            let run = runtime
                .evaluate(
                    winwincode_provider::JevHypothesis {
                        premise,
                        hypothesis: request.summary,
                    },
                    options,
                )
                .await;
            let value = run.value.map(|evaluation| {
                let scores = evaluation.scores;
                let outcome = if scores.entailment > scores.contradiction
                    && scores.entailment > scores.neutral
                {
                    JudgeClaimOutcome::ConfirmedSupports
                } else if scores.contradiction > scores.entailment
                    && scores.contradiction > scores.neutral
                {
                    JudgeClaimOutcome::ConfirmedOpposes
                } else {
                    JudgeClaimOutcome::Unresolved
                };
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "JevRuntime validates finite probabilities in [0, 1]"
                )]
                let confidence = (scores.confidence() * 100.0).round() as u8;
                JudgeAnswer {
                    claim_key: request.claim_key,
                    outcome,
                    confidence,
                    reason: "Semantic judgment requires independent verification".to_owned(),
                }
            });
            Ok(JevRun {
                value,
                observation: run.observation,
                failures: run.failures,
            })
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JevJudgeCall {
    pub claim_key: String,
    pub observation: Option<JevObservation>,
    pub failures: Vec<JevAttemptFailure>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MultiRoundReport {
    pub stop_reason: FusionStopReason,
    pub rounds: Vec<MultiRoundStep>,
    pub judged: Vec<JudgeAnswer>,
    pub jev_calls: Vec<JevJudgeCall>,
    pub jev_unavailable: Vec<crate::fusion_investigation::JevUnavailableRecord>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MultiRoundStep {
    pub round: FusionRound,
    pub disputed_claim_keys: Vec<String>,
    pub produced_new_evidence: bool,
}

enum RoundControl {
    Continue,
    Stop(FusionStopReason),
}

#[must_use]
pub fn build_blind_judge_request(
    composed: &ComposedFusionOutcome,
    claim_key: &str,
    summary: &str,
    question: &str,
    canonical_context: Value,
) -> JudgeRequest {
    winwincode_fusion::judge::build_blind_judge_request(
        &composed.claims,
        claim_key,
        summary,
        question,
        canonical_context,
    )
}

fn same_claim_text(left: &str, right: &str) -> bool {
    left.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .eq_ignore_ascii_case(&right.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn evidence_type_for_receipt(kind: SourceReceiptKind) -> EvidenceRefType {
    match kind {
        SourceReceiptKind::Command => EvidenceRefType::Command,
        SourceReceiptKind::Test => EvidenceRefType::Test,
        SourceReceiptKind::Diff => EvidenceRefType::Diff,
        SourceReceiptKind::File => EvidenceRefType::File,
        SourceReceiptKind::RunEvent => EvidenceRefType::RuntimeEvent,
        SourceReceiptKind::IndependentReview => EvidenceRefType::ReviewFinding,
    }
}

fn admit_investigation_item(
    ledger: &mut EvidenceLedger,
    claim_key: &str,
    item: &InvestigationEvidenceItem,
) -> bool {
    if !same_claim_text(&item.claim_verification.proposition, claim_key) {
        return false;
    }
    let expected_direction = match item.side {
        FusionClaimPosition::Supports => VerificationConclusion::Support,
        FusionClaimPosition::Opposes => VerificationConclusion::Counter,
    };
    if item.claim_verification.conclusion != expected_direction
        || item.claim_verification.source_receipt_ids != [item.source_receipt.id.clone()]
    {
        return false;
    }
    if ledger
        .register_source_receipt(item.source_receipt.clone())
        .is_err()
        || ledger
            .record_claim_verification(item.claim_verification.clone())
            .is_err()
    {
        return false;
    }

    let target = &item.claim_verification;
    let admission = ledger.admit_model_claim(ModelClaim {
        claim_id: item.source_receipt.id.clone(),
        proposition: target.proposition.clone(),
        scope: target.scope.clone(),
        version: target.version.clone(),
        explanation: item.detail.clone(),
        source_receipt_ids: vec![item.source_receipt.id.clone()],
    });
    admission.produced_new_evidence
}

fn recompose_with_round3(
    composed: &mut ComposedFusionOutcome,
    extra_answers: &mut Vec<ComposedFusionAnswer>,
    pack_items: &[Value],
    claim_keys: &[String],
) -> Result<(), FusionComposeError> {
    let mut investigation_claims = Vec::new();
    for item in pack_items {
        let claim_key = item
            .get("claimKey")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| {
                claim_keys
                    .iter()
                    .find(|key| {
                        composed
                            .analysis
                            .conflicts
                            .iter()
                            .any(|conflict| conflict.claim_key == **key)
                    })
                    .cloned()
            });
        let Some(claim_key) = claim_key else {
            continue;
        };
        let summary = composed
            .analysis
            .conflicts
            .iter()
            .find(|conflict| conflict.claim_key == claim_key)
            .map(|conflict| conflict.summary.clone())
            .unwrap_or_default();
        let position = match item.get("side").and_then(Value::as_str) {
            Some("opposes") => FusionClaimPosition::Opposes,
            _ => FusionClaimPosition::Supports,
        };
        let source_ref = item
            .get("sourceRef")
            .or_else(|| item.get("source_ref"))
            .and_then(Value::as_str)
            .unwrap_or("unresolved-source-receipt")
            .to_owned();
        let evidence_type = item
            .get("evidenceType")
            .or_else(|| item.get("evidence_type"))
            .and_then(Value::as_str)
            .and_then(|kind| serde_json::from_value(Value::String(kind.to_owned())).ok())
            .unwrap_or(EvidenceRefType::ReviewFinding);
        investigation_claims.push(FusionClaim {
            claim_key,
            summary,
            position,
            evidence: vec![FusionEvidence {
                evidence_type,
                source_ref,
                verified_conclusion: None,
            }],
            required_evidence: Vec::new(),
        });
    }
    if investigation_claims.is_empty() {
        return Ok(());
    }
    for (index, claim) in investigation_claims.into_iter().enumerate() {
        extra_answers.push(ComposedFusionAnswer {
            candidate_id: format!("round3-investigation-{index}"),
            answer: json!({
                "claims": [{
                    "claimKey": claim.claim_key,
                    "summary": claim.summary,
                    "position": match claim.position {
                        FusionClaimPosition::Supports => "supports",
                        FusionClaimPosition::Opposes => "opposes",
                    },
                    "evidence": [{
                        "evidenceType": "review_finding",
                        "sourceRef": claim.evidence[0].source_ref,
                        "verifiedConclusion": null
                    }]
                }],
                "decision": "inconclusive",
                "rationale": "R3 targeted investigation new evidence"
            }),
        });
    }
    *composed = compose_independent_answers(extra_answers.clone())?;
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "Pass the existing round state and evidence authorities explicitly"
)]
async fn run_targeted_investigation(
    composed: &mut ComposedFusionOutcome,
    rounds: &mut [MultiRoundStep],
    extra_answers: &mut Vec<ComposedFusionAnswer>,
    claim_keys: &[String],
    input: &FusionInput,
    investigation: &Arc<dyn FusionInvestigationPort>,
    ledger: &mut EvidenceLedger,
    target_by_claim: &mut BTreeMap<String, ClaimTarget>,
) -> Result<RoundControl, FusionComposeError> {
    let mut produced_new_evidence = false;
    let mut pack_items: Vec<Value> = Vec::new();
    for claim_key in claim_keys {
        let summary = composed
            .analysis
            .conflicts
            .iter()
            .find(|conflict| conflict.claim_key == *claim_key)
            .map(|conflict| conflict.summary.clone())
            .unwrap_or_default();
        let mut support_evidence = Vec::new();
        let mut oppose_evidence = Vec::new();
        for answer in &composed.answers {
            if let Ok(candidate) = extract_claims_from_answer(&answer.candidate_id, &answer.answer)
            {
                for claim in &candidate.claims {
                    if claim.claim_key != *claim_key {
                        continue;
                    }
                    let row = json!({
                        "seat": answer.candidate_id,
                        "position": match claim.position {
                            FusionClaimPosition::Supports => "supports",
                            FusionClaimPosition::Opposes => "opposes",
                        },
                        "evidence": claim.evidence.iter().map(|e| json!({
                            "evidenceType": format!("{:?}", e.evidence_type),
                            "sourceRef": e.source_ref,
                        })).collect::<Vec<_>>(),
                    });
                    match claim.position {
                        FusionClaimPosition::Supports => support_evidence.push(row),
                        FusionClaimPosition::Opposes => oppose_evidence.push(row),
                    }
                }
            }
        }
        let request = InvestigationRequest {
            claim_key: (*claim_key).clone(),
            summary,
            question: input.question.clone(),
            canonical_context: input.canonical_context.clone(),
            constraints: input.constraints.clone(),
            support_evidence,
            oppose_evidence,
        };
        let answers = investigation.investigate(request).await.map_err(|error| {
            FusionComposeError::MultiRound(format!("investigation failed for {claim_key}: {error}"))
        })?;
        for answer in answers {
            for item in &answer.new_evidence {
                if !admit_investigation_item(ledger, claim_key, item) {
                    continue;
                }
                target_by_claim.insert(
                    (*claim_key).clone(),
                    ClaimTarget {
                        proposition: item.claim_verification.proposition.clone(),
                        scope: item.claim_verification.scope.clone(),
                        version: item.claim_verification.version.clone(),
                    },
                );
                produced_new_evidence = true;
                pack_items.push(json!({
                    "round": "R3",
                    "seat": answer.seat_id,
                    "kind": item.kind,
                    "detail": item.detail,
                    "sourceRef": item.source_receipt.locator,
                    "sourceReceiptId": item.source_receipt.id,
                    "evidenceType": format!("{:?}", evidence_type_for_receipt(item.source_receipt.kind)),
                    "version": item.source_receipt.version,
                    "executionOwner": item.source_receipt.execution_owner,
                    "side": match item.side {
                        FusionClaimPosition::Supports => "supports",
                        FusionClaimPosition::Opposes => "opposes",
                    },
                }));
            }
        }
    }
    if let Some(step) = rounds.last_mut() {
        step.produced_new_evidence = produced_new_evidence;
    }
    if !produced_new_evidence {
        return Ok(RoundControl::Continue);
    }
    recompose_with_round3(composed, extra_answers, &pack_items, claim_keys)?;
    Ok(RoundControl::Continue)
}

#[allow(
    clippy::too_many_arguments,
    reason = "Keep judge output separate from independent verification authority"
)]
async fn run_verification(
    composed: &mut ComposedFusionOutcome,
    rounds: &mut Vec<MultiRoundStep>,
    judged: &mut Vec<JudgeAnswer>,
    jev_calls: &mut Vec<JevJudgeCall>,
    claim_keys: Vec<String>,
    input: &FusionInput,
    judge: &Arc<dyn FusionJudgePort>,
    ledger: &EvidenceLedger,
    target_by_claim: &BTreeMap<String, ClaimTarget>,
    jev_unavailable: &mut Vec<crate::fusion_investigation::JevUnavailableRecord>,
) -> Result<RoundControl, FusionComposeError> {
    rounds.push(MultiRoundStep {
        round: FusionRound::Verification,
        disputed_claim_keys: claim_keys.clone(),
        produced_new_evidence: false,
    });
    for claim_key in claim_keys {
        let summary = composed
            .analysis
            .conflicts
            .iter()
            .find(|conflict| conflict.claim_key == claim_key)
            .map(|conflict| conflict.summary.clone())
            .unwrap_or_default();
        let request = build_blind_judge_request(
            composed,
            &claim_key,
            &summary,
            &input.question,
            input.canonical_context.clone(),
        );
        let result = judge.judge(request).await.and_then(|run| {
            let code = match run.failures.last().map(|failure| failure.kind) {
                Some(winwincode_provider::JevProviderErrorKind::Timeout) => "JEV_TIMEOUT",
                Some(winwincode_provider::JevProviderErrorKind::InvalidResponse) => {
                    "JEV_INVALID_RESPONSE"
                }
                _ => "JEV_TRANSPORT",
            };
            jev_calls.push(JevJudgeCall {
                claim_key: claim_key.clone(),
                observation: run.observation,
                failures: run.failures,
            });
            run.value
                .ok_or_else(|| FusionComposeError::MultiRound(code.to_owned()))
        });
        let judged_answer = match result {
            Ok(answer) => answer,
            Err(error) => {
                let detail = error.to_string();
                jev_unavailable.push(crate::fusion_investigation::JevUnavailableRecord {
                    code: crate::fusion_investigation::jev_unavailable_code(&detail),
                    claim_id: claim_key.clone(),
                    detail,
                });
                continue;
            }
        };
        let target = target_by_claim.get(&claim_key);
        let position = match judged_answer.outcome {
            JudgeClaimOutcome::ConfirmedSupports
                if target.is_some_and(|target| ledger.has_verified_support(target)) =>
            {
                Some(FusionClaimPosition::Supports)
            }
            JudgeClaimOutcome::ConfirmedOpposes
                if target.is_some_and(|target| ledger.has_valid_counter(target)) =>
            {
                Some(FusionClaimPosition::Opposes)
            }
            JudgeClaimOutcome::ConfirmedSupports
            | JudgeClaimOutcome::ConfirmedOpposes
            | JudgeClaimOutcome::Unresolved => None,
        };
        let conclusion = match position {
            Some(FusionClaimPosition::Supports) => {
                crate::fusion_analysis::JevVerificationConclusion::Supports
            }
            Some(FusionClaimPosition::Opposes) => {
                crate::fusion_analysis::JevVerificationConclusion::Opposes
            }
            None => crate::fusion_analysis::JevVerificationConclusion::Insufficient,
        };
        crate::fusion_analysis::resolve_jev_conflict_with_verification(
            &mut composed.analysis,
            &claim_key,
            conclusion,
            &judged_answer.reason,
        );
        judged.push(judged_answer);
    }
    Ok(RoundControl::Stop(
        if crate::fusion_analysis::undecided_conflicts(&composed.analysis).is_empty()
            && crate::fusion_analysis::awaiting_verification_conflicts(&composed.analysis)
                .is_empty()
        {
            FusionStopReason::EvidenceConvergence
        } else {
            FusionStopReason::Unresolvable
        },
    ))
}

pub async fn compose_multiround(
    panel_id: &str,
    input: FusionInput,
    router: Arc<dyn FusionProviderRouter>,
    investigation: Arc<dyn FusionInvestigationPort>,
    judge: Arc<dyn FusionJudgePort>,
    max_investigation_rounds: Option<u8>,
) -> Result<(ComposedFusionOutcome, MultiRoundReport), FusionComposeError> {
    let mut composed = compose_blind_panel(panel_id, input.clone(), Arc::clone(&router)).await?;
    let mut rounds: Vec<MultiRoundStep> = Vec::new();
    let mut judged = Vec::new();
    let mut jev_calls = Vec::new();
    let mut extra_answers: Vec<ComposedFusionAnswer> = composed.answers.clone();
    let mut evidence_ledger = EvidenceLedger::default();
    let mut target_by_claim = BTreeMap::new();
    let mut jev_unavailable: Vec<crate::fusion_investigation::JevUnavailableRecord> = Vec::new();

    let mut investigation_rounds = 0u8;
    let stop_reason = loop {
        let produced_new_evidence = rounds
            .iter()
            .rev()
            .find(|step| step.round == FusionRound::TargetedInvestigation)
            .is_some_and(|step| step.produced_new_evidence);
        let disputed: Vec<String> = crate::fusion_analysis::undecided_conflicts(&composed.analysis)
            .into_iter()
            .map(|conflict| conflict.claim_key.clone())
            .collect();
        rounds.push(MultiRoundStep {
            round: if investigation_rounds == 0 {
                FusionRound::ConflictDetection
            } else {
                FusionRound::TargetedInvestigation
            },
            disputed_claim_keys: disputed.clone(),
            produced_new_evidence: false,
        });

        let budget_exhausted =
            max_investigation_rounds.is_some_and(|limit| investigation_rounds >= limit);
        let action = crate::fusion_analysis::fusion_next_action(
            &composed.analysis,
            if investigation_rounds == 0 {
                FusionRound::Discovery
            } else {
                FusionRound::TargetedInvestigation
            },
            budget_exhausted,
            produced_new_evidence,
        );

        match action {
            crate::fusion_analysis::FusionNextAction::Finish { reason } => break reason,
            crate::fusion_analysis::FusionNextAction::TargetedInvestigation { claim_keys } => {
                if budget_exhausted {
                    break FusionStopReason::BudgetLimit;
                }
                investigation_rounds = investigation_rounds.saturating_add(1);
                match run_targeted_investigation(
                    &mut composed,
                    &mut rounds,
                    &mut extra_answers,
                    &claim_keys,
                    &input,
                    &investigation,
                    &mut evidence_ledger,
                    &mut target_by_claim,
                )
                .await?
                {
                    RoundControl::Continue => {}
                    RoundControl::Stop(reason) => break reason,
                }
            }
            crate::fusion_analysis::FusionNextAction::Verify { claim_keys } => {
                match run_verification(
                    &mut composed,
                    &mut rounds,
                    &mut judged,
                    &mut jev_calls,
                    claim_keys,
                    &input,
                    &judge,
                    &evidence_ledger,
                    &target_by_claim,
                    &mut jev_unavailable,
                )
                .await?
                {
                    RoundControl::Continue => {}
                    RoundControl::Stop(reason) => break reason,
                }
            }
        }
    };

    Ok((
        composed,
        MultiRoundReport {
            stop_reason,
            rounds,
            judged,
            jev_calls,
            jev_unavailable,
        },
    ))
}

pub async fn compose_multiround_with_evidence(
    panel_id: &str,
    input: FusionInput,
    router: Arc<dyn FusionProviderRouter>,
    providers: Vec<Arc<dyn crate::fusion_planner::EvidenceProvider>>,
) -> Result<
    (
        ComposedFusionOutcome,
        crate::fusion_knowledge::ClaimGraph,
        Vec<crate::fusion_investigation::InvestigationRunReport>,
    ),
    FusionComposeError,
> {
    let composed = compose_blind_panel(panel_id, input, router).await?;
    let claims = composed.claims.clone();
    let mut graph = crate::fusion_knowledge::build_claim_graph(&claims);
    let mut reports = Vec::new();
    let mut store = graph.evidence.clone();
    for index in 0..graph.claims.len() {
        let mut claim = graph.claims[index].clone();
        if !crate::fusion_knowledge::needs_investigation(&claim) {
            continue;
        }
        let report = crate::fusion_investigation::investigate_claim(
            &mut claim,
            &mut store,
            &providers,
            &crate::fusion_investigation::ConservativeCrossReviewer,
            &crate::fusion_investigation::PolicyEvidenceVerifier,
            &crate::fusion_planner::DefaultEvidencePlanner,
            crate::fusion_investigation::InvestigationBudget::default(),
        )
        .await;
        graph.claims[index] = claim;
        reports.push(report);
    }
    graph.evidence = store;
    Ok((composed, graph, reports))
}

pub async fn compose_via_runner_port(
    input: FusionInput,
    seats: Vec<FusionRunnerSeat>,
    runner: Arc<dyn FusionRunnerAnswerPort>,
) -> Result<ComposedFusionOutcome, FusionComposeError> {
    let answers = runner.run_blind_answers(&input, &seats).await?;
    compose_independent_answers(answers)
}

pub fn answers_from_parallel_model_frames(
    rows: impl IntoIterator<Item = (String, Vec<String>)>,
) -> Result<Vec<ComposedFusionAnswer>, FusionComposeError> {
    let mut answers = Vec::new();
    for (target_id, frames) in rows {
        let answer =
            answer_from_frames(&frames).ok_or_else(|| FusionComposeError::ClaimExtraction {
                candidate_id: target_id.clone(),
                message: "no answer frame in parallel model result".to_owned(),
            })?;
        answers.push(ComposedFusionAnswer {
            candidate_id: target_id,
            answer,
        });
    }
    Ok(answers)
}

pub fn adjudicate_composed_fusion(
    delivery: &Delivery,
    candidate: &FrozenDeliveryCandidate,
    verifier_evidence: Option<&ComputedDeliveryVerdict>,
    evidence: &[ResolvedDeliveryEvidence],
    composed: &ComposedFusionOutcome,
) -> Result<CanonicalDecision, DeliveryValidationError> {
    adjudicate_canonical_decision_from_fusion_analysis(
        delivery,
        candidate,
        verifier_evidence,
        evidence,
        &composed.analysis,
    )
}

#[derive(Debug)]
pub struct FunctionFusionRunnerAnswerPort {
    answers: Vec<ComposedFusionAnswer>,
}

impl FunctionFusionRunnerAnswerPort {
    #[must_use]
    pub fn new(answers: Vec<ComposedFusionAnswer>) -> Self {
        Self { answers }
    }
}

impl FusionRunnerAnswerPort for FunctionFusionRunnerAnswerPort {
    fn run_blind_answers(
        &self,
        _input: &FusionInput,
        _seats: &[FusionRunnerSeat],
    ) -> BoxFuture<'static, Result<Vec<ComposedFusionAnswer>, FusionComposeError>> {
        let answers = self.answers.clone();
        Box::pin(async move { Ok(answers) })
    }
}

pub const LIVE_MODEL_PORT_PLUG_IN_NOTE: &str =
    "winwincode_codex::ParallelModelRunner over kernel ModelPort; target_id == candidate_id";

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use futures::future::BoxFuture;
    use serde_json::json;
    use tokio::sync::Barrier;
    use winwincode_delivery::application::verdict::test_support::{
        VerdictFixtureOutcome, verdict_fixture,
    };
    use winwincode_delivery::domain::{DecisionAuthority, DeliveryId, compute_delivery_verdict};
    use winwincode_fusion::{
        FusionProvider, FusionProviderAnswer, FusionProviderError, FusionProviderRequest,
        FusionTokenUsage, MapFusionProviderRouter,
    };

    use super::*;

    const PRODUCED_AT_MILLIS: u64 = 1_800_000_000_100;

    #[derive(Debug, Clone, Copy)]
    enum MockBehavior {
        Answer(&'static str),
        Fail,
    }

    #[derive(Debug)]
    struct MockFusionProvider {
        name: &'static str,
        behavior: MockBehavior,
        barrier: Arc<Barrier>,
        requests: Arc<Mutex<Vec<FusionProviderRequest>>>,
    }

    impl FusionProvider for MockFusionProvider {
        fn complete(
            &self,
            request: FusionProviderRequest,
        ) -> BoxFuture<'static, Result<FusionProviderAnswer, FusionProviderError>> {
            self.requests.lock().expect("request log").push(request);
            let barrier = Arc::clone(&self.barrier);
            let name = self.name;
            let behavior = self.behavior;
            Box::pin(async move {
                barrier.wait().await;
                match behavior {
                    MockBehavior::Answer(answer) => Ok(FusionProviderAnswer {
                        provider_response_id: format!("response-{name}"),
                        answer: claims_answer(answer, "supports", None),
                        token_usage: Some(FusionTokenUsage {
                            input_tokens: 8,
                            output_tokens: 4,
                            total_tokens: 12,
                        }),
                    }),
                    MockBehavior::Fail => Err(FusionProviderError::new(
                        "PROVIDER_UNAVAILABLE",
                        format!("{name} is unavailable"),
                    )),
                }
            })
        }
    }

    fn claims_answer(stance: &str, position: &str, verified: Option<&str>) -> Value {
        let mut evidence = Vec::new();
        if let Some(conclusion) = verified {
            evidence.push(json!({
                "evidenceType": "test",
                "sourceRef": format!("verification:{stance}"),
                "verifiedConclusion": conclusion,
            }));
        } else {
            evidence.push(json!({
                "evidenceType": "command",
                "sourceRef": format!("command:{stance}"),
            }));
        }
        json!({
            "claims": [{
                "claimKey": "claim:tests-pass",
                "summary": "The tests pass",
                "position": position,
                "evidence": evidence,
            }]
        })
    }

    fn sample_input(timeout_millis: u64) -> FusionInput {
        build_fusion_input(
            "Which change fixes the root cause?",
            json!({ "repository": "fixture", "revision": "abc123" }),
            vec!["Cite only the supplied context.".to_owned()],
            default_claim_output_schema(),
            ["a", "b", "c"]
                .into_iter()
                .map(|suffix| FusionProviderCandidate {
                    id: format!("candidate-{suffix}"),
                    provider: format!("provider-{suffix}"),
                    model: format!("model-{suffix}"),
                    reasoning_effort: Some("high".to_owned()),
                })
                .collect(),
            FusionBudget {
                candidate_timeout_millis: Some(timeout_millis),
                max_total_tokens: Some(2_000),
            },
        )
    }

    fn panel_router(behaviors: [(&'static str, MockBehavior); 3]) -> Arc<dyn FusionProviderRouter> {
        let barrier = Arc::new(Barrier::new(behaviors.len()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut map = MapFusionProviderRouter::new();
        for (name, behavior) in behaviors {
            map = map.with(
                name,
                Arc::new(MockFusionProvider {
                    name,
                    behavior,
                    barrier: Arc::clone(&barrier),
                    requests: Arc::clone(&requests),
                }),
            );
        }
        Arc::new(map)
    }

    fn mock_model_port_like_router() -> Arc<dyn FusionProviderRouter> {
        panel_router([
            ("provider-a", MockBehavior::Answer("panel-a")),
            ("provider-b", MockBehavior::Answer("panel-b")),
            ("provider-c", MockBehavior::Answer("panel-c")),
        ])
    }

    #[derive(Debug)]
    struct MockParallelModelSource;

    #[derive(Debug)]
    struct Seat {
        position: &'static str,
    }
    impl FusionProvider for Seat {
        fn complete(
            &self,
            request: FusionProviderRequest,
        ) -> BoxFuture<'static, Result<FusionProviderAnswer, FusionProviderError>> {
            let position = self.position;
            Box::pin(async move {
                Ok(FusionProviderAnswer {
                    provider_response_id: format!("resp-{}", request.candidate_id),
                    answer: json!({
                        "claims": [{
                            "claimKey": "claim:tests-pass",
                            "summary": "The tests pass",
                            "position": position,
                            "evidence": [{
                                "evidenceType": "command",
                                "sourceRef": "tool:assert",
                                "verifiedConclusion": null
                            }]
                        }],
                        "decision": "inconclusive",
                        "rationale": "fixture"
                    }),
                    token_usage: None,
                })
            })
        }
    }

    struct MapRouter {
        p1: Arc<dyn FusionProvider>,
        p2: Arc<dyn FusionProvider>,
        p3: Arc<dyn FusionProvider>,
    }
    impl fmt::Debug for MapRouter {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("MapRouter")
        }
    }
    impl FusionProviderRouter for MapRouter {
        fn resolve(&self, provider: &str) -> Option<Arc<dyn FusionProvider>> {
            match provider {
                "provider-a" => Some(Arc::clone(&self.p1)),
                "provider-b" => Some(Arc::clone(&self.p2)),
                "provider-c" => Some(Arc::clone(&self.p3)),
                _ => None,
            }
        }
    }

    #[derive(Debug)]
    struct NoNewEvidence;
    impl FusionInvestigationPort for NoNewEvidence {
        fn investigate(
            &self,
            _request: InvestigationRequest,
        ) -> BoxFuture<'static, Result<Vec<InvestigationAnswer>, FusionComposeError>> {
            Box::pin(async {
                Ok(vec![InvestigationAnswer {
                    seat_id: "inv".to_owned(),
                    new_evidence: Vec::new(),
                    conclusion: FusionClaimPosition::Supports,
                }])
            })
        }
    }

    #[derive(Debug)]
    struct MustNotJudge;
    impl FusionJudgePort for MustNotJudge {
        fn judge(
            &self,
            _request: JudgeRequest,
        ) -> BoxFuture<'static, Result<JevRun<JudgeAnswer>, FusionComposeError>> {
            Box::pin(async {
                Err(FusionComposeError::MultiRound(
                    "ADR-0036 forbids JEV before new evidence".to_owned(),
                ))
            })
        }
    }

    #[derive(Debug)]
    struct UnavailableJudge {
        code: &'static str,
    }
    impl FusionJudgePort for UnavailableJudge {
        fn judge(
            &self,
            _request: JudgeRequest,
        ) -> BoxFuture<'static, Result<JevRun<JudgeAnswer>, FusionComposeError>> {
            let code = self.code;
            Box::pin(async move { Err(FusionComposeError::MultiRound(code.to_owned())) })
        }
    }

    #[test]
    fn serialized_judge_request_excludes_identity_and_vote_metadata() {
        let composed = compose_independent_answers(vec![
            ComposedFusionAnswer {
                candidate_id: "candidate-alpha".to_owned(),
                answer: claims_answer("alpha", "supports", None),
            },
            ComposedFusionAnswer {
                candidate_id: "candidate-beta".to_owned(),
                answer: claims_answer("beta", "opposes", None),
            },
            ComposedFusionAnswer {
                candidate_id: "candidate-gamma".to_owned(),
                answer: claims_answer("gamma", "opposes", None),
            },
        ])
        .expect("fixture claims compose");

        let request = build_blind_judge_request(
            &composed,
            "claim:tests-pass",
            "The tests pass",
            "Which change fixes the root cause?",
            json!({
                "provider": "provider-alpha",
                "model": "model-alpha",
                "seat": "candidate-alpha",
                "candidate": "candidate-alpha",
                "voteCount": 3,
                "supportCount": 2,
                "repository": "fixture",
                "revision": "abc123",
            }),
        );
        let serialized = serde_json::to_value(request).expect("judge request serializes");
        let text = serialized.to_string();

        for forbidden in [
            "provider",
            "model",
            "seat",
            "candidate",
            "voteCount",
            "supportCount",
        ] {
            assert!(
                !text.contains(forbidden),
                "judge request leaked {forbidden}: {text}"
            );
        }
        for identity in ["provider-alpha", "model-alpha", "candidate-alpha"] {
            assert!(
                !text.contains(identity),
                "judge request leaked identity {identity}: {text}"
            );
        }
        assert_eq!(
            serialized["canonicalContext"],
            json!({
                "repository": "fixture",
                "revision": "abc123",
            })
        );
    }

    #[test]
    fn serialized_judge_request_does_not_forward_model_verified_conclusion() {
        let composed = compose_independent_answers(vec![
            ComposedFusionAnswer {
                candidate_id: "candidate-alpha".to_owned(),
                answer: claims_answer("alpha", "supports", Some("pass")),
            },
            ComposedFusionAnswer {
                candidate_id: "candidate-beta".to_owned(),
                answer: claims_answer("beta", "opposes", Some("fail")),
            },
        ])
        .expect("malicious verification fields are parseable model prose");

        let request = build_blind_judge_request(
            &composed,
            "claim:tests-pass",
            "The tests pass",
            "Which change fixes the root cause?",
            json!({ "repository": "fixture", "revision": "abc123" }),
        );
        let serialized = serde_json::to_value(request).expect("judge request serializes");

        assert!(serialized.to_string().contains("verification:alpha"));
        assert!(serialized.to_string().contains("verification:beta"));
        assert!(
            serialized["evidencePack"]
                .as_array()
                .expect("evidence pack is an array")
                .iter()
                .all(|row| row.get("verifiedConclusion").is_none())
        );
        assert!(!serialized.to_string().contains("verifiedConclusion"));
    }

    #[test]
    fn serialized_judge_request_ignores_seat_rename_order_and_repeated_support() {
        let baseline = compose_independent_answers(vec![
            ComposedFusionAnswer {
                candidate_id: "candidate-alpha".to_owned(),
                answer: claims_answer("same-support", "supports", None),
            },
            ComposedFusionAnswer {
                candidate_id: "candidate-beta".to_owned(),
                answer: claims_answer("same-oppose", "opposes", None),
            },
            ComposedFusionAnswer {
                candidate_id: "candidate-gamma".to_owned(),
                answer: claims_answer("same-oppose", "opposes", None),
            },
        ])
        .expect("baseline claims compose");
        let reordered_and_repeated = compose_independent_answers(vec![
            ComposedFusionAnswer {
                candidate_id: "seat-zulu".to_owned(),
                answer: claims_answer("same-oppose", "opposes", None),
            },
            ComposedFusionAnswer {
                candidate_id: "seat-yankee".to_owned(),
                answer: claims_answer("same-support", "supports", None),
            },
            ComposedFusionAnswer {
                candidate_id: "seat-xray".to_owned(),
                answer: claims_answer("same-oppose", "opposes", None),
            },
            ComposedFusionAnswer {
                candidate_id: "seat-whiskey".to_owned(),
                answer: claims_answer("same-support", "supports", None),
            },
        ])
        .expect("renamed claims compose");

        let baseline_request = build_blind_judge_request(
            &baseline,
            "claim:tests-pass",
            "The tests pass",
            "Which change fixes the root cause?",
            json!({ "repository": "fixture", "revision": "abc123" }),
        );
        let renamed_request = build_blind_judge_request(
            &reordered_and_repeated,
            "claim:tests-pass",
            "The tests pass",
            "Which change fixes the root cause?",
            json!({ "repository": "fixture", "revision": "abc123" }),
        );

        assert_eq!(
            serde_json::to_value(baseline_request).expect("baseline judge request serializes"),
            serde_json::to_value(renamed_request).expect("renamed judge request serializes")
        );
    }

    impl MockParallelModelSource {
        fn frame_rows() -> Vec<(String, Vec<String>)> {
            ["a", "b", "c"].into_iter().map(|seat| (
                format!("candidate-{seat}"),
                vec![
                    json!({"type":"output_text_delta", "delta":"ignored delta"}).to_string(),
                    json!({"type":"output_item_done","item":{
                        "type":"message","role":"assistant","phase":"final_answer",
                        "content":[{"type":"output_text","text":
                            claims_answer(&format!("runner-{seat}"), "supports", None).to_string()}]
                    }}).to_string(),
                    json!({"type":"completed","responseId":format!("response-{seat}"),"endTurn":true}).to_string(),
                ],
            )).collect()
        }
    }

    fn fail_verifier_fixture() -> (
        Delivery,
        FrozenDeliveryCandidate,
        ComputedDeliveryVerdict,
        Vec<ResolvedDeliveryEvidence>,
    ) {
        let fixture = verdict_fixture(
            &DeliveryId("dlv_01J00000000000000000000001".to_owned()),
            VerdictFixtureOutcome::Fail,
        );
        let computed = compute_delivery_verdict(
            &fixture.delivery,
            &fixture.candidate,
            &fixture.verification,
            &fixture.evidence,
            PRODUCED_AT_MILLIS,
        )
        .expect("failed verifier fixture computes");
        assert_eq!(
            computed.verdict().status,
            winwincode_delivery::domain::CriterionVerdict::Fail
        );
        (
            fixture.delivery,
            fixture.candidate,
            computed,
            fixture.evidence,
        )
    }

    #[test]
    fn build_fusion_input_keeps_application_question_and_candidates() {
        let input = sample_input(200);
        assert_eq!(input.question, "Which change fixes the root cause?");
        assert_eq!(input.provider_candidates.len(), 3);
        assert_eq!(input.budget.max_total_tokens, Some(2_000));
        assert!(input.expected_output_schema.is_object());
    }

    #[test]
    fn claim_extractor_reads_isolated_answer_without_sibling_input() {
        let isolated = claims_answer("solo", "supports", None);
        let claims = extract_claims_from_answer("candidate-solo", &isolated)
            .expect("isolated answer extracts");
        assert_eq!(claims.candidate_id, "candidate-solo");
        assert_eq!(claims.claims.len(), 1);
        assert_eq!(claims.claims[0].claim_key, "claim:tests-pass");
        assert_eq!(claims.claims[0].position, FusionClaimPosition::Supports);
        assert!(claims.claims[0].evidence[0].verified_conclusion.is_none());
    }

    #[tokio::test]
    async fn blind_panel_compose_keeps_isolation_partial_failure_and_host_analysis() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let barrier = Arc::new(Barrier::new(3));
        let mut map = MapFusionProviderRouter::new();
        for (name, behavior) in [
            ("provider-a", MockBehavior::Answer("panel-a")),
            ("provider-b", MockBehavior::Fail),
            ("provider-c", MockBehavior::Answer("panel-c")),
        ] {
            map = map.with(
                name,
                Arc::new(MockFusionProvider {
                    name,
                    behavior,
                    barrier: Arc::clone(&barrier),
                    requests: Arc::clone(&requests),
                }),
            );
        }
        let router: Arc<dyn FusionProviderRouter> = Arc::new(map);

        let composed = compose_blind_panel("panel-compose", sample_input(300), router)
            .await
            .expect("panel compose succeeds with partial failure");

        let panel = composed.panel.as_ref().expect("panel record present");
        let collected = compose_collected_panel(
            "panel-compose",
            &sample_input(300),
            serde_json::from_slice(&serde_json::to_vec(panel).unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(collected.analysis, composed.analysis);
        assert_eq!(collected.claims, composed.claims);
        assert_eq!(collected.panel.as_ref(), Some(panel));
        assert_eq!(panel.candidates.len(), 2);
        assert_eq!(panel.failures.len(), 1);
        assert_eq!(panel.failures[0].candidate_id, "candidate-b");
        assert_eq!(composed.answers.len(), 2);
        assert_eq!(composed.claims.len(), 2);
        assert!(composed.claims.iter().all(|row| row.claims.len() == 1));

        let captured = requests.lock().expect("request log");
        assert_eq!(captured.len(), 3);
        let prompt_keys: Vec<Vec<String>> = captured
            .iter()
            .map(|request| {
                let value = serde_json::to_value(&request.prompt).expect("prompt value");
                value
                    .as_object()
                    .expect("object prompt")
                    .keys()
                    .cloned()
                    .collect()
            })
            .collect();
        for keys in &prompt_keys {
            assert_eq!(
                keys,
                &[
                    "canonicalContext".to_owned(),
                    "constraints".to_owned(),
                    "expectedOutputSchema".to_owned(),
                    "question".to_owned(),
                ]
            );
        }
        let request_ids: std::collections::HashSet<String> = captured
            .iter()
            .map(|request| request.request_id.clone())
            .collect();
        assert_eq!(request_ids.len(), 3);
        drop(captured);

        assert_eq!(composed.analysis.consensus.len(), 1);
        assert_eq!(
            composed.analysis.consensus[0].candidate_ids,
            vec!["candidate-a".to_owned(), "candidate-c".to_owned()]
        );

        let (delivery, candidate, computed, evidence) = fail_verifier_fixture();
        let decision = adjudicate_composed_fusion(
            &delivery,
            &candidate,
            Some(&computed),
            &evidence,
            &composed,
        )
        .expect("composed host adjudication");
        assert_eq!(
            decision.outcome(),
            winwincode_delivery::domain::CriterionVerdict::Fail
        );
        assert_eq!(decision.authority(), DecisionAuthority::VerifiedFact);
        assert_ne!(decision.authority(), DecisionAuthority::ModelConsensus);
        assert!(decision.canonical_verdict_id().is_some());
    }

    #[tokio::test]
    async fn parallel_runner_frame_rows_compose_into_host_chain() {
        let rows = MockParallelModelSource::frame_rows();
        let answers = answers_from_parallel_model_frames(rows).expect("frames convert to answers");
        assert_eq!(answers.len(), 3);
        assert_eq!(answers[0].candidate_id, "candidate-a");

        let seats = answers
            .iter()
            .map(|answer| FusionRunnerSeat {
                candidate_id: answer.candidate_id.clone(),
                routes: vec![format!("runtime/{}", answer.candidate_id)],
            })
            .collect::<Vec<_>>();
        let runner = Arc::new(FunctionFusionRunnerAnswerPort::new(answers.clone()));
        let composed = compose_via_runner_port(sample_input(300), seats, runner)
            .await
            .expect("runner-port compose");

        assert!(composed.panel.is_none());
        assert_eq!(composed.answers.len(), 3);
        assert_eq!(composed.claims.len(), 3);
        assert_eq!(composed.analysis.consensus.len(), 1);

        let fixture = verdict_fixture(
            &DeliveryId("dlv_01J00000000000000000000002".to_owned()),
            VerdictFixtureOutcome::Pass,
        );
        let decision =
            adjudicate_composed_fusion(&fixture.delivery, &fixture.candidate, None, &[], &composed)
                .expect("runner compose adjudicates");
        assert_eq!(
            decision.outcome(),
            winwincode_delivery::domain::CriterionVerdict::Pass
        );
        assert_eq!(decision.authority(), DecisionAuthority::ModelConsensus);
        assert!(decision.canonical_verdict_id().is_none());
        assert_ne!(decision.authority(), DecisionAuthority::VerifiedFact);
        assert!(LIVE_MODEL_PORT_PLUG_IN_NOTE.contains("ParallelModelRunner"));
    }

    #[tokio::test]
    async fn panel_and_runner_paths_share_the_same_analysis_contract() {
        let panel_composed = compose_blind_panel(
            "panel-shared",
            sample_input(300),
            mock_model_port_like_router(),
        )
        .await
        .expect("panel compose");
        let runner_answers = vec![
            ComposedFusionAnswer {
                candidate_id: "candidate-a".to_owned(),
                answer: claims_answer("panel-a", "supports", None),
            },
            ComposedFusionAnswer {
                candidate_id: "candidate-b".to_owned(),
                answer: claims_answer("panel-b", "supports", None),
            },
            ComposedFusionAnswer {
                candidate_id: "candidate-c".to_owned(),
                answer: claims_answer("panel-c", "supports", None),
            },
        ];
        let runner_composed =
            compose_independent_answers(runner_answers).expect("runner answers compose");

        assert_eq!(
            panel_composed.analysis.consensus.len(),
            runner_composed.analysis.consensus.len()
        );
        assert_eq!(panel_composed.claims.len(), runner_composed.claims.len());
        assert!(panel_composed.panel.is_some());
        assert!(runner_composed.panel.is_none());
    }

    #[test]
    fn compose_rejects_empty_answers_and_invalid_claim_payloads() {
        let empty =
            compose_independent_answers(Vec::new()).expect_err("empty answers are rejected");
        assert_eq!(empty, FusionComposeError::NoSuccessfulAnswers);
        let invalid = vec![ComposedFusionAnswer {
            candidate_id: "candidate-bad".to_owned(),
            answer: json!({ "narrative": "no claims" }),
        }];
        let error = compose_independent_answers(invalid).expect_err("invalid claims rejected");
        assert!(matches!(
            error,
            FusionComposeError::ClaimExtraction { candidate_id, .. } if candidate_id == "candidate-bad"
        ));
    }

    #[test]
    fn parallel_frames_without_answer_are_rejected() {
        let rows = vec![(
            "candidate-x".to_owned(),
            vec![r#"{"type":"chunk","text":"partial"}"#.to_owned()],
        )];
        let error = answers_from_parallel_model_frames(rows).expect_err("missing answer rejected");
        assert!(matches!(
            error,
            FusionComposeError::ClaimExtraction { candidate_id, .. } if candidate_id == "candidate-x"
        ));
    }

    #[test]
    fn parallel_answer_requires_completed_final_message() {
        let (_, frames) = MockParallelModelSource::frame_rows().remove(0);
        let expected = claims_answer("runner-a", "supports", None);
        assert_eq!(answer_from_frames(&frames), Some(expected));
        assert!(answer_from_frames(&frames[..2]).is_none());
        let mut progress = frames.clone();
        progress[1] = progress[1].replace("final_answer", "commentary");
        assert!(answer_from_frames(&progress).is_none());
        let mut failed = frames.clone();
        failed[2] = json!({"type":"error","error":{"code":"FAILED"}}).to_string();
        assert!(answer_from_frames(&failed).is_none());
        let mut unfinished = frames.clone();
        unfinished[2] = json!({"type":"completed","endTurn":false}).to_string();
        assert!(answer_from_frames(&unfinished).is_none());
        let mut trailing = frames.clone();
        trailing.push(frames[1].clone());
        assert!(answer_from_frames(&trailing).is_none());
        assert!(
            answer_from_frames(&[json!({"type":"completed","answer":{}}).to_string()]).is_none()
        );
    }

    #[tokio::test]
    async fn compose_multiround_blocks_jev_without_new_evidence() {
        use crate::fusion_analysis::{
            FusionNextAction, FusionRound, FusionStopReason, fusion_next_action,
        };

        let router = Arc::new(MapRouter {
            p1: Arc::new(Seat {
                position: "supports",
            }),
            p2: Arc::new(Seat {
                position: "opposes",
            }),
            p3: Arc::new(Seat {
                position: "opposes",
            }),
        });
        let input = sample_input(300);
        let (composed, report) = compose_multiround(
            "panel-multiround",
            input,
            router,
            Arc::new(NoNewEvidence),
            Arc::new(MustNotJudge),
            Some(1),
        )
        .await
        .expect("multi-round compose");
        assert_eq!(report.stop_reason, FusionStopReason::BudgetLimit);
        assert!(report.judged.is_empty());
        assert_eq!(
            fusion_next_action(&composed.analysis, FusionRound::Discovery, false, false),
            FusionNextAction::TargetedInvestigation {
                claim_keys: vec!["claim:tests-pass".to_owned()]
            }
        );
    }

    #[tokio::test]
    async fn jev_unavailable_preserves_composed_state_and_reaches_recoverable_unresolved() {
        for code in ["JEV_TIMEOUT", "JEV_TRANSPORT", "JEV_INVALID_RESPONSE"] {
            let mut composed = compose_independent_answers(vec![
                ComposedFusionAnswer {
                    candidate_id: "candidate-a".to_owned(),
                    answer: claims_answer("panel-a", "supports", None),
                },
                ComposedFusionAnswer {
                    candidate_id: "candidate-b".to_owned(),
                    answer: claims_answer("panel-b", "opposes", None),
                },
            ])
            .expect("fixture answers compose");
            let before_claims = composed.claims.clone();
            let before_answers = composed.answers.clone();
            let before_analysis = composed.analysis.clone();
            let mut rounds = Vec::new();
            let mut judged = Vec::new();
            let mut jev_calls = Vec::new();
            let mut unavailable = Vec::new();
            let judge: Arc<dyn FusionJudgePort> = Arc::new(UnavailableJudge { code });

            let control = run_verification(
                &mut composed,
                &mut rounds,
                &mut judged,
                &mut jev_calls,
                vec!["claim:tests-pass".to_owned()],
                &sample_input(310),
                &judge,
                &EvidenceLedger::default(),
                &BTreeMap::new(),
                &mut unavailable,
            )
            .await
            .expect("JEV outage is non-destructive");

            assert!(matches!(
                control,
                RoundControl::Stop(FusionStopReason::Unresolvable)
            ));
            assert_eq!(unavailable.len(), 1);
            assert_eq!(unavailable[0].code, code);
            assert_eq!(unavailable[0].claim_id, "claim:tests-pass");
            assert!(judged.is_empty());
            assert!(jev_calls.is_empty());
            assert_eq!(composed.claims, before_claims);
            assert_eq!(composed.answers, before_answers);
            assert_eq!(composed.analysis, before_analysis);
        }
    }
}
