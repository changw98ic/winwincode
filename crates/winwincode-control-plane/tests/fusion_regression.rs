// SPDX-License-Identifier: Apache-2.0
//! Executable ADR-0037 regression fixtures under `fusion-regression/`.

use serde::Deserialize;
use serde_json::Value;

use winwincode_control_plane::fusion_analysis::{FusionCandidateClaims, FusionClaim, FusionClaimPosition, FusionEvidence};
use winwincode_control_plane::fusion_knowledge::{
    ClaimState, EvidenceDirection, EvidenceStrength, FusionEvidenceRecord, build_claim_graph,
    refute_claim,
};
use winwincode_delivery::domain::EvidenceRefType;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    claims: Vec<FixtureCandidate>,
    expected: FixtureExpected,
    #[serde(default)]
    after_evidence: Vec<FixtureEvidence>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureCandidate {
    candidate_id: String,
    claims: Vec<FixtureClaim>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureClaim {
    claim_key: String,
    summary: String,
    position: String,
    #[serde(default)]
    evidence: Vec<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureExpected {
    claim_count: usize,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    must_not_be: Vec<String>,
    #[serde(default)]
    must_trigger_investigation: Option<bool>,
    #[serde(default)]
    refute_allowed_only_with_verified_counter: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureEvidence {
    direction: String,
    verified: bool,
    kind: String,
    facts: Vec<String>,
    provider: String,
}

fn parse_fixture(raw: &str) -> Fixture {
    serde_json::from_str(raw).expect("fixture json")
}

fn to_candidates(fixture: &Fixture) -> Vec<FusionCandidateClaims> {
    fixture
        .claims
        .iter()
        .map(|candidate| FusionCandidateClaims {
            candidate_id: candidate.candidate_id.clone(),
            claims: candidate
                .claims
                .iter()
                .map(|claim| FusionClaim {
                    claim_key: claim.claim_key.clone(),
                    summary: claim.summary.clone(),
                    position: if claim.position == "opposes" {
                        FusionClaimPosition::Opposes
                    } else {
                        FusionClaimPosition::Supports
                    },
                    evidence: claim
                        .evidence
                        .iter()
                        .map(|item| FusionEvidence {
                            evidence_type: EvidenceRefType::ReviewFinding,
                            source_ref: item
                                .get("sourceRef")
                                .and_then(Value::as_str)
                                .unwrap_or("fixture")
                                .to_owned(),
                            verified_conclusion: None,
                        })
                        .collect(),
                    required_evidence: Vec::new(),
                })
                .collect(),
        })
        .collect()
}

fn state_name(state: ClaimState) -> &'static str {
    match state {
        ClaimState::Discovered => "DISCOVERED",
        ClaimState::Supported => "SUPPORTED",
        ClaimState::Confirmed => "CONFIRMED",
        ClaimState::Disputed => "DISPUTED",
        ClaimState::Investigating => "INVESTIGATING",
        ClaimState::Refuted => "REFUTED",
        ClaimState::Escalated => "ESCALATED",
        ClaimState::Unresolved => "UNRESOLVED",
        ClaimState::UnverifiedAbsence => "UNVERIFIED_ABSENCE",
    }
}

fn check_fixture(raw: &str) {
    let fixture = parse_fixture(raw);
    let candidates = to_candidates(&fixture);
    let mut graph = build_claim_graph(&candidates);
    assert_eq!(graph.claims.len(), fixture.expected.claim_count);
    for claim in &graph.claims {
        if let Some(expected) = &fixture.expected.state {
            assert_eq!(state_name(claim.state), expected.as_str());
        }
        for banned in &fixture.expected.must_not_be {
            assert_ne!(state_name(claim.state), banned.as_str());
        }
    }
    if fixture.expected.must_trigger_investigation == Some(true) {
        assert!(graph
            .disputed_or_investigating()
            .iter()
            .any(|claim| claim.state == ClaimState::Disputed));
    }
    if fixture.expected.refute_allowed_only_with_verified_counter == Some(true) {
        let id = graph.claims[0].id.clone();
        assert!(!refute_claim(&mut graph, &id));
        for item in &fixture.after_evidence {
            let direction = if item.direction == "counter" {
                EvidenceDirection::Counter
            } else {
                EvidenceDirection::Support
            };
            let record = FusionEvidenceRecord {
                id: format!("ev_fixture_{}", item.kind),
                claim_id: id.clone(),
                provider: item.provider.clone(),
                direction,
                kind: item.kind.clone(),
                strength: EvidenceStrength::Direct,
                facts: item.facts.clone(),
                source_refs: vec![item.provider.clone()],
                independence_group: format!("ig:{}", item.kind),
                verified: item.verified,
                invalidated: false,
            };
            if direction == EvidenceDirection::Counter && record.verified {
                graph.claims[0].has_verified_counter = true;
            }
            graph.evidence.push(record);
        }
        if graph.claims[0].has_verified_counter {
            assert!(refute_claim(&mut graph, &id));
            assert_eq!(graph.claims[0].state, ClaimState::Refuted);
        }
    }
}

#[test]
fn regression_q2_root_shared_fixture_race() {
    check_fixture(include_str!(
        "../../../fusion-regression/q2-root-shared-fixture-race.json"
    ));
}

#[test]
fn regression_q1_defect_null_unwrap() {
    check_fixture(include_str!(
        "../../../fusion-regression/q1-defect-null-unwrap.json"
    ));
}

#[test]
fn regression_q1_defect_shared_map_race() {
    check_fixture(include_str!(
        "../../../fusion-regression/q1-defect-shared-map-race.json"
    ));
}

#[test]
fn regression_q2_root_float_precision() {
    check_fixture(include_str!(
        "../../../fusion-regression/q2-root-float-precision.json"
    ));
}

#[test]
fn regression_q2_blocking_ci() {
    check_fixture(include_str!("../../../fusion-regression/q2-blocking-ci.json"));
}

#[test]
fn regression_minority_1v4() {
    check_fixture(include_str!(
        "../../../fusion-regression/special-minority-1v4.json"
    ));
}

#[test]
fn regression_unique_truth() {
    check_fixture(include_str!(
        "../../../fusion-regression/special-unique-truth.json"
    ));
}

#[test]
fn regression_false_unique_requires_verified_counter_to_refute() {
    check_fixture(include_str!(
        "../../../fusion-regression/special-false-unique.json"
    ));
}
