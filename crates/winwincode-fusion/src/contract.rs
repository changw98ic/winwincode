// SPDX-License-Identifier: Apache-2.0

//! Canonical Fusion wire contract.
//!
//! Field names use camelCase on the wire. The types intentionally do not
//! depend on Kernel, Codex, or Jev types so the Fusion epic can version
//! independently.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

/// One independently addressed Provider route in a blind panel.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FusionProviderCandidate {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
}

/// Hard per-candidate limits carried by the canonical panel input.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FusionBudget {
    pub candidate_timeout_millis: u64,
    pub max_total_tokens: u64,
}

/// Canonical input shared by every route.
///
/// Only the blind prompt fields are exposed to a model. Sibling candidates and
/// their outputs never cross a Provider boundary.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FusionInput {
    pub question: String,
    pub canonical_context: Value,
    pub constraints: Vec<String>,
    pub expected_output_schema: Value,
    pub provider_candidates: Vec<FusionProviderCandidate>,
    pub budget: FusionBudget,
}

/// Isolated prompt payload delivered to exactly one Provider.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FusionBlindPrompt {
    pub question: String,
    pub canonical_context: Value,
    pub constraints: Vec<String>,
    pub expected_output_schema: Value,
}

/// Provider-neutral token accounting returned by a candidate answer.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FusionTokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

/// One complete independent request to a single Provider route.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FusionProviderRequest {
    pub panel_id: String,
    pub candidate_id: String,
    /// Stable identity unique to this panel + candidate + input digest.
    pub request_id: String,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    /// Blind prompt only. Never includes sibling candidates.
    pub prompt: FusionBlindPrompt,
    pub max_total_tokens: u64,
}

/// One complete independent Provider answer.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FusionProviderAnswer {
    pub provider_response_id: String,
    pub answer: Value,
    pub token_usage: Option<FusionTokenUsage>,
}

/// Stable facts binding one answer to the exact input and model request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FusionCandidateAudit {
    pub panel_id: String,
    pub candidate_id: String,
    pub request_id: String,
    pub input_digest: String,
    pub request_payload_digest: String,
    pub provider: String,
    pub model: String,
    pub provider_response_id: String,
    pub token_usage: Option<FusionTokenUsage>,
    pub elapsed_millis: u64,
}

/// One complete independently auditable model answer.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FusionCandidate {
    pub audit: FusionCandidateAudit,
    pub answer: Value,
}

/// One isolated candidate failure retained without failing successful siblings.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FusionCandidateFailure {
    pub panel_id: String,
    pub candidate_id: String,
    pub request_id: String,
    pub input_digest: String,
    pub request_payload_digest: String,
    pub provider: String,
    pub model: String,
    pub code: String,
    pub message: String,
    pub elapsed_millis: u64,
}

/// Terminal panel record. Candidate and failure order follows candidate id order.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FusionPanelResult {
    pub panel_id: String,
    pub input_digest: String,
    pub candidates: Vec<FusionCandidate>,
    pub failures: Vec<FusionCandidateFailure>,
}
