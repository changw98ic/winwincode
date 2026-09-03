// SPDX-License-Identifier: Apache-2.0

//! Canonical derivation and fail-closed validation for one-shot `ChangeBatch` observation.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Component, Path};

use serde::Deserialize as _;
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use winwincode_domain::{ChangeBatchId, ObservationId, Sha256Digest, WorkspaceRevision};

use crate::change_batch_identity::validate_change_batch_identity_derivation;
use crate::diagnostic_parser::validate_normalized_diagnostic;
use crate::generated::{
    ObservationDecision, ObservationIntent, ObservationPromptInjectionStatus,
    ObservationReasonCode, ObservationReceipt, ObservationRequest, ObservationResponse,
    ObservationSecretScanStatus, ObservationSource, ObservationUntrustedInput, RepairClass,
    ValidationProfileName,
};

/// Maximum canonical Observer request bytes retained or sent to a provider.
pub const MAX_OBSERVATION_REQUEST_BYTES: usize = 131_072;
/// Maximum raw strict JSON response bytes accepted from a provider.
pub const MAX_OBSERVATION_RESPONSE_BYTES: usize = 65_536;

const OBSERVATION_ID_DOMAIN: &[u8] = b"winwincode.observation-id.v1";
const PROFILE_DIGEST_DOMAIN: &[u8] = b"winwincode.observation-profile.v1";
const CONTENT_DIGEST_DOMAIN: &[u8] = b"winwincode.observation-content.v1";
const INPUT_DIGEST_DOMAIN: &[u8] = b"winwincode.observation-input.v1";
const OUTPUT_DIGEST_DOMAIN: &[u8] = b"winwincode.observation-output.v1";

/// Stable Observer contract failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationContractErrorCode {
    InvalidIdentity,
    InvalidProfile,
    InvalidIntent,
    UnsafeInput,
    RequestTooLarge,
    ResponseTooLarge,
    InvalidJson,
    DuplicateField,
    InvalidResponse,
    InvalidReceipt,
}

/// Bounded Observer contract error that never includes model or source content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservationContractError {
    code: ObservationContractErrorCode,
    message: &'static str,
}

impl ObservationContractError {
    /// Returns the stable machine category.
    pub const fn code(&self) -> ObservationContractErrorCode {
        self.code
    }
}

impl fmt::Display for ObservationContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ObservationContractError {}

/// Derives the unique observation identity for one batch result and selected profile.
///
/// # Errors
///
/// Rejects malformed batch, revision, or digest values before hashing.
pub fn derive_observation_id(
    batch_id: &ChangeBatchId,
    result_revision: &WorkspaceRevision,
    profile_digest: &Sha256Digest,
) -> Result<ObservationId, ObservationContractError> {
    if !valid_sha256(&batch_id.0)
        || !valid_revision(result_revision)
        || !valid_sha256(&profile_digest.0)
    {
        return Err(invalid_identity());
    }
    let mut digest = Sha256::new();
    digest.update(OBSERVATION_ID_DOMAIN);
    frame(&mut digest, batch_id.0.as_bytes());
    frame(&mut digest, result_revision.0.as_bytes());
    frame(&mut digest, profile_digest.0.as_bytes());
    Ok(ObservationId(format!("sha256:{:x}", digest.finalize())))
}

/// Derives the selected validation-profile digest from exact configuration and command order.
///
/// # Errors
///
/// Rejects malformed configuration digests, empty/oversized inventories, or duplicate IDs.
pub fn derive_observation_profile_digest(
    profile: &ValidationProfileName,
    configuration_digest: &Sha256Digest,
    command_ids: &[String],
) -> Result<Sha256Digest, ObservationContractError> {
    if !valid_sha256(&configuration_digest.0)
        || command_ids.is_empty()
        || command_ids.len() > 64
        || !unique_strings(command_ids)
        || command_ids.iter().any(|id| !valid_identifier(id, 100))
    {
        return Err(invalid_profile());
    }
    let mut digest = Sha256::new();
    digest.update(PROFILE_DIGEST_DOMAIN);
    frame(&mut digest, profile_text(profile).as_bytes());
    frame(&mut digest, configuration_digest.0.as_bytes());
    digest.update(u64_len(command_ids.len()));
    for id in command_ids {
        frame(&mut digest, id.as_bytes());
    }
    Ok(Sha256Digest(format!("sha256:{:x}", digest.finalize())))
}

/// Derives the digest of the bounded untrusted payload, excluding its stored digest field.
///
/// # Errors
///
/// Rejects input that cannot be represented as canonical JSON.
pub fn derive_observation_content_digest(
    input: &ObservationUntrustedInput,
) -> Result<Sha256Digest, ObservationContractError> {
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct ContentMaterial<'input> {
        trust_level: &'input crate::generated::ObservationUntrustedInputTrustLevel,
        goal_summary: &'input str,
        acceptance_criteria: &'input [crate::generated::ObservationAcceptanceCriterion],
        batch_summary: &'input str,
        delta: &'input crate::generated::ObservationDeltaSummary,
        new_diagnostics: &'input [crate::generated::NormalizedDiagnostic],
        failed_tests: &'input [crate::generated::ObservationFailedTestSummary],
        snippets: &'input [crate::generated::ObservationSnippet],
    }
    let material = ContentMaterial {
        trust_level: &input.trust_level,
        goal_summary: &input.goal_summary,
        acceptance_criteria: &input.acceptance_criteria,
        batch_summary: &input.batch_summary,
        delta: &input.delta,
        new_diagnostics: &input.new_diagnostics,
        failed_tests: &input.failed_tests,
        snippets: &input.snippets,
    };
    digest_serialized(CONTENT_DIGEST_DOMAIN, &material)
}

/// Derives the exact Observer input digest, excluding only the stored digest field itself.
///
/// The bounded untrusted payload is included through both its typed fields and content digest.
///
/// # Errors
///
/// Rejects intent that cannot be represented as canonical JSON.
pub fn derive_observation_input_digest(
    intent: &ObservationIntent,
) -> Result<Sha256Digest, ObservationContractError> {
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct InputMaterial<'intent> {
        observation_id: &'intent ObservationId,
        identity: &'intent crate::generated::ChangeBatchIdentity,
        result_revision: &'intent WorkspaceRevision,
        validation_profile: &'intent ValidationProfileName,
        profile_digest: &'intent Sha256Digest,
        delta_digest: &'intent Sha256Digest,
        delta_exact: bool,
        hard_check_failed: bool,
        all_checks_executed: bool,
        secret_scan: &'intent crate::generated::ObservationSecretScan,
        prompt_injection_scan: &'intent crate::generated::ObservationPromptInjectionScan,
        data_egress: &'intent crate::generated::ObservationDataEgressPolicy,
        untrusted_input: &'intent ObservationUntrustedInput,
    }
    let material = InputMaterial {
        observation_id: &intent.observation_id,
        identity: &intent.identity,
        result_revision: &intent.result_revision,
        validation_profile: &intent.validation_profile,
        profile_digest: &intent.profile_digest,
        delta_digest: &intent.delta_digest,
        delta_exact: intent.delta_exact,
        hard_check_failed: intent.hard_check_failed,
        all_checks_executed: intent.all_checks_executed,
        secret_scan: &intent.secret_scan,
        prompt_injection_scan: &intent.prompt_injection_scan,
        data_egress: &intent.data_egress,
        untrusted_input: &intent.untrusted_input,
    };
    digest_serialized(INPUT_DIGEST_DOMAIN, &material)
}

/// Derives the canonical digest of one validated Observer response.
///
/// # Errors
///
/// Rejects a response that cannot be represented as canonical JSON.
pub fn derive_observation_output_digest(
    response: &ObservationResponse,
) -> Result<Sha256Digest, ObservationContractError> {
    digest_serialized(OUTPUT_DIGEST_DOMAIN, response)
}

/// Revalidates all authority, safety, derivation, size, and exactness facts in an intent.
///
/// # Errors
///
/// Rejects hard-check failures, inexact deltas, incomplete checks, rejected Secret Scan output,
/// data egress, oversized content, or any digest/identity drift.
pub fn validate_observation_intent(
    intent: &ObservationIntent,
) -> Result<(), ObservationContractError> {
    validate_change_batch_identity_derivation(&intent.identity).map_err(|_| invalid_identity())?;
    if !valid_revision(&intent.result_revision)
        || !valid_sha256(&intent.profile_digest.0)
        || !valid_sha256(&intent.delta_digest.0)
        || intent.delta_digest != intent.untrusted_input.delta.delta_digest
        || !intent.delta_exact
        || intent.hard_check_failed
        || !intent.all_checks_executed
        || intent.data_egress.network_allowed
        || intent.data_egress.external_artifact_reads_allowed
        || intent.data_egress.provider_file_uploads_allowed
        || !valid_prompt_injection_scan(intent)
    {
        return Err(unsafe_input());
    }
    let expected_id = derive_observation_id(
        &intent.identity.batch_id,
        &intent.result_revision,
        &intent.profile_digest,
    )?;
    if intent.observation_id != expected_id {
        return Err(invalid_identity());
    }
    validate_untrusted_input(&intent.untrusted_input)?;
    let expected_content = derive_observation_content_digest(&intent.untrusted_input)?;
    if intent.untrusted_input.content_digest != expected_content
        || intent.secret_scan.output_digest != expected_content
        || !valid_secret_scan(intent)
        || derive_observation_input_digest(intent)? != intent.input_digest
    {
        return Err(unsafe_input());
    }
    Ok(())
}

/// Revalidates the strict one-shot request and its serialized byte ceiling.
///
/// # Errors
///
/// Rejects version/mode drift, invalid intent, or an oversized request.
pub fn validate_observation_request(
    request: &ObservationRequest,
) -> Result<(), ObservationContractError> {
    if request.schema_version != 1 || !request.one_shot {
        return Err(invalid_intent());
    }
    validate_observation_intent(&request.intent)?;
    let bytes = serde_json::to_vec(request).map_err(|_| invalid_intent())?;
    if bytes.len() > MAX_OBSERVATION_REQUEST_BYTES {
        return Err(error(
            ObservationContractErrorCode::RequestTooLarge,
            "Observer request exceeds the canonical byte limit",
        ));
    }
    Ok(())
}

/// Parses one canonical request with duplicate and unknown field rejection.
///
/// # Errors
///
/// Rejects oversized or malformed JSON, duplicate keys at any depth, unknown fields, wrong
/// one-shot/version values, unsafe intent content, or any authority/digest drift.
pub fn parse_observation_request_strict(
    bytes: &[u8],
) -> Result<ObservationRequest, ObservationContractError> {
    if bytes.len() > MAX_OBSERVATION_REQUEST_BYTES {
        return Err(error(
            ObservationContractErrorCode::RequestTooLarge,
            "Observer request exceeds the canonical byte limit",
        ));
    }
    let value = parse_strict_json_value(bytes)?;
    let request =
        serde_json::from_value::<ObservationRequest>(value).map_err(|_| invalid_intent())?;
    validate_observation_request(&request)?;
    Ok(request)
}

/// Parses one provider response with duplicate and unknown field rejection.
///
/// # Errors
///
/// Rejects oversized/malformed JSON, duplicate keys at any depth, unknown fields, wrong
/// observation identity, bounds drift, an illegal decision/reason/repair combination, or an
/// `accept` decision for input whose prompt-injection scan is suspected.
pub fn parse_observation_response_strict(
    bytes: &[u8],
    intent: &ObservationIntent,
) -> Result<ObservationResponse, ObservationContractError> {
    validate_observation_intent(intent)?;
    if bytes.len() > MAX_OBSERVATION_RESPONSE_BYTES {
        return Err(error(
            ObservationContractErrorCode::ResponseTooLarge,
            "Observer response exceeds the canonical byte limit",
        ));
    }
    let value = parse_strict_json_value(bytes)?;
    let response =
        serde_json::from_value::<ObservationResponse>(value).map_err(|_| invalid_response())?;
    validate_observation_response(&response, intent)?;
    Ok(response)
}

/// Revalidates response semantics, exact observation identity, and input-risk policy.
///
/// # Errors
///
/// Rejects version, bounds, identity, decision/reason, repair-class drift, or acceptance of a
/// prompt-injection-suspected input.
pub fn validate_observation_response(
    response: &ObservationResponse,
    intent: &ObservationIntent,
) -> Result<(), ObservationContractError> {
    if response.schema_version != 1
        || response.observation_id != intent.observation_id
        || !bounded_line(&response.summary, 500)
        || response.root_causes.len() > 16
        || !unique_strings(&response.root_causes)
        || response
            .root_causes
            .iter()
            .any(|cause| !bounded_line(cause, 500))
        || !(0..=10_000).contains(&response.confidence_bps)
        || !valid_response_combination(response)
        || (intent.prompt_injection_scan.status == ObservationPromptInjectionStatus::Suspected
            && response.decision == ObservationDecision::Accept)
    {
        return Err(invalid_response());
    }
    Ok(())
}

/// Revalidates a durable receipt against the exact source intent.
///
/// # Errors
///
/// Rejects authority, revision, digest, response, source, or usage drift.
pub fn validate_observation_receipt(
    receipt: &ObservationReceipt,
    intent: &ObservationIntent,
) -> Result<(), ObservationContractError> {
    validate_observation_intent(intent)?;
    validate_observation_response(&receipt.response, intent)?;
    let valid_usage = receipt.model_usage.as_ref().is_none_or(|usage| {
        (0..=9_007_199_254_740_991).contains(&usage.runtime_millis)
            && (0..=9_007_199_254_740_991).contains(&usage.tokens)
            && (0..=9_007_199_254_740_991).contains(&usage.cost_microunits)
    });
    let source_usage = match receipt.source {
        ObservationSource::Model => receipt.model_usage.is_some(),
        ObservationSource::ObserverRuntime => {
            receipt.model_usage.is_none()
                && receipt.response.decision == ObservationDecision::InfrastructureError
        }
    };
    if receipt.identity != intent.identity
        || receipt.result_revision != intent.result_revision
        || receipt.profile_digest != intent.profile_digest
        || receipt.input_digest != intent.input_digest
        || receipt.output_digest != derive_observation_output_digest(&receipt.response)?
        || !valid_usage
        || !source_usage
    {
        return Err(error(
            ObservationContractErrorCode::InvalidReceipt,
            "Observer receipt is not bound to the exact intent and response",
        ));
    }
    Ok(())
}

/// Returns the closed provider-safe strict JSON Schema for [`ObservationResponse`].
#[must_use]
pub fn observation_response_json_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "schemaVersion": { "type": "integer", "enum": [1] },
            "observationId": { "type": "string" },
            "decision": { "type": "string", "enum": ["accept", "repair_required", "semantic_risk", "infrastructure_error", "inconclusive"] },
            "reasonCode": { "type": "string", "enum": ["criteria_satisfied", "targeted_repair_required", "semantic_risk_detected", "observer_infrastructure_error", "insufficient_evidence"] },
            "summary": { "type": "string" },
            "rootCauses": { "type": "array", "items": { "type": "string" } },
            "repairClass": {
                "type": ["string", "null"],
                "enum": ["targeted_patch", "regenerate_batch", "replan", "human_review", null]
            },
            "confidenceBps": { "type": "integer" }
        },
        "required": ["schemaVersion", "observationId", "decision", "reasonCode", "summary", "rootCauses", "repairClass", "confidenceBps"]
    })
}

fn validate_untrusted_input(
    input: &ObservationUntrustedInput,
) -> Result<(), ObservationContractError> {
    if !bounded_line(&input.goal_summary, 500)
        || !bounded_line(&input.batch_summary, 500)
        || input.acceptance_criteria.is_empty()
        || input.acceptance_criteria.len() > 64
        || input.new_diagnostics.len() > 64
        || input.failed_tests.len() > 32
        || input.snippets.len() > 8
        || input.delta.file_count < 0
        || input.delta.file_count > 20
        || input.delta.hunk_count < 0
        || input.delta.hunk_count > 100
        || !input.delta.delta_exact
        || !valid_sha256(&input.delta.delta_digest.0)
        || !bounded_line(&input.delta.summary, 500)
        || !valid_sha256(&input.content_digest.0)
    {
        return Err(unsafe_input());
    }
    let mut criteria = BTreeSet::new();
    for criterion in &input.acceptance_criteria {
        if !valid_identifier(&criterion.id, 200)
            || !bounded_line(&criterion.summary, 500)
            || !criteria.insert(criterion.id.as_str())
        {
            return Err(unsafe_input());
        }
    }
    let mut diagnostic_ids = BTreeSet::new();
    for diagnostic in &input.new_diagnostics {
        if validate_normalized_diagnostic(diagnostic).is_err()
            || !diagnostic_ids.insert(diagnostic.diagnostic_id.0.as_str())
        {
            return Err(unsafe_input());
        }
    }
    let mut tests = BTreeSet::new();
    for test in &input.failed_tests {
        if !valid_identifier(&test.name, 100)
            || !bounded_line(&test.summary, 500)
            || test
                .diagnostic_digest
                .as_ref()
                .is_some_and(|digest| !valid_sha256(&digest.0))
            || !tests.insert(test.name.as_str())
        {
            return Err(unsafe_input());
        }
    }
    let mut snippets = BTreeSet::new();
    for snippet in &input.snippets {
        let expected = Sha256Digest(format!(
            "sha256:{:x}",
            Sha256::digest(snippet.content.as_bytes())
        ));
        if !portable_path(&snippet.path)
            || !(1..=i64::from(i32::MAX)).contains(&snippet.start_line)
            || snippet.end_line < snippet.start_line
            || snippet.end_line > i64::from(i32::MAX)
            || snippet.content.is_empty()
            || snippet.content.chars().count() > 2_000
            || snippet.content_digest != expected
            || !snippets.insert((
                snippet.path.as_str(),
                snippet.start_line,
                snippet.end_line,
                snippet.content_digest.0.as_str(),
            ))
        {
            return Err(unsafe_input());
        }
    }
    Ok(())
}

fn valid_secret_scan(intent: &ObservationIntent) -> bool {
    let scan = &intent.secret_scan;
    if !valid_identifier(&scan.scanner_version, 64)
        || !valid_sha256(&scan.input_digest.0)
        || !valid_sha256(&scan.output_digest.0)
        || !(0..=64).contains(&scan.finding_count)
    {
        return false;
    }
    match scan.status {
        ObservationSecretScanStatus::Clean => {
            scan.finding_count == 0 && scan.input_digest == scan.output_digest
        }
        ObservationSecretScanStatus::Redacted => scan.finding_count > 0,
        ObservationSecretScanStatus::Rejected => false,
    }
}

fn valid_prompt_injection_scan(intent: &ObservationIntent) -> bool {
    let scan = &intent.prompt_injection_scan;
    if !valid_identifier(&scan.scanner_version, 64)
        || !valid_sha256(&scan.rules_digest.0)
        || scan.input_digest != intent.untrusted_input.content_digest
        || !(0..=64).contains(&scan.finding_count)
    {
        return false;
    }
    match scan.status {
        ObservationPromptInjectionStatus::Clean => scan.finding_count == 0,
        ObservationPromptInjectionStatus::Suspected => scan.finding_count > 0,
    }
}

fn valid_response_combination(response: &ObservationResponse) -> bool {
    match (
        &response.decision,
        &response.reason_code,
        &response.repair_class,
    ) {
        (ObservationDecision::Accept, ObservationReasonCode::CriteriaSatisfied, None) => {
            response.root_causes.is_empty()
        }
        (
            ObservationDecision::RepairRequired,
            ObservationReasonCode::TargetedRepairRequired,
            Some(RepairClass::TargetedPatch | RepairClass::RegenerateBatch),
        )
        | (
            ObservationDecision::SemanticRisk,
            ObservationReasonCode::SemanticRiskDetected,
            Some(RepairClass::Replan | RepairClass::HumanReview),
        ) => !response.root_causes.is_empty(),
        (
            ObservationDecision::InfrastructureError,
            ObservationReasonCode::ObserverInfrastructureError,
            None,
        )
        | (ObservationDecision::Inconclusive, ObservationReasonCode::InsufficientEvidence, None) => {
            true
        }
        _ => false,
    }
}

fn digest_serialized(
    domain: &[u8],
    value: &impl serde::Serialize,
) -> Result<Sha256Digest, ObservationContractError> {
    let bytes = serde_json::to_vec(value).map_err(|_| invalid_intent())?;
    let mut digest = Sha256::new();
    digest.update(domain);
    frame(&mut digest, &bytes);
    Ok(Sha256Digest(format!("sha256:{:x}", digest.finalize())))
}

fn parse_strict_json_value(bytes: &[u8]) -> Result<Value, ObservationContractError> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let StrictValue(value) = StrictValue::deserialize(&mut deserializer).map_err(|failure| {
        if failure
            .to_string()
            .contains("duplicate Observer JSON field")
        {
            error(
                ObservationContractErrorCode::DuplicateField,
                "Observer JSON contains a duplicate field",
            )
        } else {
            error(
                ObservationContractErrorCode::InvalidJson,
                "Observer input is not valid JSON",
            )
        }
    })?;
    deserializer.end().map_err(|_| {
        error(
            ObservationContractErrorCode::InvalidJson,
            "Observer JSON has trailing input",
        )
    })?;
    Ok(value)
}

fn frame(digest: &mut Sha256, value: &[u8]) {
    digest.update(u64_len(value.len()));
    digest.update(value);
}

fn u64_len(value: usize) -> [u8; 8] {
    u64::try_from(value).unwrap_or(u64::MAX).to_be_bytes()
}

fn profile_text(profile: &ValidationProfileName) -> &'static str {
    match profile {
        ValidationProfileName::Changed => "changed",
        ValidationProfileName::Fast => "fast",
        ValidationProfileName::Affected => "affected",
        ValidationProfileName::Final => "final",
    }
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 71
        && value.strip_prefix("sha256:").is_some_and(|hex| {
            hex.bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        })
}

fn valid_revision(revision: &WorkspaceRevision) -> bool {
    revision.0.strip_prefix("git-tree:").is_some_and(|hex| {
        matches!(hex.len(), 40 | 64)
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}

fn valid_identifier(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'@' | b'-')
        })
}

fn bounded_line(value: &str, max: usize) -> bool {
    !value.is_empty() && value.chars().count() <= max && !value.contains(['\0', '\r', '\n'])
}

fn unique_strings(values: &[String]) -> bool {
    values.iter().collect::<BTreeSet<_>>().len() == values.len()
}

fn portable_path(path: &str) -> bool {
    let candidate = Path::new(path);
    !path.is_empty()
        && path.len() <= 4_096
        && !path.contains(['\\', ':', '\0', '<', '>', '"', '|', '?', '*'])
        && !path.bytes().any(|byte| byte.is_ascii_control())
        && !candidate.is_absolute()
        && candidate
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        && path.split('/').all(portable_path_component)
}

fn portable_path_component(component: &str) -> bool {
    if component.is_empty()
        || matches!(component, "." | "..")
        || component.eq_ignore_ascii_case(".git")
        || component.ends_with([' ', '.'])
    {
        return false;
    }
    let stem = component.split('.').next().unwrap_or(component);
    let upper = stem.to_ascii_uppercase();
    !matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        && !upper
            .strip_prefix("COM")
            .or_else(|| upper.strip_prefix("LPT"))
            .is_some_and(|suffix| {
                matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
}

struct StrictValue(Value);

impl<'de> serde::Deserialize<'de> for StrictValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictValueVisitor)
    }
}

struct StrictValueVisitor;

impl<'de> Visitor<'de> for StrictValueVisitor {
    type Value = StrictValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate object fields")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Number(value.into())))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Number(value.into())))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .map(StrictValue)
            .ok_or_else(|| E::custom("non-finite Observer JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Null))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(StrictValue(value)) = sequence.next_element()? {
            values.push(value);
        }
        Ok(StrictValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Map::new();
        while let Some(key) = object.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(serde::de::Error::custom("duplicate Observer JSON field"));
            }
            let StrictValue(value) = object.next_value()?;
            values.insert(key, value);
        }
        Ok(StrictValue(Value::Object(values)))
    }
}

const fn error(
    code: ObservationContractErrorCode,
    message: &'static str,
) -> ObservationContractError {
    ObservationContractError { code, message }
}

const fn invalid_identity() -> ObservationContractError {
    error(
        ObservationContractErrorCode::InvalidIdentity,
        "Observer identity input is not canonical",
    )
}

const fn invalid_profile() -> ObservationContractError {
    error(
        ObservationContractErrorCode::InvalidProfile,
        "Observer profile input is not canonical",
    )
}

const fn invalid_intent() -> ObservationContractError {
    error(
        ObservationContractErrorCode::InvalidIntent,
        "Observer intent is not canonical",
    )
}

const fn unsafe_input() -> ObservationContractError {
    error(
        ObservationContractErrorCode::UnsafeInput,
        "Observer input did not pass the canonical safety boundary",
    )
}

const fn invalid_response() -> ObservationContractError {
    error(
        ObservationContractErrorCode::InvalidResponse,
        "Observer response is not canonical",
    )
}
