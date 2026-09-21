// SPDX-License-Identifier: Apache-2.0

//! Control-plane composition seam for FUSION-01 blind panel and FUSION-02
//! parallel runner answers into the existing Fusion host chain.
//!
//! Composition path (one-way, no delivery → control-plane cycle):
//!
//! ```text
//! application question + canonical context + providerCandidates
//!   → FusionInput
//!   → FUSION-01 run_blind_panel  and/or  FUSION-02 independent answers
//!   → ComposedFusionAnswer (isolated per candidate)
//!   → FusionCandidateClaims
//!   → analyze_fusion (FUSION-03)
//!   → FusionAnalysisFixture mapping (fusion_adjudication_host)
//!   → adjudicate_canonical_decision_from_analysis (FUSION-04)
//!   → CanonicalDecision (read-only projection)
//! ```
//!
//! Isolation and invincible-verifier rules held by this seam:
//! - Each candidate answer is extracted into claims independently. Sibling
//!   answers never cross a candidate boundary here or in the panel runner.
//! - Compose never raises mapped model claims above the model-claim tier.
//! - Compose never writes Canonical State and never invents a verifier pass.
//! - Failed panel candidates and failed FUSION-02 targets are omitted from
//!   claims; they never cancel successful siblings.
//!
//! Live `ModelPort` / Provider Runtime plug-in later:
//! - FUSION-01 production: implement [`winwincode_fusion::FusionProvider`] /
//!   [`winwincode_fusion::FusionProviderRouter`] adapters that wrap the
//!   Device Provider Runtime. The panel boundary already isolates credentials
//!   from this seam.
//! - FUSION-02 production: run `winwincode_codex::ParallelModelRunner` over
//!   kernel `ModelPort`, then feed succeeded `(target_id, frames)` rows into
//!   [`answers_from_parallel_model_frames`] or implement
//!   [`FusionRunnerAnswerPort`] with that runner. `target_id` must equal the
//!   Fusion `candidate_id`.
//! - Tests inject mock Providers / mock runner ports only.

use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::Value;
use winwincode_delivery::domain::{
    CanonicalDecision, ComputedDeliveryVerdict, Delivery, DeliveryValidationError, EvidenceRefType,
    FrozenDeliveryCandidate, evidence::ResolvedDeliveryEvidence,
    verification::VerificationFindingConclusion,
};
use winwincode_fusion::{
    FusionBudget, FusionCandidate, FusionInput, FusionPanelResult, FusionProviderCandidate,
    FusionProviderRouter, run_blind_panel,
};

use crate::fusion_adjudication_host::adjudicate_canonical_decision_from_fusion_analysis;
use crate::fusion_analysis::{
    FusionAnalysis, FusionAnalysisError, FusionCandidateClaims, FusionClaim, FusionClaimPosition,
    FusionEvidence, analyze_fusion,
};

/// One independently collected model answer consumed by the compose seam.
///
/// `candidate_id` is the Fusion panel candidate id and the FUSION-02
/// `ParallelModelTarget::target_id`.
#[derive(Clone, Debug, PartialEq)]
pub struct ComposedFusionAnswer {
    pub candidate_id: String,
    pub answer: Value,
}

/// One FUSION-02 runner seat accepted by the portable runner port.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FusionRunnerSeat {
    pub candidate_id: String,
    /// Ordered Provider Runtime routes. First is primary; later routes fall back.
    pub routes: Vec<String>,
}

/// Portable host-side port for FUSION-02 independent answers.
///
/// Production wraps `winwincode_codex::ParallelModelRunner` over kernel
/// `ModelPort`. Tests inject mock ports that never open Provider connections.
pub trait FusionRunnerAnswerPort: fmt::Debug + Send + Sync {
    /// Runs blind independent answers for every seat.
    fn run_blind_answers(
        &self,
        input: &FusionInput,
        seats: &[FusionRunnerSeat],
    ) -> BoxFuture<'static, Result<Vec<ComposedFusionAnswer>, FusionComposeError>>;
}

/// Compose-seam failure before host adjudication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FusionComposeError {
    /// FUSION-01 rejected the canonical panel input.
    Panel(winwincode_fusion::FusionPanelError),
    /// FUSION-03 rejected the extracted claim fixture.
    Analysis(FusionAnalysisError),
    /// An answer could not be converted into independent claims.
    ClaimExtraction {
        candidate_id: String,
        message: String,
    },
    /// No successful independent answer remained for analysis.
    NoSuccessfulAnswers,
    /// The portable runner port failed.
    Runner(String),
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
        }
    }
}

impl std::error::Error for FusionComposeError {}

/// Full compose product handed to the existing host adjudication path.
#[derive(Clone, Debug)]
pub struct ComposedFusionOutcome {
    /// Panel record when the FUSION-01 blind panel path ran.
    pub panel: Option<FusionPanelResult>,
    /// Independent answers that contributed claims (panel successes and/or
    /// FUSION-02 runner successes).
    pub answers: Vec<ComposedFusionAnswer>,
    /// Isolated per-candidate claims used for analysis.
    pub claims: Vec<FusionCandidateClaims>,
    /// FUSION-03 pure analysis.
    pub analysis: FusionAnalysis,
}

/// Builds the canonical FUSION-01/02 panel input from application facts.
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

/// Default claim schema object used when an answer does not carry claims.
#[must_use]
pub fn default_claim_output_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "claims": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "claimKey": { "type": "string" },
                        "summary": { "type": "string" },
                        "position": { "type": "string", "enum": ["supports", "opposes"] },
                        "evidence": { "type": "array" },
                        "requiredEvidence": { "type": "array" }
                    },
                    "required": ["claimKey", "summary", "position"],
                    "additionalProperties": true
                }
            }
        },
        "required": ["claims"],
        "additionalProperties": true
    })
}

/// Extracts one candidate's claims from an isolated answer payload.
///
/// Expected answer shape (`camelCase` or `snake_case` accepted):
/// `{ "claims": [{ "claimKey", "summary", "position", "evidence"?, "requiredEvidence"? }] }`
///
/// Extraction is purely local to `answer`. Sibling outputs are never an input.
///
/// # Errors
///
/// Returns [`FusionComposeError::ClaimExtraction`] when the answer is not an
/// object, has no parseable `claims` array, or a claim row is invalid.
pub fn extract_claims_from_answer(
    candidate_id: &str,
    answer: &Value,
) -> Result<FusionCandidateClaims, FusionComposeError> {
    let fail = |message: &str| FusionComposeError::ClaimExtraction {
        candidate_id: candidate_id.to_owned(),
        message: message.to_owned(),
    };
    let object = answer
        .as_object()
        .ok_or_else(|| fail("answer must be a JSON object"))?;
    let claims_value = object
        .get("claims")
        .or_else(|| object.get("Claims"))
        .ok_or_else(|| fail("answer is missing claims"))?;
    let claim_rows = claims_value
        .as_array()
        .ok_or_else(|| fail("claims must be an array"))?;
    if claim_rows.is_empty() {
        return Err(fail("claims must not be empty"));
    }

    let mut claims = Vec::with_capacity(claim_rows.len());
    for row in claim_rows {
        claims.push(parse_claim(row).map_err(|message| fail(&message))?);
    }
    Ok(FusionCandidateClaims {
        candidate_id: candidate_id.to_owned(),
        claims,
    })
}

fn parse_claim(row: &Value) -> Result<FusionClaim, String> {
    let claim_key = required_text(row, "claimKey", "claim_key")?;
    let summary = required_text(row, "summary", "summary")?;
    let position = parse_position(
        row.get("position")
            .or_else(|| row.get("Position"))
            .and_then(Value::as_str)
            .ok_or("claim position is required")?,
    )?;
    let mut evidence = Vec::new();
    if let Some(rows) = row.get("evidence").or_else(|| row.get("Evidence")) {
        let rows = rows.as_array().ok_or("claim evidence must be an array")?;
        for item in rows {
            evidence.push(parse_evidence(item)?);
        }
    }
    let mut required_evidence = Vec::new();
    if let Some(types) = row
        .get("requiredEvidence")
        .or_else(|| row.get("required_evidence"))
    {
        let types = types
            .as_array()
            .ok_or("requiredEvidence must be an array")?;
        for item in types {
            let text = item
                .as_str()
                .ok_or("requiredEvidence entries must be strings")?;
            required_evidence.push(parse_evidence_type(text)?);
        }
    }
    Ok(FusionClaim {
        claim_key,
        summary,
        position,
        evidence,
        required_evidence,
    })
}

fn required_text(row: &Value, camel: &str, snake: &str) -> Result<String, String> {
    row.get(camel)
        .or_else(|| row.get(snake))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("{camel} is required"))
}

fn parse_position(position: &str) -> Result<FusionClaimPosition, String> {
    match position.trim().to_ascii_lowercase().as_str() {
        "supports" | "support" => Ok(FusionClaimPosition::Supports),
        "opposes" | "oppose" => Ok(FusionClaimPosition::Opposes),
        other => Err(format!("unsupported claim position: {other}")),
    }
}

fn parse_evidence(item: &Value) -> Result<FusionEvidence, String> {
    let evidence_type = parse_evidence_type(
        item.get("evidenceType")
            .or_else(|| item.get("evidence_type"))
            .and_then(Value::as_str)
            .ok_or("evidenceType is required")?,
    )?;
    let source_ref = required_text(item, "sourceRef", "source_ref")?;
    let verified_conclusion = item
        .get("verifiedConclusion")
        .or_else(|| item.get("verified_conclusion"))
        .and_then(Value::as_str)
        .map(
            |conclusion| match conclusion.trim().to_ascii_lowercase().as_str() {
                "pass" => Ok(VerificationFindingConclusion::Pass),
                "fail" => Ok(VerificationFindingConclusion::Fail),
                other => Err(format!("unsupported verifiedConclusion: {other}")),
            },
        )
        .transpose()?;
    Ok(FusionEvidence {
        evidence_type,
        source_ref,
        verified_conclusion,
    })
}

fn parse_evidence_type(text: &str) -> Result<EvidenceRefType, String> {
    let normalized = text.trim().to_ascii_lowercase();
    let json = Value::String(normalized);
    serde_json::from_value(json).map_err(|_| format!("unsupported evidence type: {text}"))
}

/// Converts isolated answers into claim rows and runs FUSION-03 analysis.
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

/// FUSION-01 path: build-independent panel answers → claims → analysis.
///
/// # Errors
///
/// Returns panel-input rejection, claim-extraction failure, or analyzer
/// rejection. Provider timeouts/failures stay isolated panel failure rows and
/// are not treated as compose errors when at least one sibling succeeds.
pub async fn compose_blind_panel(
    panel_id: &str,
    input: FusionInput,
    router: Arc<dyn FusionProviderRouter>,
) -> Result<ComposedFusionOutcome, FusionComposeError> {
    let panel = run_blind_panel(panel_id, input, router)
        .await
        .map_err(FusionComposeError::Panel)?;
    let answers = answers_from_panel_candidates(&panel.candidates);
    let (claims, analysis) = claims_and_analysis(&answers)?;
    Ok(ComposedFusionOutcome {
        panel: Some(panel),
        answers,
        claims,
        analysis,
    })
}

/// Maps successful panel candidates onto independent compose answers.
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

/// FUSION-02 portable path: already-collected independent answers → claims → analysis.
///
/// # Errors
///
/// Returns claim-extraction or analyzer rejection. Failed runner targets must
/// simply be omitted from `answers`.
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

/// FUSION-02 host path through a portable runner port (mock `ModelPort` ok).
///
/// # Errors
///
/// Returns runner-port failure, claim-extraction failure, analyzer rejection,
/// or [`FusionComposeError::NoSuccessfulAnswers`] when every seat failed.
pub async fn compose_via_runner_port(
    input: FusionInput,
    seats: Vec<FusionRunnerSeat>,
    runner: Arc<dyn FusionRunnerAnswerPort>,
) -> Result<ComposedFusionOutcome, FusionComposeError> {
    let answers = runner.run_blind_answers(&input, &seats).await?;
    compose_independent_answers(answers)
}

/// Converts FUSION-02 `(target_id, frames)` rows into compose answers.
///
/// Production callers pass `ParallelModelRunner` succeeded-target frames.
/// The last JSON object frame that exposes `answer` / `Answer` (or a bare
/// object payload) becomes the candidate answer. `target_id` is the
/// Fusion `candidate_id`.
///
/// # Errors
///
/// Returns [`FusionComposeError::ClaimExtraction`] when a row has no usable
/// answer frame.
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

fn answer_from_frames(frames: &[String]) -> Option<Value> {
    for frame in frames.iter().rev() {
        let Ok(value) = serde_json::from_str::<Value>(frame) else {
            continue;
        };
        if let Some(answer) = value.get("answer").or_else(|| value.get("Answer")) {
            return Some(answer.clone());
        }
        if value.is_object() && value.get("type").is_none() {
            return Some(value);
        }
    }
    None
}

/// Host adjudication for a composed Fusion outcome.
///
/// Maps composed analysis through the existing FUSION-03 → fixture → FUSION-04
/// host path. Verifier real results remain invincible. This function adds no
/// Canonical State write power.
///
/// # Errors
///
/// Returns the same stale repository, canonical, verifier, or Evidence
/// rejection as the delivery adjudicator.
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

/// Default [`FusionRunnerAnswerPort`] adapter over independent answers.
///
/// Production replaces this with a `ParallelModelRunner` + kernel `ModelPort`
/// adapter; tests inject mock answers or mock ports only.
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

/// Documents the live `ModelPort` plug-in point without importing kernel types.
///
/// Production shape:
/// `ParallelModelRunner::new(Arc<dyn ModelPort>)` → `run(targets, budget, cancel)`
/// → succeeded `ParallelModelResult { target_id, frames }` →
/// [`answers_from_parallel_model_frames`] → [`compose_independent_answers`].
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
                candidate_timeout_millis: timeout_millis,
                max_total_tokens: 2_000,
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

    /// Stand-in for kernel ModelPort used only to document the FUSION-02
    /// frame shape that `answers_from_parallel_model_frames` consumes.
    #[derive(Debug)]
    struct MockParallelModelSource;

    impl MockParallelModelSource {
        fn frame_rows(&self) -> Vec<(String, Vec<String>)> {
            vec![
                (
                    "candidate-a".to_owned(),
                    vec![
                        r#"{"type":"chunk","text":"thinking"}"#.to_owned(),
                        format!(
                            r#"{{"type":"completed","answer":{}}}"#,
                            claims_answer("runner-a", "supports", None)
                        ),
                    ],
                ),
                (
                    "candidate-b".to_owned(),
                    vec![format!(
                        r#"{{"type":"completed","answer":{}}}"#,
                        claims_answer("runner-b", "supports", None)
                    )],
                ),
                (
                    "candidate-c".to_owned(),
                    vec![format!(
                        r#"{{"type":"completed","answer":{}}}"#,
                        claims_answer("runner-c", "supports", None)
                    )],
                ),
            ]
        }
    }

    fn fail_verifier_fixture() -> (
        winwincode_delivery::domain::Delivery,
        winwincode_delivery::domain::FrozenDeliveryCandidate,
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
        assert_eq!(input.budget.max_total_tokens, 2_000);
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
        // Rebuild router with shared request log for isolation assertions.
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
        assert_eq!(panel.candidates.len(), 2);
        assert_eq!(panel.failures.len(), 1);
        assert_eq!(panel.failures[0].candidate_id, "candidate-b");
        assert_eq!(composed.answers.len(), 2);
        assert_eq!(composed.claims.len(), 2);
        assert!(composed.claims.iter().all(|row| row.claims.len() == 1));

        // Isolation: every Provider request saw only the blind prompt fields.
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

        // Analyzer sees two independent Supports claims on the same key.
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
        // FUSION-02 stand-in: frames shaped like ParallelModelRunner successes.
        let rows = MockParallelModelSource.frame_rows();
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

        // Model-only consensus stays at ModelConsensus and does not write Canonical State.
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
}
