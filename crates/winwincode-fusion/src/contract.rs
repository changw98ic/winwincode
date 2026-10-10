use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FusionProviderCandidate {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FusionBudget {
    pub candidate_timeout_millis: Option<u64>,
    pub max_total_tokens: Option<u64>,
}

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

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FusionBlindPrompt {
    pub question: String,
    pub canonical_context: Value,
    pub constraints: Vec<String>,
    pub expected_output_schema: Value,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FusionTokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FusionProviderRequest {
    pub panel_id: String,
    pub candidate_id: String,
    pub request_id: String,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub prompt: FusionBlindPrompt,
    pub max_total_tokens: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FusionProviderAnswer {
    pub provider_response_id: String,
    pub answer: Value,
    pub token_usage: Option<FusionTokenUsage>,
}

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

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FusionCandidate {
    pub audit: FusionCandidateAudit,
    pub answer: Value,
}

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

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FusionPanelResult {
    pub panel_id: String,
    pub input_digest: String,
    pub candidates: Vec<FusionCandidate>,
    pub failures: Vec<FusionCandidateFailure>,
}
