// SPDX-License-Identifier: Apache-2.0

//! Control-plane host wiring between the FUSION-03 analyzer and the FUSION-04
//! `CanonicalDecision` adjudicator.
//!
//! Delivery must not import control-plane `fusion_analysis` (dependency
//! cycle). This host owns the one-way mapping:
//! `analyze_fusion` → [`FusionAnalysis`] → [`FusionAnalysisFixture`] →
//! [`adjudicate_canonical_decision_from_analysis`].
//!
//! Mapped analyzer findings stay in the model-claim tier. They never become
//! Delivery [`DecisionAuthority::VerifiedFact`] and never override verifier
//! real test results. The host adds no Canonical State write power.

use winwincode_delivery::domain::{
    CanonicalDecision, ComputedDeliveryVerdict, CriterionVerdict, Delivery,
    DeliveryValidationError, FrozenDeliveryCandidate, FusionAnalysisFixture,
    adjudicate_canonical_decision_from_analysis, evidence::ResolvedDeliveryEvidence,
};

use crate::fusion_analysis::{FusionAnalysis, FusionClaimPosition, FusionConflict};

/// Maps one analyzer claim position onto a Delivery criterion outcome.
///
/// Supports → Pass, Opposes → Fail. The mapping is claim-shaped only; it
/// never raises authority above the model-claim tier.
#[must_use]
pub const fn fusion_position_outcome(position: FusionClaimPosition) -> CriterionVerdict {
    match position {
        FusionClaimPosition::Supports => CriterionVerdict::Pass,
        FusionClaimPosition::Opposes => CriterionVerdict::Fail,
    }
}

/// Host-maps FUSION-03 [`FusionAnalysis`] into FUSION-04 [`FusionAnalysisFixture`].
///
/// - Consensus findings become `consensus_outcomes`, one model claim per
///   supporting candidate.
/// - Each conflict contributes `conflict_outcomes`: the evidence-ranked
///   leading side when present, otherwise every position so unresolved
///   evidence rank stays non-unanimous. Each side emits one claim per
///   candidate.
/// - Unique insights and unsupported claims stay single-model claims under
///   `unsupported_model_claims`.
/// - Missing-evidence notes have no delivery outcome and are not mapped.
///
/// Candidate counts never raise authority above tool observation or verified
/// facts. Analyzer evidence quality is recorded as model claims only.
#[must_use]
pub fn map_fusion_analysis_to_fixture(analysis: &FusionAnalysis) -> FusionAnalysisFixture {
    let mut fixture = FusionAnalysisFixture::default();
    for finding in &analysis.consensus {
        push_repeated_outcome(
            &mut fixture.consensus_outcomes,
            fusion_position_outcome(finding.position),
            finding.candidate_ids.len(),
        );
    }
    for conflict in &analysis.conflicts {
        push_conflict_outcomes(&mut fixture, conflict);
    }
    for unique in &analysis.unique_insights {
        push_repeated_outcome(
            &mut fixture.unsupported_model_claims,
            fusion_position_outcome(unique.position),
            unique.candidate_ids.len(),
        );
    }
    for unsupported in &analysis.unsupported_claims {
        fixture
            .unsupported_model_claims
            .push(fusion_position_outcome(unsupported.position));
    }
    fixture
}

/// Control-plane host adjudication path for analyzer findings.
///
/// Maps [`FusionAnalysis`] into [`FusionAnalysisFixture`] and delegates to
/// [`adjudicate_canonical_decision_from_analysis`]. Verifier facts and sealed
/// test Evidence still outrank every mapped model claim. This host never
/// invents a verifier pass and never writes Canonical State.
///
/// # Errors
///
/// Returns the same stale repository, canonical, verifier, or Evidence
/// rejection as the delivery adjudicator.
pub fn adjudicate_canonical_decision_from_fusion_analysis(
    delivery: &Delivery,
    candidate: &FrozenDeliveryCandidate,
    verifier_evidence: Option<&ComputedDeliveryVerdict>,
    evidence: &[ResolvedDeliveryEvidence],
    analysis: &FusionAnalysis,
) -> Result<CanonicalDecision, DeliveryValidationError> {
    let fixture = map_fusion_analysis_to_fixture(analysis);
    adjudicate_canonical_decision_from_analysis(
        delivery,
        candidate,
        verifier_evidence,
        evidence,
        &fixture,
    )
}

fn push_conflict_outcomes(fixture: &mut FusionAnalysisFixture, conflict: &FusionConflict) {
    if let Some(leading) = conflict.leading_position {
        let side = conflict
            .positions
            .iter()
            .find(|position| position.position == leading);
        push_repeated_outcome(
            &mut fixture.conflict_outcomes,
            fusion_position_outcome(leading),
            side.map_or(1, |position| position.candidate_ids.len().max(1)),
        );
        return;
    }
    for position in &conflict.positions {
        push_repeated_outcome(
            &mut fixture.conflict_outcomes,
            fusion_position_outcome(position.position),
            position.candidate_ids.len(),
        );
    }
}

/// Emits one model-claim outcome per supporting candidate (at least one).
fn push_repeated_outcome(
    outcomes: &mut Vec<CriterionVerdict>,
    outcome: CriterionVerdict,
    candidate_count: usize,
) {
    for _ in 0..candidate_count.max(1) {
        outcomes.push(outcome);
    }
}

#[cfg(test)]
mod tests {
    use winwincode_delivery::application::verdict::test_support::{
        VerdictFixtureOutcome, verdict_fixture,
    };
    use winwincode_delivery::domain::{
        DecisionAuthority, DeliveryId, EvidenceRefType, compute_delivery_verdict,
        verification::VerificationFindingConclusion,
    };

    use super::*;
    use crate::fusion_analysis::{
        FusionCandidateClaims, FusionClaim, FusionConflict, FusionEvidence, FusionEvidenceQuality,
        FusionFinding, FusionMissingEvidence, FusionPositionFinding, FusionUnsupportedClaim,
        analyze_fusion,
    };

    const PRODUCED_AT_MILLIS: u64 = 1_800_000_000_100;

    fn claim(
        key: &str,
        summary: &str,
        position: FusionClaimPosition,
        evidence: Vec<FusionEvidence>,
    ) -> FusionClaim {
        FusionClaim {
            claim_key: key.to_owned(),
            summary: summary.to_owned(),
            position,
            evidence,
            required_evidence: Vec::new(),
        }
    }

    fn verified_fail_test() -> Vec<FusionEvidence> {
        vec![FusionEvidence {
            evidence_type: EvidenceRefType::Test,
            source_ref: "verification:failing-suite".to_owned(),
            verified_conclusion: Some(VerificationFindingConclusion::Fail),
        }]
    }

    fn tool_observation() -> Vec<FusionEvidence> {
        vec![FusionEvidence {
            evidence_type: EvidenceRefType::Command,
            source_ref: "command:observed".to_owned(),
            verified_conclusion: None,
        }]
    }

    fn consensus_support_analysis() -> FusionAnalysis {
        FusionAnalysis {
            consensus: vec![FusionFinding {
                claim_key: "claim:tests-pass".to_owned(),
                summary: "The tests pass".to_owned(),
                position: FusionClaimPosition::Supports,
                candidate_ids: vec!["model-a".to_owned(), "model-b".to_owned()],
                strongest_evidence: FusionEvidenceQuality::ToolObservation,
            }],
            conflicts: Vec::new(),
            unique_insights: Vec::new(),
            unsupported_claims: Vec::new(),
            missing_evidence: Vec::new(),
        }
    }

    #[test]
    fn maps_consensus_conflict_and_unsupported_findings_into_fixture_tiers() {
        let analysis = FusionAnalysis {
            consensus: vec![FusionFinding {
                claim_key: "claim:consensus".to_owned(),
                summary: "Consensus claim".to_owned(),
                position: FusionClaimPosition::Supports,
                candidate_ids: vec!["model-a".to_owned(), "model-b".to_owned()],
                strongest_evidence: FusionEvidenceQuality::VerifiedFact,
            }],
            conflicts: vec![FusionConflict {
                claim_key: "claim:conflict".to_owned(),
                summary: "Conflict claim".to_owned(),
                positions: vec![FusionPositionFinding {
                    position: FusionClaimPosition::Supports,
                    candidate_ids: vec!["model-a".to_owned()],
                    strongest_evidence: FusionEvidenceQuality::ToolObservation,
                }],
                leading_position: Some(FusionClaimPosition::Opposes),
            }],
            unique_insights: vec![FusionFinding {
                claim_key: "claim:unique".to_owned(),
                summary: "Unique insight".to_owned(),
                position: FusionClaimPosition::Supports,
                candidate_ids: vec!["model-c".to_owned()],
                strongest_evidence: FusionEvidenceQuality::ToolObservation,
            }],
            unsupported_claims: vec![FusionUnsupportedClaim {
                candidate_id: "model-d".to_owned(),
                claim_key: "claim:unsupported".to_owned(),
                position: FusionClaimPosition::Opposes,
            }],
            missing_evidence: vec![FusionMissingEvidence {
                candidate_id: "model-d".to_owned(),
                claim_key: "claim:unsupported".to_owned(),
                evidence_type: EvidenceRefType::Test,
            }],
        };

        let fixture = map_fusion_analysis_to_fixture(&analysis);

        assert_eq!(
            fixture.consensus_outcomes,
            vec![CriterionVerdict::Pass, CriterionVerdict::Pass]
        );
        // Leading evidence-ranked side wins; unsupported majority does not.
        assert_eq!(fixture.conflict_outcomes, vec![CriterionVerdict::Fail]);
        assert_eq!(
            fixture.unsupported_model_claims,
            vec![CriterionVerdict::Pass, CriterionVerdict::Fail]
        );
        assert_eq!(
            fixture.model_claims(),
            vec![
                CriterionVerdict::Pass,
                CriterionVerdict::Pass,
                CriterionVerdict::Fail,
                CriterionVerdict::Pass,
                CriterionVerdict::Fail,
            ]
        );
    }

    #[test]
    fn unresolved_conflict_records_every_position_as_model_claims() {
        let analysis = FusionAnalysis {
            consensus: Vec::new(),
            conflicts: vec![FusionConflict {
                claim_key: "claim:disputed".to_owned(),
                summary: "Equal evidence rank".to_owned(),
                positions: vec![
                    FusionPositionFinding {
                        position: FusionClaimPosition::Supports,
                        candidate_ids: vec!["model-a".to_owned()],
                        strongest_evidence: FusionEvidenceQuality::VerifiedFact,
                    },
                    FusionPositionFinding {
                        position: FusionClaimPosition::Opposes,
                        candidate_ids: vec!["model-b".to_owned()],
                        strongest_evidence: FusionEvidenceQuality::VerifiedFact,
                    },
                ],
                leading_position: None,
            }],
            unique_insights: Vec::new(),
            unsupported_claims: Vec::new(),
            missing_evidence: Vec::new(),
        };

        let fixture = map_fusion_analysis_to_fixture(&analysis);

        assert!(fixture.consensus_outcomes.is_empty());
        assert_eq!(
            fixture.conflict_outcomes,
            vec![CriterionVerdict::Pass, CriterionVerdict::Fail,]
        );
        assert!(fixture.unsupported_model_claims.is_empty());
    }

    #[test]
    fn analyze_fusion_findings_map_through_host_fixture_path() {
        let candidates = [
            FusionCandidateClaims {
                candidate_id: "model-a".to_owned(),
                claims: vec![
                    claim(
                        "claim:tests-pass",
                        "The tests pass",
                        FusionClaimPosition::Supports,
                        tool_observation(),
                    ),
                    claim(
                        "claim:cache-path",
                        "Cache path unique insight",
                        FusionClaimPosition::Supports,
                        tool_observation(),
                    ),
                ],
            },
            FusionCandidateClaims {
                candidate_id: "model-b".to_owned(),
                claims: vec![claim(
                    "claim:tests-pass",
                    "The tests pass",
                    FusionClaimPosition::Supports,
                    tool_observation(),
                )],
            },
            FusionCandidateClaims {
                candidate_id: "model-c".to_owned(),
                claims: vec![claim(
                    "claim:tests-pass",
                    "The tests pass",
                    FusionClaimPosition::Opposes,
                    verified_fail_test(),
                )],
            },
        ];

        let analysis = analyze_fusion(&candidates).expect("fixture candidates analyze");
        let fixture = map_fusion_analysis_to_fixture(&analysis);

        // Analyzer ranks the verified Opposes side as leading on the conflict.
        // The unique Supports insight stays a single-model claim.
        assert!(analysis.consensus.is_empty());
        assert_eq!(analysis.conflicts.len(), 1);
        assert_eq!(
            analysis.conflicts[0].leading_position,
            Some(FusionClaimPosition::Opposes)
        );
        assert_eq!(analysis.unique_insights.len(), 1);
        assert!(fixture.consensus_outcomes.is_empty());
        assert_eq!(fixture.conflict_outcomes, vec![CriterionVerdict::Fail]);
        assert_eq!(
            fixture.unsupported_model_claims,
            vec![CriterionVerdict::Pass]
        );
        assert_eq!(
            fixture.model_claims(),
            vec![CriterionVerdict::Fail, CriterionVerdict::Pass]
        );
    }

    #[test]
    fn host_adjudication_keeps_model_consensus_at_model_authority() {
        let fixture = verdict_fixture(
            &DeliveryId("dlv_01J00000000000000000000000".to_owned()),
            VerdictFixtureOutcome::Pass,
        );
        let analysis = consensus_support_analysis();

        let decision = adjudicate_canonical_decision_from_fusion_analysis(
            &fixture.delivery,
            &fixture.candidate,
            None,
            &[],
            &analysis,
        )
        .expect("host path adjudicates model-only analysis");

        assert_eq!(decision.outcome(), CriterionVerdict::Pass);
        assert_eq!(decision.authority(), DecisionAuthority::ModelConsensus);
        assert!(decision.canonical_verdict_id().is_none());
        assert_ne!(decision.authority(), DecisionAuthority::VerifiedFact);
    }

    #[test]
    fn host_adjudication_cannot_override_failed_verifier_real_results() {
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
        .expect("failed verifier fixture computes a canonical verdict");
        assert_eq!(computed.verdict().status, CriterionVerdict::Fail);

        // Analyzer-unanimous Pass claims must not overwrite verifier facts.
        let analysis = consensus_support_analysis();
        let decision = adjudicate_canonical_decision_from_fusion_analysis(
            &fixture.delivery,
            &fixture.candidate,
            Some(&computed),
            &fixture.evidence,
            &analysis,
        )
        .expect("host path adjudicates against sealed verifier facts");

        assert_eq!(decision.outcome(), CriterionVerdict::Fail);
        assert_eq!(decision.authority(), DecisionAuthority::VerifiedFact);
        assert_eq!(
            decision.canonical_verdict_id(),
            Some(&computed.verdict().id)
        );
    }

    #[test]
    fn host_mapping_matches_direct_fixture_adjudication() {
        let fixture = verdict_fixture(
            &DeliveryId("dlv_01J00000000000000000000002".to_owned()),
            VerdictFixtureOutcome::Pass,
        );
        let analysis = consensus_support_analysis();
        let mapped = map_fusion_analysis_to_fixture(&analysis);

        let host_decision = adjudicate_canonical_decision_from_fusion_analysis(
            &fixture.delivery,
            &fixture.candidate,
            None,
            &[],
            &analysis,
        )
        .expect("host decision");
        let direct = adjudicate_canonical_decision_from_analysis(
            &fixture.delivery,
            &fixture.candidate,
            None,
            &[],
            &mapped,
        )
        .expect("direct fixture decision");

        assert_eq!(host_decision, direct);
        assert_eq!(host_decision.authority(), DecisionAuthority::ModelConsensus);
    }
}
