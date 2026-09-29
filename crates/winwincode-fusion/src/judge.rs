// SPDX-License-Identifier: Apache-2.0

//! Anonymous evidence packs shared by host and Device semantic judges.

use crate::analysis::{FusionCandidateClaims, FusionClaimPosition};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// R4 judge input is an evidence pack (R1+R3), never vote counts (ADR-0036).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JudgeRequest {
    pub claim_key: String,
    pub summary: String,
    pub question: String,
    pub canonical_context: Value,
    pub evidence_pack: Vec<Value>,
}

/// Builds one anonymous semantic judgment request from independently collected claims.
#[must_use]
pub fn build_blind_judge_request(
    claims: &[FusionCandidateClaims],
    claim_key: &str,
    summary: &str,
    question: &str,
    canonical_context: Value,
) -> JudgeRequest {
    JudgeRequest {
        claim_key: claim_key.to_owned(),
        summary: summary.to_owned(),
        question: question.to_owned(),
        canonical_context: blind_context_value(canonical_context),
        evidence_pack: build_judge_evidence_pack(claims, claim_key),
    }
}

impl JudgeRequest {
    /// Produces the same anonymous premise at every runtime entry.
    ///
    /// # Errors
    /// Returns a JSON serialization failure.
    pub fn premise(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&json!({
            "question": self.question,
            "context": blind_context_value(self.canonical_context.clone()),
            "evidence": blind_context_value(Value::Array(self.evidence_pack.clone())),
        }))
    }
}

fn blind_context_value(value: Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .filter(|(key, _)| !is_blind_metadata_key(key))
                .map(|(key, value)| (key, blind_context_value(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(blind_context_value).collect()),
        other => other,
    }
}

fn is_blind_metadata_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|character| character.to_ascii_lowercase())
        .collect();
    matches!(
        normalized.as_str(),
        "provider"
            | "providerid"
            | "providername"
            | "model"
            | "modelid"
            | "modelname"
            | "seat"
            | "seatid"
            | "seatname"
            | "candidate"
            | "candidateid"
            | "candidatename"
            | "vote"
            | "votes"
            | "votecount"
            | "supportcount"
            | "supportercount"
            | "majority"
            | "minority"
    )
}

/// R3 evidence rows collected for disputed claims, canonicalized and deduped.
///
/// Evidence provenance is retained as evidence content; respondent identity is
/// deliberately omitted. Repeated identical support therefore cannot become a
/// vote-count signal.
fn build_judge_evidence_pack(claims: &[FusionCandidateClaims], claim_key: &str) -> Vec<Value> {
    let mut rows = BTreeMap::<(String, String, String), Value>::new();
    for candidate in claims {
        for claim in &candidate.claims {
            if claim.claim_key != claim_key {
                continue;
            }
            let position = match claim.position {
                FusionClaimPosition::Supports => "supports",
                FusionClaimPosition::Opposes => "opposes",
            };
            if claim.evidence.is_empty() {
                rows.entry((position.to_owned(), String::new(), String::new()))
                    .or_insert_with(|| json!({ "position": position, "evidence": [] }));
                continue;
            }
            for evidence in &claim.evidence {
                let evidence_type = format!("{:?}", evidence.evidence_type);
                rows.entry((
                    position.to_owned(),
                    evidence_type.clone(),
                    evidence.source_ref.clone(),
                ))
                .or_insert_with(|| {
                    json!({
                        "position": position,
                        "evidenceType": evidence_type,
                        "sourceRef": evidence.source_ref
                    })
                });
            }
        }
    }
    rows.into_values()
        .enumerate()
        .map(|(index, mut row)| {
            if let Value::Object(object) = &mut row {
                object.insert(
                    "evidenceId".to_owned(),
                    json!(format!("evidence-{index:04}")),
                );
            }
            row
        })
        .collect()
}
