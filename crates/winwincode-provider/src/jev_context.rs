// SPDX-License-Identifier: Apache-2.0

//! Provider scoring and deterministic retention share one auditable operation.

use std::fmt;
use std::time::Duration;

use winwincode_execution_port::jev_decision::{
    ContextDecision, ContextDecisionInput, ContextHypotheses, JevDecisionError, JevPolicy,
    NliProbabilities, decide_context, validate_policy,
};

use crate::{
    JevAttemptFailure, JevExecutionOptions, JevHypothesis, JevProviderErrorKind, JevRun,
    JevRuntime, JevScores,
};

/// The host supplies protection and archive eligibility; the provider cannot change them.
#[derive(Clone, serde::Serialize)]
pub struct JevContextRequest {
    pub task: String,
    pub candidate: String,
    pub protected: bool,
    pub archive_eligible: bool,
}

/// Original H1-H4 probabilities alongside the action, for deterministic replay.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JevContextEvaluation {
    pub hypotheses: ContextHypotheses,
    pub decision: ContextDecision,
}

impl fmt::Debug for JevContextRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JevContextRequest")
            .field("task", &"<private>")
            .field("candidate", &"<private>")
            .field("protected", &self.protected)
            .field("archive_eligible", &self.archive_eligible)
            .finish()
    }
}

impl JevRuntime {
    /// Scores H1-H4 in one batch, then applies system-owned retention policy.
    /// An unavailable or invalid inference returns no decision and retains its
    /// failures and usage; the caller must keep the previous context.
    ///
    /// # Errors
    /// Rejects empty inputs and invalid policy before contacting a provider.
    pub async fn evaluate_context(
        &self,
        input: JevContextRequest,
        policy: &JevPolicy,
        options: JevExecutionOptions,
    ) -> Result<JevRun<JevContextEvaluation>, JevDecisionError> {
        self.evaluate_context_authorized(input, policy, options, &|| true)
            .await
    }

    pub(crate) async fn evaluate_context_authorized(
        &self,
        input: JevContextRequest,
        policy: &JevPolicy,
        options: JevExecutionOptions,
        can_start: &(impl Fn() -> bool + ?Sized),
    ) -> Result<JevRun<JevContextEvaluation>, JevDecisionError> {
        validate_policy(policy)?;
        if input.task.trim().is_empty() || input.candidate.trim().is_empty() {
            return Err(JevDecisionError::InvalidMetadata);
        }
        let premise =
            serde_json::json!({ "task": input.task, "candidate": input.candidate }).to_string();
        let hypotheses = [
            "The candidate is a required system fact or hard constraint for the task.",
            "The candidate is useful for completing the current task.",
            "The candidate remains useful in compact or archived form.",
            "The candidate is noise or superseded information that is safe to discard.",
        ]
        .map(|hypothesis| JevHypothesis {
            premise: premise.clone(),
            hypothesis: hypothesis.to_owned(),
        });
        let run = self
            .batch_evaluate_authorized(hypotheses.into(), options, can_start)
            .await;
        let mut failures = run.failures;
        let mut value = None;
        if let (Some(batch), Some(observation)) = (&run.value, &run.observation) {
            if let [critical, relevant, compressible, disposable] = batch.evaluations.as_slice() {
                let context = ContextDecisionInput {
                    provider: observation.provider_id.clone(),
                    model: observation
                        .resolved_model_id
                        .as_ref()
                        .unwrap_or(&observation.model_id)
                        .clone(),
                    protected: input.protected,
                    archive_eligible: input.archive_eligible,
                    hypotheses: ContextHypotheses {
                        critical: probabilities(*critical),
                        relevant: probabilities(*relevant),
                        compressible: probabilities(*compressible),
                        disposable: probabilities(*disposable),
                    },
                };
                value = decide_context(&context, policy)
                    .map(|decision| JevContextEvaluation {
                        hypotheses: context.hypotheses,
                        decision,
                    })
                    .ok();
            }
            if value.is_none() {
                failures.push(JevAttemptFailure {
                    provider_id: observation.provider_id.clone(),
                    kind: JevProviderErrorKind::InvalidResponse,
                    latency: Duration::ZERO,
                });
            }
        }
        Ok(JevRun {
            value,
            observation: run.observation,
            failures,
        })
    }
}

fn probabilities(scores: JevScores) -> NliProbabilities {
    NliProbabilities {
        entailment: f64::from(scores.entailment),
        contradiction: f64::from(scores.contradiction),
        neutral: f64::from(scores.neutral),
    }
}
