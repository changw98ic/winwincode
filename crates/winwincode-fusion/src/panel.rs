// SPDX-License-Identifier: Apache-2.0

//! Parallel blind panel runner.
//!
//! Guarantees encoded here:
//! - at least three distinct Providers must be present in the canonical input;
//! - every candidate receives the same blind prompt in an isolated request;
//! - candidates are sent in parallel;
//! - timeout, Provider failure, missing route, invalid answer, or token
//!   overrun becomes one failure row and never cancels successful siblings.

use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use futures::StreamExt as _;
use futures::stream::FuturesUnordered;
use sha2::Digest as _;
use sha2::Sha256;
use tokio::time::timeout;

use crate::FusionBlindPrompt;
use crate::FusionCandidate;
use crate::FusionCandidateAudit;
use crate::FusionCandidateFailure;
use crate::FusionInput;
use crate::FusionPanelResult;
use crate::FusionProviderCandidate;
use crate::FusionProviderError;
use crate::FusionProviderRequest;
use crate::FusionProviderRouter;
use crate::FusionTokenUsage;
use crate::MAX_PROVIDER_COUNT;
use crate::MIN_PROVIDER_COUNT;

const MAX_TEXT_BYTES: usize = 256 * 1024;
const MAX_CONSTRAINTS: usize = 128;

/// Invalid canonical input detected before any Provider request is sent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FusionPanelError(&'static str);

impl fmt::Display for FusionPanelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for FusionPanelError {}

/// Sends the same blind prompt to every candidate route in parallel.
///
/// # Errors
///
/// Returns a typed panel error only for invalid canonical input. Runtime
/// Provider failures are retained as [`FusionCandidateFailure`] rows.
pub async fn run_blind_panel(
    panel_id: &str,
    input: FusionInput,
    router: Arc<dyn FusionProviderRouter>,
) -> Result<FusionPanelResult, FusionPanelError> {
    validate_input(panel_id, &input)?;
    let prompt = blind_prompt(&input);
    let input_digest = digest_prompt(&prompt)?;
    let dispatch = PanelDispatch {
        panel_id: panel_id.to_owned(),
        input_digest,
        timeout: Duration::from_millis(input.budget.candidate_timeout_millis),
        max_total_tokens: input.budget.max_total_tokens,
        prompt,
    };
    let mut pending = FuturesUnordered::new();
    for route in &input.provider_candidates {
        pending.push(dispatch_candidate(dispatch.clone(), route.clone(), &router));
    }

    let mut candidates = Vec::new();
    let mut failures = Vec::new();
    while let Some(outcome) = pending.next().await {
        match outcome {
            CandidateOutcome::Candidate(candidate) => candidates.push(candidate),
            CandidateOutcome::Failure(failure) => failures.push(failure),
        }
    }
    candidates.sort_by(|left, right| left.audit.candidate_id.cmp(&right.audit.candidate_id));
    failures.sort_by(|left, right| left.candidate_id.cmp(&right.candidate_id));
    Ok(FusionPanelResult {
        panel_id: panel_id.to_owned(),
        input_digest: dispatch.input_digest,
        candidates,
        failures,
    })
}

enum CandidateOutcome {
    Candidate(FusionCandidate),
    Failure(FusionCandidateFailure),
}

#[derive(Clone)]
struct PanelDispatch {
    panel_id: String,
    input_digest: String,
    timeout: Duration,
    max_total_tokens: u64,
    prompt: FusionBlindPrompt,
}

fn blind_prompt(input: &FusionInput) -> FusionBlindPrompt {
    FusionBlindPrompt {
        question: input.question.clone(),
        canonical_context: input.canonical_context.clone(),
        constraints: input.constraints.clone(),
        expected_output_schema: input.expected_output_schema.clone(),
    }
}

fn digest_prompt(prompt: &FusionBlindPrompt) -> Result<String, FusionPanelError> {
    let prompt_bytes = serde_json::to_vec(prompt)
        .map_err(|_| FusionPanelError("Fusion input could not be serialized"))?;
    Ok(sha256(&prompt_bytes))
}

fn dispatch_candidate(
    dispatch: PanelDispatch,
    route: FusionProviderCandidate,
    router: &Arc<dyn FusionProviderRouter>,
) -> impl Future<Output = CandidateOutcome> + Send {
    let request = provider_request(&dispatch, &route);
    let request_payload_digest =
        sha256(serde_json::to_vec(&request).unwrap_or_default().as_slice());
    let provider = router.resolve(&route.provider);
    async move {
        let started = Instant::now();
        let request_id = request.request_id.clone();
        let result = match provider {
            None => Err(FusionProviderError::new(
                "PROVIDER_UNRESOLVED",
                "Fusion router has no adapter for this Provider",
            )),
            Some(adapter) => timeout(dispatch.timeout, adapter.complete(request))
                .await
                .unwrap_or_else(|_| {
                    Err(FusionProviderError::new(
                        "TIMEOUT",
                        "Fusion candidate exceeded its wall-time budget",
                    ))
                }),
        };
        let elapsed_millis = elapsed_millis(started);
        let identity = FailureIdentity {
            panel_id: dispatch.panel_id,
            candidate_id: route.id.clone(),
            request_id,
            input_digest: dispatch.input_digest,
            request_payload_digest,
            provider: route.provider.clone(),
            model: route.model.clone(),
            elapsed_millis,
        };
        match result {
            Ok(answer) => {
                if exceeds_token_budget(answer.token_usage, dispatch.max_total_tokens) {
                    return CandidateOutcome::Failure(identity.into_failure(
                        "TOKEN_BUDGET_EXCEEDED",
                        "Fusion candidate exceeded its token budget",
                    ));
                }
                CandidateOutcome::Candidate(FusionCandidate {
                    audit: identity.into_audit(
                        route.id,
                        answer.provider_response_id,
                        answer.token_usage,
                    ),
                    answer: answer.answer,
                })
            }
            Err(error) => {
                CandidateOutcome::Failure(identity.into_failure(error.code(), error.message()))
            }
        }
    }
}

fn exceeds_token_budget(usage: Option<FusionTokenUsage>, max_total_tokens: u64) -> bool {
    usage.is_some_and(|usage| {
        usage.total_tokens > max_total_tokens
            || usage.input_tokens.saturating_add(usage.output_tokens) > max_total_tokens
    })
}

struct FailureIdentity {
    panel_id: String,
    candidate_id: String,
    request_id: String,
    input_digest: String,
    request_payload_digest: String,
    provider: String,
    model: String,
    elapsed_millis: u64,
}

impl FailureIdentity {
    fn into_failure(
        self,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> FusionCandidateFailure {
        FusionCandidateFailure {
            panel_id: self.panel_id,
            candidate_id: self.candidate_id,
            request_id: self.request_id,
            input_digest: self.input_digest,
            request_payload_digest: self.request_payload_digest,
            provider: self.provider,
            model: self.model,
            code: code.into(),
            message: message.into(),
            elapsed_millis: self.elapsed_millis,
        }
    }

    fn into_audit(
        self,
        candidate_id: String,
        provider_response_id: String,
        token_usage: Option<FusionTokenUsage>,
    ) -> FusionCandidateAudit {
        FusionCandidateAudit {
            panel_id: self.panel_id,
            candidate_id,
            request_id: self.request_id,
            input_digest: self.input_digest,
            request_payload_digest: self.request_payload_digest,
            provider: self.provider,
            model: self.model,
            provider_response_id,
            token_usage,
            elapsed_millis: self.elapsed_millis,
        }
    }
}

fn provider_request(
    dispatch: &PanelDispatch,
    route: &FusionProviderCandidate,
) -> FusionProviderRequest {
    let identity = sha256(
        format!(
            "{}\0{}\0{}",
            dispatch.panel_id, route.id, dispatch.input_digest
        )
        .as_bytes(),
    );
    let request_id = format!("fusion-{}", &identity[7..39]);
    FusionProviderRequest {
        panel_id: dispatch.panel_id.clone(),
        candidate_id: route.id.clone(),
        request_id,
        provider: route.provider.clone(),
        model: route.model.clone(),
        reasoning_effort: route.reasoning_effort.clone(),
        prompt: dispatch.prompt.clone(),
        max_total_tokens: dispatch.max_total_tokens,
    }
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn validate_input(panel_id: &str, input: &FusionInput) -> Result<(), FusionPanelError> {
    validate_text(panel_id)?;
    validate_text(&input.question)?;
    if !input.expected_output_schema.is_object() {
        return Err(FusionPanelError(
            "Fusion expected output schema must be an object",
        ));
    }
    if input.constraints.len() > MAX_CONSTRAINTS {
        return Err(FusionPanelError("Fusion constraints exceed the bound"));
    }
    for constraint in &input.constraints {
        validate_text(constraint)?;
    }
    if !(MIN_PROVIDER_COUNT..=MAX_PROVIDER_COUNT).contains(&input.provider_candidates.len()) {
        return Err(FusionPanelError(
            "Fusion panels require three to sixteen Providers",
        ));
    }
    if input.budget.candidate_timeout_millis == 0 || input.budget.max_total_tokens == 0 {
        return Err(FusionPanelError("Fusion budget limits must be positive"));
    }
    let mut ids = HashSet::new();
    let mut providers = HashSet::new();
    for candidate in &input.provider_candidates {
        validate_text(&candidate.id)?;
        validate_text(&candidate.provider)?;
        validate_text(&candidate.model)?;
        if let Some(effort) = candidate.reasoning_effort.as_deref() {
            validate_text(effort)?;
        }
        if !ids.insert(candidate.id.as_str()) {
            return Err(FusionPanelError("Fusion candidate ids must be unique"));
        }
        providers.insert(candidate.provider.as_str());
    }
    if providers.len() < MIN_PROVIDER_COUNT {
        return Err(FusionPanelError(
            "Fusion panels require at least three distinct Providers",
        ));
    }
    Ok(())
}

fn validate_text(value: &str) -> Result<(), FusionPanelError> {
    if value.trim().is_empty() || value.len() > MAX_TEXT_BYTES {
        return Err(FusionPanelError(
            "Fusion text is empty or exceeds the bound",
        ));
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}
