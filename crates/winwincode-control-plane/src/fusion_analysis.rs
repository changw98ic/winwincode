// SPDX-License-Identifier: Apache-2.0

//! Pure claim analysis for blind Fusion candidates.

use std::collections::{BTreeMap, BTreeSet};

use winwincode_delivery::domain::{EvidenceRefType, verification::VerificationFindingConclusion};

const MAX_CANDIDATES: usize = 64;
const MAX_CLAIMS: usize = 1_000;
const MAX_TEXT_BYTES: usize = 65_536;
const MAX_REFERENCE_BYTES: usize = 4_096;

/// One candidate's already-extracted claims. The analyzer does no model or text inference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FusionCandidateClaims {
    pub candidate_id: String,
    pub claims: Vec<FusionClaim>,
}

/// One canonical claim made by a candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FusionClaim {
    pub claim_key: String,
    pub summary: String,
    pub position: FusionClaimPosition,
    pub evidence: Vec<FusionEvidence>,
    pub required_evidence: Vec<EvidenceRefType>,
}

/// A candidate's position on one canonical claim.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum FusionClaimPosition {
    Supports,
    Opposes,
}

/// Evidence cited by a candidate. A validated verification finding outranks an observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FusionEvidence {
    pub evidence_type: EvidenceRefType,
    pub source_ref: String,
    pub verified_conclusion: Option<VerificationFindingConclusion>,
}

/// Evidence strength used by analysis. Candidate counts never increase this value.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum FusionEvidenceQuality {
    Unsupported,
    ToolObservation,
    VerifiedFact,
}

/// Complete claim analysis. Categories may overlap: a unique claim can also be unsupported.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FusionAnalysis {
    pub consensus: Vec<FusionFinding>,
    pub conflicts: Vec<FusionConflict>,
    pub unique_insights: Vec<FusionFinding>,
    pub unsupported_claims: Vec<FusionUnsupportedClaim>,
    pub missing_evidence: Vec<FusionMissingEvidence>,
}

/// One consensus or unique finding and its strongest evidence quality.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FusionFinding {
    pub claim_key: String,
    pub summary: String,
    pub position: FusionClaimPosition,
    pub candidate_ids: Vec<String>,
    pub strongest_evidence: FusionEvidenceQuality,
}

/// Opposing positions on one claim. `leading_position` is evidence-ranked, never vote-ranked.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FusionConflict {
    pub claim_key: String,
    pub summary: String,
    pub positions: Vec<FusionPositionFinding>,
    pub leading_position: Option<FusionClaimPosition>,
}

/// Candidates and evidence quality behind one side of a conflict.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FusionPositionFinding {
    pub position: FusionClaimPosition,
    pub candidate_ids: Vec<String>,
    pub strongest_evidence: FusionEvidenceQuality,
}

/// One claim assertion that cites no evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FusionUnsupportedClaim {
    pub candidate_id: String,
    pub claim_key: String,
    pub position: FusionClaimPosition,
}

/// One evidence type explicitly required by a claim but not cited for it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FusionMissingEvidence {
    pub candidate_id: String,
    pub claim_key: String,
    pub evidence_type: EvidenceRefType,
}

/// Invalid fixture/input shape.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FusionAnalysisError {
    InvalidInput,
    DuplicateCandidate,
    DuplicateClaim,
    ClaimMismatch,
}

#[derive(Default)]
struct ClaimGroup<'input> {
    summary: &'input str,
    assertions: Vec<(&'input str, &'input FusionClaim)>,
}

/// Analyzes normalized candidate claims without model calls, persistence, or majority voting.
///
/// # Errors
///
/// Rejects empty, oversized, duplicate, or inconsistently keyed fixture input.
pub fn analyze_fusion(
    candidates: &[FusionCandidateClaims],
) -> Result<FusionAnalysis, FusionAnalysisError> {
    validate_candidates(candidates)?;

    let mut groups = BTreeMap::<&str, ClaimGroup<'_>>::new();
    let mut unsupported_claims = Vec::new();
    let mut missing_evidence = Vec::new();

    for candidate in candidates {
        for claim in &candidate.claims {
            let group = groups
                .entry(&claim.claim_key)
                .or_insert_with(|| ClaimGroup {
                    summary: &claim.summary,
                    assertions: Vec::new(),
                });
            if group.summary != claim.summary {
                return Err(FusionAnalysisError::ClaimMismatch);
            }
            group.assertions.push((&candidate.candidate_id, claim));

            if claim.evidence.is_empty() {
                unsupported_claims.push(FusionUnsupportedClaim {
                    candidate_id: candidate.candidate_id.clone(),
                    claim_key: claim.claim_key.clone(),
                    position: claim.position,
                });
            }
            for required in &claim.required_evidence {
                if !claim
                    .evidence
                    .iter()
                    .any(|evidence| evidence.evidence_type == *required)
                {
                    missing_evidence.push(FusionMissingEvidence {
                        candidate_id: candidate.candidate_id.clone(),
                        claim_key: claim.claim_key.clone(),
                        evidence_type: *required,
                    });
                }
            }
        }
    }

    let mut consensus = Vec::new();
    let mut conflicts = Vec::new();
    let mut unique_insights = Vec::new();

    for (claim_key, group) in groups {
        let positions = position_findings(&group.assertions);
        if positions.len() > 1 {
            let strongest = positions
                .iter()
                .map(|position| position.strongest_evidence)
                .max()
                .unwrap_or(FusionEvidenceQuality::Unsupported);
            let mut leaders = positions
                .iter()
                .filter(|position| position.strongest_evidence == strongest);
            let leading_position = leaders.next().map(|position| position.position);
            let leading_position = if leaders.next().is_none() {
                leading_position
            } else {
                None
            };
            conflicts.push(FusionConflict {
                claim_key: claim_key.to_owned(),
                summary: group.summary.to_owned(),
                positions,
                leading_position,
            });
            continue;
        }

        let Some(position) = positions.first() else {
            continue;
        };
        let finding = FusionFinding {
            claim_key: claim_key.to_owned(),
            summary: group.summary.to_owned(),
            position: position.position,
            candidate_ids: position.candidate_ids.clone(),
            strongest_evidence: position.strongest_evidence,
        };
        if group.assertions.len() == 1 {
            unique_insights.push(finding);
        } else {
            consensus.push(finding);
        }
    }

    Ok(FusionAnalysis {
        consensus,
        conflicts,
        unique_insights,
        unsupported_claims,
        missing_evidence,
    })
}

fn position_findings(assertions: &[(&str, &FusionClaim)]) -> Vec<FusionPositionFinding> {
    let mut positions =
        BTreeMap::<FusionClaimPosition, (Vec<String>, FusionEvidenceQuality)>::new();
    for (candidate_id, claim) in assertions {
        let entry = positions
            .entry(claim.position)
            .or_insert_with(|| (Vec::new(), FusionEvidenceQuality::Unsupported));
        entry.0.push((*candidate_id).to_owned());
        entry.1 = entry.1.max(strongest_evidence(&claim.evidence));
    }
    positions
        .into_iter()
        .map(
            |(position, (candidate_ids, strongest_evidence))| FusionPositionFinding {
                position,
                candidate_ids,
                strongest_evidence,
            },
        )
        .collect()
}

fn strongest_evidence(evidence: &[FusionEvidence]) -> FusionEvidenceQuality {
    evidence
        .iter()
        .map(|evidence| {
            if evidence.verified_conclusion.is_some() {
                FusionEvidenceQuality::VerifiedFact
            } else {
                FusionEvidenceQuality::ToolObservation
            }
        })
        .max()
        .unwrap_or(FusionEvidenceQuality::Unsupported)
}

fn validate_candidates(candidates: &[FusionCandidateClaims]) -> Result<(), FusionAnalysisError> {
    if candidates.is_empty() || candidates.len() > MAX_CANDIDATES {
        return Err(FusionAnalysisError::InvalidInput);
    }
    let mut candidate_ids = BTreeSet::new();
    for candidate in candidates {
        if !valid_text(&candidate.candidate_id, MAX_REFERENCE_BYTES)
            || candidate.claims.len() > MAX_CLAIMS
        {
            return Err(FusionAnalysisError::InvalidInput);
        }
        if !candidate_ids.insert(candidate.candidate_id.as_str()) {
            return Err(FusionAnalysisError::DuplicateCandidate);
        }
        let mut claim_keys = BTreeSet::new();
        for claim in &candidate.claims {
            if !valid_text(&claim.claim_key, MAX_REFERENCE_BYTES)
                || !valid_text(&claim.summary, MAX_TEXT_BYTES)
                || claim.evidence.len() > MAX_CLAIMS
                || claim.required_evidence.len() > MAX_CLAIMS
                || claim
                    .evidence
                    .iter()
                    .any(|evidence| !valid_text(&evidence.source_ref, MAX_REFERENCE_BYTES))
            {
                return Err(FusionAnalysisError::InvalidInput);
            }
            if !claim_keys.insert(claim.claim_key.as_str()) {
                return Err(FusionAnalysisError::DuplicateClaim);
            }
        }
    }
    Ok(())
}

fn valid_text(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(position: FusionClaimPosition, evidence: Vec<FusionEvidence>) -> FusionClaim {
        FusionClaim {
            claim_key: "claim:tests-pass".to_owned(),
            summary: "The tests pass".to_owned(),
            position,
            evidence,
            required_evidence: vec![EvidenceRefType::Test],
        }
    }

    fn verified_test_evidence() -> Vec<FusionEvidence> {
        vec![FusionEvidence {
            evidence_type: EvidenceRefType::Test,
            source_ref: "verification:passing-suite".to_owned(),
            verified_conclusion: Some(VerificationFindingConclusion::Pass),
        }]
    }

    fn tool_command_evidence() -> Vec<FusionEvidence> {
        vec![FusionEvidence {
            evidence_type: EvidenceRefType::Command,
            source_ref: "command:ls-tests".to_owned(),
            verified_conclusion: None,
        }]
    }

    #[test]
    fn two_to_one_cannot_override_stronger_evidence() {
        let candidates = [
            FusionCandidateClaims {
                candidate_id: "model-a".to_owned(),
                claims: vec![claim(FusionClaimPosition::Supports, Vec::new())],
            },
            FusionCandidateClaims {
                candidate_id: "model-b".to_owned(),
                claims: vec![claim(FusionClaimPosition::Supports, Vec::new())],
            },
            FusionCandidateClaims {
                candidate_id: "model-c".to_owned(),
                claims: vec![claim(
                    FusionClaimPosition::Opposes,
                    vec![FusionEvidence {
                        evidence_type: EvidenceRefType::Test,
                        source_ref: "verification:failing-test".to_owned(),
                        verified_conclusion: Some(VerificationFindingConclusion::Fail),
                    }],
                )],
            },
        ];

        let analysis = analyze_fusion(&candidates).expect("fixture is valid");

        assert_eq!(analysis.conflicts.len(), 1);
        assert_eq!(
            analysis.conflicts[0].leading_position,
            Some(FusionClaimPosition::Opposes)
        );
        assert_eq!(
            analysis.conflicts[0].positions[0].strongest_evidence,
            FusionEvidenceQuality::Unsupported
        );
        assert_eq!(analysis.consensus.len(), 0);
        assert_eq!(analysis.unique_insights.len(), 0);
        assert_eq!(analysis.unsupported_claims.len(), 2);
        assert_eq!(analysis.missing_evidence.len(), 2);
    }

    #[test]
    fn equal_evidence_quality_leaves_conflict_unresolved() {
        let candidates = [
            FusionCandidateClaims {
                candidate_id: "model-a".to_owned(),
                claims: vec![claim(
                    FusionClaimPosition::Supports,
                    verified_test_evidence(),
                )],
            },
            FusionCandidateClaims {
                candidate_id: "model-b".to_owned(),
                claims: vec![claim(
                    FusionClaimPosition::Opposes,
                    vec![FusionEvidence {
                        evidence_type: EvidenceRefType::Test,
                        source_ref: "verification:counter-test".to_owned(),
                        verified_conclusion: Some(VerificationFindingConclusion::Fail),
                    }],
                )],
            },
        ];

        let analysis = analyze_fusion(&candidates).expect("fixture is valid");

        assert_eq!(analysis.conflicts.len(), 1);
        assert_eq!(analysis.conflicts[0].leading_position, None);
        assert!(
            analysis.conflicts[0]
                .positions
                .iter()
                .all(|position| position.strongest_evidence == FusionEvidenceQuality::VerifiedFact)
        );
        assert!(analysis.unsupported_claims.is_empty());
        assert!(analysis.missing_evidence.is_empty());
    }

    #[test]
    fn tool_observation_loses_to_verified_fact_even_when_outnumbered() {
        let candidates = [
            FusionCandidateClaims {
                candidate_id: "model-a".to_owned(),
                claims: vec![claim(
                    FusionClaimPosition::Supports,
                    tool_command_evidence(),
                )],
            },
            FusionCandidateClaims {
                candidate_id: "model-b".to_owned(),
                claims: vec![claim(
                    FusionClaimPosition::Supports,
                    tool_command_evidence(),
                )],
            },
            FusionCandidateClaims {
                candidate_id: "model-c".to_owned(),
                claims: vec![claim(
                    FusionClaimPosition::Opposes,
                    vec![FusionEvidence {
                        evidence_type: EvidenceRefType::Test,
                        source_ref: "verification:failing-test".to_owned(),
                        verified_conclusion: Some(VerificationFindingConclusion::Fail),
                    }],
                )],
            },
        ];

        let analysis = analyze_fusion(&candidates).expect("fixture is valid");

        assert_eq!(analysis.conflicts.len(), 1);
        assert_eq!(
            analysis.conflicts[0].leading_position,
            Some(FusionClaimPosition::Opposes)
        );
        let supports = analysis.conflicts[0]
            .positions
            .iter()
            .find(|position| position.position == FusionClaimPosition::Supports)
            .expect("supports side present");
        assert_eq!(supports.candidate_ids, vec!["model-a", "model-b"]);
        assert_eq!(
            supports.strongest_evidence,
            FusionEvidenceQuality::ToolObservation
        );
    }

    #[test]
    fn consensus_and_unique_insight_are_separated_by_support_count() {
        let candidates = [
            FusionCandidateClaims {
                candidate_id: "model-a".to_owned(),
                claims: vec![
                    claim(FusionClaimPosition::Supports, verified_test_evidence()),
                    FusionClaim {
                        claim_key: "claim:unique-path".to_owned(),
                        summary: "Only one model found the cache path".to_owned(),
                        position: FusionClaimPosition::Supports,
                        evidence: tool_command_evidence(),
                        required_evidence: Vec::new(),
                    },
                ],
            },
            FusionCandidateClaims {
                candidate_id: "model-b".to_owned(),
                claims: vec![claim(
                    FusionClaimPosition::Supports,
                    tool_command_evidence(),
                )],
            },
            FusionCandidateClaims {
                candidate_id: "model-c".to_owned(),
                claims: vec![claim(
                    FusionClaimPosition::Supports,
                    verified_test_evidence(),
                )],
            },
        ];

        let analysis = analyze_fusion(&candidates).expect("fixture is valid");

        assert_eq!(analysis.consensus.len(), 1);
        assert_eq!(analysis.consensus[0].claim_key, "claim:tests-pass");
        assert_eq!(
            analysis.consensus[0].candidate_ids,
            vec!["model-a", "model-b", "model-c"]
        );
        assert_eq!(
            analysis.consensus[0].strongest_evidence,
            FusionEvidenceQuality::VerifiedFact
        );
        assert_eq!(analysis.unique_insights.len(), 1);
        assert_eq!(analysis.unique_insights[0].claim_key, "claim:unique-path");
        assert_eq!(analysis.unique_insights[0].candidate_ids, vec!["model-a"]);
        assert!(analysis.conflicts.is_empty());
    }

    #[test]
    fn unique_unsupported_claim_and_missing_evidence_overlap() {
        let candidates = [FusionCandidateClaims {
            candidate_id: "model-a".to_owned(),
            claims: vec![
                FusionClaim {
                    claim_key: "claim:opinion".to_owned(),
                    summary: "This design is better".to_owned(),
                    position: FusionClaimPosition::Supports,
                    evidence: Vec::new(),
                    required_evidence: Vec::new(),
                },
                FusionClaim {
                    claim_key: "claim:partial-evidence".to_owned(),
                    summary: "Tests exist but command proof is required".to_owned(),
                    position: FusionClaimPosition::Supports,
                    evidence: verified_test_evidence(),
                    required_evidence: vec![EvidenceRefType::Test, EvidenceRefType::Command],
                },
            ],
        }];

        let analysis = analyze_fusion(&candidates).expect("fixture is valid");

        assert_eq!(analysis.unique_insights.len(), 2);
        assert_eq!(analysis.unsupported_claims.len(), 1);
        assert_eq!(analysis.unsupported_claims[0].claim_key, "claim:opinion");
        assert_eq!(analysis.missing_evidence.len(), 1);
        assert_eq!(
            analysis.missing_evidence[0].claim_key,
            "claim:partial-evidence"
        );
        assert_eq!(
            analysis.missing_evidence[0].evidence_type,
            EvidenceRefType::Command
        );
        assert!(analysis.consensus.is_empty());
        assert!(analysis.conflicts.is_empty());
    }

    #[test]
    fn rejects_duplicate_candidate_ids_and_mismatched_claim_summaries() {
        let duplicate = [
            FusionCandidateClaims {
                candidate_id: "model-a".to_owned(),
                claims: vec![claim(FusionClaimPosition::Supports, Vec::new())],
            },
            FusionCandidateClaims {
                candidate_id: "model-a".to_owned(),
                claims: vec![claim(FusionClaimPosition::Opposes, Vec::new())],
            },
        ];
        assert_eq!(
            analyze_fusion(&duplicate),
            Err(FusionAnalysisError::DuplicateCandidate)
        );

        let mismatched = [
            FusionCandidateClaims {
                candidate_id: "model-a".to_owned(),
                claims: vec![claim(FusionClaimPosition::Supports, Vec::new())],
            },
            FusionCandidateClaims {
                candidate_id: "model-b".to_owned(),
                claims: vec![FusionClaim {
                    summary: "The tests fail".to_owned(),
                    ..claim(FusionClaimPosition::Opposes, Vec::new())
                }],
            },
        ];
        assert_eq!(
            analyze_fusion(&mismatched),
            Err(FusionAnalysisError::ClaimMismatch)
        );

        assert_eq!(analyze_fusion(&[]), Err(FusionAnalysisError::InvalidInput));
    }
}
