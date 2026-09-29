// SPDX-License-Identifier: Apache-2.0

//! Extracts independent model claims without granting them verification authority.

use crate::analysis::{FusionCandidateClaims, FusionClaim, FusionClaimPosition, FusionEvidence};
use serde_json::Value;
use winwincode_delivery::domain::EvidenceRefType;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaimExtractionError {
    pub candidate_id: String,
    pub message: String,
}

/// Closed structured answer expected from independent members.
#[must_use]
pub fn default_claim_output_schema() -> Value {
    let evidence_type = serde_json::json!({"type":"string", "enum":[
        EvidenceRefType::Test, EvidenceRefType::Command, EvidenceRefType::Diff,
        EvidenceRefType::File, EvidenceRefType::Commit, EvidenceRefType::PullRequest,
        EvidenceRefType::RuntimeEvent, EvidenceRefType::ReviewFinding,
    ]});
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
                        "evidence": { "type": "array", "items": {
                            "type":"object", "properties": {
                                "evidenceType": evidence_type,
                                "sourceRef": {"type":"string"}
                            },
                            "required":["evidenceType","sourceRef"],
                            "additionalProperties":false
                        } },
                        "requiredEvidence": { "type": "array", "items": evidence_type }
                    },
                    "required": ["claimKey", "summary", "position", "evidence", "requiredEvidence"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["claims"],
        "additionalProperties": false
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
/// Returns [`ClaimExtractionError`] when the answer is not an
/// object, has no parseable `claims` array, or a claim row is invalid.
pub fn extract_claims_from_answer(
    candidate_id: &str,
    answer: &Value,
) -> Result<FusionCandidateClaims, ClaimExtractionError> {
    let fail = |message: &str| ClaimExtractionError {
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
    Ok(FusionEvidence {
        evidence_type,
        source_ref,
        // A candidate's answer is model prose. It can cite a source, but it
        // cannot promote that citation to a verified fact by naming a result.
        verified_conclusion: None,
    })
}

fn parse_evidence_type(text: &str) -> Result<EvidenceRefType, String> {
    let normalized = text.trim().to_ascii_lowercase();
    let json = Value::String(normalized);
    serde_json::from_value(json).map_err(|_| format!("unsupported evidence type: {text}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn assert_closed_schema(schema: &Value) {
        match schema["type"].as_str() {
            Some("object") => {
                assert_eq!(schema["additionalProperties"], false);
                let properties = schema["properties"].as_object().unwrap();
                let required: BTreeSet<_> = schema["required"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|key| key.as_str().unwrap())
                    .collect();
                assert_eq!(required, properties.keys().map(String::as_str).collect());
                for property in properties.values() {
                    assert_closed_schema(property);
                }
            }
            Some("array") => assert_closed_schema(&schema["items"]),
            Some("string") => {}
            other => panic!("missing or unsupported schema type: {other:?}"),
        }
    }

    #[test]
    fn member_schema_is_closed_and_every_evidence_type_is_parseable() {
        let schema = default_claim_output_schema();
        assert_closed_schema(&schema);
        for name in schema["properties"]["claims"]["items"]["properties"]["requiredEvidence"]["items"]["enum"].as_array().unwrap() {
            assert!(serde_json::from_value::<EvidenceRefType>(name.clone()).is_ok());
        }
    }
}
