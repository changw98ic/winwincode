// SPDX-License-Identifier: Apache-2.0
//!
//! Agent Fusion Engine knowledge core (ADR-0037 P0–P3).
//!
//! - P0 [`ClaimIdentity`] canonical claim id (namespace + key)
//! - P1 claim union into [`ClaimGraph`]
//! - P2 [`FusionEvidenceRecord`] store with independence groups
//! - P3 [`ClaimState`] machine: disagreement investigates; only verified
//!   contradiction subtracts; consensus is metadata, not evidence.

use std::collections::{BTreeMap, BTreeSet};

use crate::evidence::{ClaimTargetLike, EvidenceLedger};
use serde::{Deserialize, Serialize};
use winwincode_delivery::domain::EvidenceRefType;

use crate::analysis::{FusionCandidateClaims, FusionClaim, FusionClaimPosition};

/// P0: canonical claim identity. Display keeps `claim:{namespace}:{key}`.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimIdentity {
    pub namespace: String,
    pub key: String,
}

impl ClaimIdentity {
    /// Parses `claim:defect-null-unwrap`, `defect:null-unwrap`, `defect-null-unwrap`.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        let trimmed = collapsed.trim();
        let lower = trimmed.to_ascii_lowercase();
        let body = if let Some(rest) = lower.strip_prefix("claim:") {
            // Recover original casing from the raw trimmed form.
            trimmed[trimmed.len() - rest.len()..].trim()
        } else {
            trimmed
        };
        if let Some((ns, key)) = body.split_once(':') {
            return Self {
                namespace: ns.trim().to_ascii_lowercase(),
                key: key.trim().to_ascii_lowercase(),
            };
        }
        // `defect-null-unwrap` → namespace=defect, key=null-unwrap when possible.
        if let Some((ns, key)) = body.split_once('-') {
            let ns_l = ns.to_ascii_lowercase();
            if matches!(ns_l.as_str(), "defect" | "root" | "claim" | "risk" | "plan") {
                return Self {
                    namespace: ns_l,
                    key: key.to_ascii_lowercase(),
                };
            }
        }
        Self {
            namespace: "claim".to_owned(),
            key: body.to_ascii_lowercase(),
        }
    }

    #[must_use]
    pub fn canonical_key(&self) -> String {
        format!("{}:{}", self.namespace, self.key)
    }

    /// UI display is `namespace:key` — never double-prefix with `claim:`.
    #[must_use]
    pub fn display_key(&self) -> String {
        self.canonical_key()
    }
}

pub use crate::context::{ClaimState, EvidenceDirection, EvidenceStrength, FusionEvidenceRecord};

/// P1/P3: one node of the claim graph.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimNode {
    pub id: String,
    pub identity: ClaimIdentity,
    pub display_key: String,
    pub summary: String,
    pub state: ClaimState,
    pub supporters: Vec<String>,
    pub opponents: Vec<String>,
    pub evidence_ids: Vec<String>,
    pub counter_evidence_ids: Vec<String>,
    pub unknowns: Vec<String>,
    /// Consensus count is metadata only (ADR-0037 principle 5).
    pub supporter_count: usize,
    pub opponent_count: usize,
    pub has_verified_counter: bool,
}

/// Full claim graph after union + conflict detection.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimGraph {
    pub claims: Vec<ClaimNode>,
    pub evidence: Vec<FusionEvidenceRecord>,
}

impl ClaimGraph {
    #[must_use]
    pub fn claim(&self, id: &str) -> Option<&ClaimNode> {
        self.claims.iter().find(|claim| claim.id == id)
    }

    /// Minority / unique claims that still need investigation (not conflict-only).
    #[must_use]
    pub fn needs_investigation(&self) -> Vec<&ClaimNode> {
        self.claims
            .iter()
            .filter(|claim| needs_investigation(claim))
            .collect()
    }

    /// Minority / unique claims are retained (never majority-killed).
    #[must_use]
    pub fn disputed_or_investigating(&self) -> Vec<&ClaimNode> {
        self.claims
            .iter()
            .filter(|claim| {
                matches!(
                    claim.state,
                    ClaimState::Disputed
                        | ClaimState::Investigating
                        | ClaimState::Escalated
                        | ClaimState::UnverifiedAbsence
                )
            })
            .collect()
    }
}

fn evidence_strength_for(claim: &FusionClaim) -> EvidenceStrength {
    let mut best = EvidenceStrength::Speculation;
    for evidence in &claim.evidence {
        let strength = if matches!(
            evidence.evidence_type,
            EvidenceRefType::Test
                | EvidenceRefType::Command
                | EvidenceRefType::Diff
                | EvidenceRefType::RuntimeEvent
        ) {
            EvidenceStrength::StrongInference
        } else {
            EvidenceStrength::WeakInference
        };
        best = best.max(strength);
    }
    best
}

fn independence_group_for(claim: &FusionClaim) -> String {
    // Cluster by claim identity + evidence source refs (P2 dedup).
    let mut sources: Vec<String> = claim
        .evidence
        .iter()
        .map(|evidence| evidence.source_ref.to_ascii_lowercase())
        .collect();
    sources.sort();
    sources.dedup();
    format!(
        "ig:{}:{}",
        crate::analysis::claim_group_key(&claim.claim_key),
        sources.join("|")
    )
}

/// P1–P3: union candidate claims into a claim graph and assign states.
///
/// Disagreement → [`ClaimState::Disputed`] (investigate next). Unique insight
/// without opposition → [`ClaimState::Supported`] (retain, do not drop).
/// `Refuted` is never assigned here without verified counter-evidence.
#[must_use]
pub fn build_claim_graph(candidates: &[FusionCandidateClaims]) -> ClaimGraph {
    let mut nodes: BTreeMap<String, ClaimNode> = BTreeMap::new();
    let mut evidence: Vec<FusionEvidenceRecord> = Vec::new();
    let mut id_by_group: BTreeMap<String, String> = BTreeMap::new();
    let mut seen_evidence: BTreeSet<String> = BTreeSet::new();

    for candidate in candidates {
        for claim in &candidate.claims {
            let identity = ClaimIdentity::parse(&claim.claim_key);
            let group = identity.canonical_key();
            let claim_id = id_by_group
                .entry(group.clone())
                .or_insert_with(|| format!("c_{}", simple_hash(&group)));
            let node = nodes.entry(claim_id.clone()).or_insert_with(|| ClaimNode {
                id: claim_id.clone(),
                display_key: identity.display_key(),
                identity: identity.clone(),
                summary: claim.summary.clone(),
                state: ClaimState::Discovered,
                supporters: Vec::new(),
                opponents: Vec::new(),
                evidence_ids: Vec::new(),
                counter_evidence_ids: Vec::new(),
                unknowns: Vec::new(),
                supporter_count: 0,
                opponent_count: 0,
                has_verified_counter: false,
            });

            match claim.position {
                FusionClaimPosition::Supports => {
                    node.supporters.push(candidate.candidate_id.clone());
                    node.supporter_count += 1;
                }
                FusionClaimPosition::Opposes => {
                    node.opponents.push(candidate.candidate_id.clone());
                    node.opponent_count += 1;
                }
            }

            let direction = match claim.position {
                FusionClaimPosition::Supports => EvidenceDirection::Support,
                FusionClaimPosition::Opposes => EvidenceDirection::Counter,
            };
            for (index, item) in claim.evidence.iter().enumerate() {
                let evidence_id = format!("ev_{}_{}_{}", claim_id, candidate.candidate_id, index);
                if !seen_evidence.insert(evidence_id.clone()) {
                    continue;
                }
                let record = FusionEvidenceRecord {
                    id: evidence_id.clone(),
                    claim_id: claim_id.clone(),
                    provider: format!("{:?}", item.evidence_type),
                    direction,
                    kind: format!("{:?}", item.evidence_type),
                    strength: evidence_strength_for(claim),
                    facts: vec![item.source_ref.clone()],
                    source_refs: vec![item.source_ref.clone()],
                    independence_group: independence_group_for(claim),
                    // Candidate evidence is a claim-side citation. Verification
                    // facts enter only through a bound runtime/Verifier record.
                    verified: false,
                    invalidated: false,
                };
                match direction {
                    EvidenceDirection::Support => node.evidence_ids.push(evidence_id.clone()),
                    EvidenceDirection::Counter => {
                        node.counter_evidence_ids.push(evidence_id.clone());
                    }
                }
                evidence.push(record);
            }
        }
    }

    for node in nodes.values_mut() {
        node.state = classify_state(node);
    }

    ClaimGraph {
        claims: nodes.into_values().collect(),
        evidence,
    }
}

fn classify_state(node: &ClaimNode) -> ClaimState {
    let opposed = node.opponent_count > 0;
    let supported = node.supporter_count > 0;
    // Consensus is metadata — numbers alone never confirm or refute (ADR-0037).
    if opposed && supported {
        return ClaimState::Disputed;
    }
    if supported && node.has_verified_counter {
        return ClaimState::Disputed;
    }
    if supported {
        return ClaimState::Supported;
    }
    if opposed {
        // Unanimous "not present" without direct safety proof is not confirmed false.
        if !node.has_verified_counter && is_high_risk_absence(node) {
            return ClaimState::UnverifiedAbsence;
        }
        return ClaimState::Discovered;
    }
    ClaimState::Discovered
}

/// High-risk defect classes where unanimous negative still needs verification.
#[must_use]
pub fn is_high_risk_absence(node: &ClaimNode) -> bool {
    let hay = format!(
        "{} {}",
        node.display_key.to_ascii_lowercase(),
        node.summary.to_ascii_lowercase()
    );
    [
        "race",
        "null",
        "security",
        "injection",
        "deadlock",
        "leak",
        "blocking",
        "regression",
        "shared",
        "fixture",
        "mutation",
        "concurrent",
    ]
    .iter()
    .any(|needle| hay.contains(needle))
}

/// Investigation entry is not conflict-only (false consensus / weak support / coverage).
#[must_use]
pub fn needs_investigation(node: &ClaimNode) -> bool {
    match node.state {
        // `UnverifiedAbsence` joins the disputed set: unanimous "not present"
        // without direct safety proof is a false-consensus blind spot.
        // A citation is still a model claim. Verified support must transition
        // the node to Confirmed before it can leave the investigation queue.
        ClaimState::Disputed
        | ClaimState::Investigating
        | ClaimState::Escalated
        | ClaimState::UnverifiedAbsence
        | ClaimState::Supported
        | ClaimState::Discovered => true,
        _ => false,
    }
}

/// Mark a claim confirmed after evidence verification (R4 JEV / verifier).
pub fn confirm_claim(graph: &mut ClaimGraph, claim_id: &str) -> bool {
    for claim in &mut graph.claims {
        if claim.id == claim_id {
            claim.state = ClaimState::Confirmed;
            return true;
        }
    }
    false
}

/// Legacy unbound entry point cannot prove proposition, scope, version or
/// source ownership. It therefore never refutes; use the ledger-bound API.
pub fn refute_claim(graph: &mut ClaimGraph, claim_id: &str) -> bool {
    let _ = (graph, claim_id);
    false
}

/// Refute only when the ledger contains live machine receipts and a verified
/// counter for the exact proposition, scope and version being subtracted.
pub fn refute_claim_with_verified_counter<T: ClaimTargetLike>(
    graph: &mut ClaimGraph,
    claim_id: &str,
    target: &T,
    ledger: &EvidenceLedger,
) -> bool {
    if !ledger.can_refute(target) {
        return false;
    }
    for claim in &mut graph.claims {
        if claim.id == claim_id {
            claim.has_verified_counter = true;
            claim.state = ClaimState::Refuted;
            return true;
        }
    }
    false
}

pub fn mark_investigating(graph: &mut ClaimGraph, claim_id: &str) -> bool {
    for claim in &mut graph.claims {
        if claim.id == claim_id {
            claim.state = ClaimState::Investigating;
            return true;
        }
    }
    false
}

pub fn mark_unresolved(graph: &mut ClaimGraph, claim_id: &str) -> bool {
    for claim in &mut graph.claims {
        if claim.id == claim_id {
            claim.state = ClaimState::Unresolved;
            return true;
        }
    }
    false
}

fn simple_hash(value: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::FusionEvidence;
    use crate::evidence::{
        ClaimTarget, ClaimVerification, SourceReceipt, SourceReceiptKind, VerificationConclusion,
    };
    use winwincode_delivery::domain::verification::VerificationFindingConclusion;

    fn claim(key: &str, position: FusionClaimPosition, verified: bool) -> FusionClaim {
        FusionClaim {
            claim_key: key.to_owned(),
            summary: "shared fixture race".to_owned(),
            position,
            evidence: vec![FusionEvidence {
                evidence_type: EvidenceRefType::Command,
                source_ref: if verified {
                    "test:race-repro"
                } else {
                    "code:fixture"
                }
                .to_owned(),
                verified_conclusion: if verified {
                    Some(VerificationFindingConclusion::Fail)
                } else {
                    None
                },
            }],
            required_evidence: Vec::new(),
        }
    }

    fn candidate(id: &str, claims: Vec<FusionClaim>) -> FusionCandidateClaims {
        FusionCandidateClaims {
            candidate_id: id.to_owned(),
            claims,
        }
    }

    #[test]
    fn p0_claim_identity_is_canonical() {
        let a = ClaimIdentity::parse("claim:defect-null-unwrap");
        let b = ClaimIdentity::parse("defect:null-unwrap");
        let c = ClaimIdentity::parse("CLAIM:defect-null-unwrap");
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert_eq!(a.display_key(), "defect:null-unwrap");
        assert_eq!(a.canonical_key(), "defect:null-unwrap");
        let style = ClaimIdentity::parse("claim:blocking-ci");
        assert_eq!(style.display_key(), "claim:blocking-ci");
        assert!(!style.display_key().starts_with("claim:claim:"));
    }

    #[test]
    fn p1_union_keeps_unique_and_minority_claims() {
        let graph = build_claim_graph(&[
            candidate(
                "m1",
                vec![claim(
                    "root:shared-fixture-race",
                    FusionClaimPosition::Supports,
                    false,
                )],
            ),
            candidate(
                "m2",
                vec![claim(
                    "root:shared-fixture-race",
                    FusionClaimPosition::Opposes,
                    false,
                )],
            ),
            candidate(
                "m3",
                vec![claim(
                    "defect:unique-path",
                    FusionClaimPosition::Supports,
                    false,
                )],
            ),
        ]);
        assert_eq!(graph.claims.len(), 2);
        let race = graph
            .claims
            .iter()
            .find(|c| c.identity.canonical_key() == "root:shared-fixture-race")
            .expect("race claim retained");
        assert_eq!(race.state, ClaimState::Disputed);
        assert_eq!(race.supporter_count, 1);
        assert_eq!(race.opponent_count, 1);
        let unique = graph
            .claims
            .iter()
            .find(|c| c.identity.canonical_key() == "defect:unique-path")
            .expect("unique claim retained");
        assert_eq!(unique.state, ClaimState::Supported);
        assert!(!unique.evidence_ids.is_empty());
        assert_eq!(graph.needs_investigation().len(), 2);
    }

    #[test]
    fn p3_refuted_requires_verified_counter_evidence() {
        let mut graph = build_claim_graph(&[candidate(
            "m1",
            vec![claim(
                "root:phantom-race",
                FusionClaimPosition::Supports,
                false,
            )],
        )]);
        let id = graph.claims[0].id.clone();
        assert!(!graph.claims[0].has_verified_counter);
        assert!(graph.evidence.iter().all(|evidence| !evidence.verified));
        assert!(!refute_claim(&mut graph, &id));
        assert_ne!(graph.claims[0].state, ClaimState::Refuted);

        let mut ledger = EvidenceLedger::default();
        let receipt = ledger
            .register_source_receipt(SourceReceipt {
                id: "counter-receipt".to_owned(),
                kind: SourceReceiptKind::Test,
                locator: "test:phantom-race-counter".to_owned(),
                version: "source-digest-1".to_owned(),
                execution_owner: "worker-run-42".to_owned(),
                content_digest: "5e".repeat(32),
            })
            .expect("counter receipt registers");
        ledger
            .record_claim_verification(ClaimVerification {
                id: "counter-verification".to_owned(),
                proposition: "root:phantom-race".to_owned(),
                scope: "shared fixture race".to_owned(),
                version: "source-digest-1".to_owned(),
                conclusion: VerificationConclusion::Counter,
                source_receipt_ids: vec![receipt.id],
                verifier: "independent-verifier".to_owned(),
            })
            .expect("counter verification registers");
        let target = ClaimTarget {
            proposition: "root:phantom-race".to_owned(),
            scope: "shared fixture race".to_owned(),
            version: "source-digest-1".to_owned(),
        };
        assert!(refute_claim_with_verified_counter(
            &mut graph, &id, &target, &ledger,
        ));
        assert_eq!(graph.claims[0].state, ClaimState::Refuted);
    }

    #[test]
    fn four_unverified_opponents_cannot_refute_supported_claim() {
        let mut graph = build_claim_graph(&[
            candidate(
                "minority",
                vec![claim(
                    "root:shared-fixture-race",
                    FusionClaimPosition::Supports,
                    false,
                )],
            ),
            candidate(
                "opponent-1",
                vec![claim(
                    "root:shared-fixture-race",
                    FusionClaimPosition::Opposes,
                    false,
                )],
            ),
            candidate(
                "opponent-2",
                vec![claim(
                    "root:shared-fixture-race",
                    FusionClaimPosition::Opposes,
                    false,
                )],
            ),
            candidate(
                "opponent-3",
                vec![claim(
                    "root:shared-fixture-race",
                    FusionClaimPosition::Opposes,
                    false,
                )],
            ),
            candidate(
                "opponent-4",
                vec![claim(
                    "root:shared-fixture-race",
                    FusionClaimPosition::Opposes,
                    false,
                )],
            ),
        ]);

        assert_eq!(graph.claims.len(), 1);
        let retained_id = graph.claims[0].id.clone();
        assert_eq!(graph.claims[0].supporter_count, 1);
        assert_eq!(graph.claims[0].opponent_count, 4);
        assert_eq!(graph.claims[0].state, ClaimState::Disputed);
        assert_ne!(graph.claims[0].state, ClaimState::Refuted);
        assert!(!refute_claim(&mut graph, &retained_id));
        assert_eq!(graph.claims.len(), 1);
        assert_ne!(graph.claims[0].state, ClaimState::Refuted);
    }

    #[test]
    fn false_consensus_high_risk_absence_needs_verification() {
        let graph = build_claim_graph(&[candidate(
            "m1",
            vec![claim(
                "defect:shared-map-race",
                FusionClaimPosition::Opposes,
                false,
            )],
        )]);
        let node = &graph.claims[0];
        assert_eq!(node.state, ClaimState::UnverifiedAbsence);
        assert!(needs_investigation(node));
        assert!(is_high_risk_absence(node));
    }

    #[test]
    fn p2_evidence_has_independence_groups() {
        let graph = build_claim_graph(&[
            candidate(
                "m1",
                vec![claim("root:x", FusionClaimPosition::Supports, false)],
            ),
            candidate(
                "m2",
                vec![claim("root:x", FusionClaimPosition::Supports, false)],
            ),
        ]);
        assert!(graph.evidence.len() >= 2);
        let groups: BTreeSet<_> = graph
            .evidence
            .iter()
            .map(|e| e.independence_group.as_str())
            .collect();
        // Same source refs → same independence group (not 2 votes).
        assert_eq!(groups.len(), 1);
    }
}
