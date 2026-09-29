// SPDX-License-Identifier: Apache-2.0
//!
//! ADR-0038 Phase 1 instrumentation + JEV rebuild integrity gate.
//!
//! Three-layer context (Canonical / Active / Archive), rebuild checks that are
//! deterministic (no LLM), and joint Fusion×JEV metrics types.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// P3 claim lifecycle (ADR-0037). `Refuted` requires verified counter-evidence.
/// `UnverifiedAbsence` = unanimous "not present" without direct safety proof
/// (false-consensus blind spot; still needs verification).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ClaimState {
    Discovered,
    Supported,
    Confirmed,
    Disputed,
    Investigating,
    Refuted,
    Escalated,
    Unresolved,
    UnverifiedAbsence,
}

/// Evidence strength is categorical — never a fake-precise score.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EvidenceStrength {
    Direct,
    StrongInference,
    WeakInference,
    Speculation,
}

/// P2: one evidence object (models explain evidence; they do not invent facts).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FusionEvidenceRecord {
    pub id: String,
    pub claim_id: String,
    pub provider: String,
    pub direction: EvidenceDirection,
    pub kind: String,
    pub strength: EvidenceStrength,
    pub facts: Vec<String>,
    pub source_refs: Vec<String>,
    pub independence_group: String,
    pub verified: bool,
    pub invalidated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvidenceDirection {
    Support,
    Counter,
}

// ---------------------------------------------------------------------------
// L1 Canonical State (JEV must not change semantics)
// ---------------------------------------------------------------------------

/// L1: facts Fusion/Verifier own. JEV rebuild may re-present but not alter.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CanonicalState {
    pub task_objective: String,
    pub acceptance_criteria: Vec<String>,
    pub hard_constraints: Vec<String>,
    pub claims: Vec<CanonicalClaimFact>,
    pub evidence: Vec<FusionEvidenceRecord>,
    pub current_plan: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CanonicalClaimFact {
    pub display_key: String,
    pub state: ClaimState,
    pub evidence_ids: Vec<String>,
    pub basis: ClaimBasis,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClaimBasis {
    ModelConsensus,
    ToolEvidence,
    Verifier,
}

/// L2: what the agent sees after JEV build.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveContext {
    pub current_goal: String,
    pub current_phase: String,
    pub priority_claims: Vec<String>,
    pub evidence_facts: Vec<String>,
    pub code_snippets: Vec<String>,
    pub constraints: Vec<String>,
    pub recent_observations: Vec<String>,
}

/// L3 archive entry (never silently mutated).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveEntry {
    pub id: String,
    pub kind: String,
    pub body: String,
}

// ---------------------------------------------------------------------------
// Context snapshot + integrity check + rebuild gate
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextSnapshot {
    pub label: String,
    pub canonical: CanonicalState,
    pub active: ActiveContext,
    pub archive: Vec<ArchiveEntry>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrityReport {
    pub pass: bool,
    /// JEV may re-present canonical truth, but only Fusion/Verifier may update it.
    pub canonical_changed: bool,
    pub active_goal_changed: bool,
    pub changed_active_evidence: Vec<String>,
    pub missing_active_claims: Vec<String>,
    pub unknown_active_claims: Vec<String>,
    pub missing_active_constraints: Vec<String>,
    pub missing_constraints: Vec<String>,
    pub missing_confirmed_claims: Vec<String>,
    pub dangling_evidence_ids: Vec<String>,
    pub state_regressions: Vec<String>,
    pub hallucinated_claims: Vec<String>,
    pub hallucinated_evidence: Vec<String>,
    pub stale_refuted_in_active: Vec<String>,
    pub invalid_archive_ids: Vec<String>,
    pub invalid_claim_ids: Vec<String>,
    pub invalid_evidence_ids: Vec<String>,
}

/// Deterministic rebuild integrity check (ADR-0038 §Rebuild Gate).
#[must_use]
pub fn check_rebuild_integrity(before: &CanonicalState, after: &CanonicalState) -> IntegrityReport {
    let mut report = IntegrityReport {
        pass: true,
        invalid_claim_ids: [before, after]
            .into_iter()
            .flat_map(|state| {
                invalid_context_ids(state.claims.iter().map(|claim| claim.display_key.as_str()))
            })
            .collect(),
        invalid_evidence_ids: [before, after]
            .into_iter()
            .flat_map(|state| {
                invalid_context_ids(state.evidence.iter().map(|record| record.id.as_str()))
            })
            .collect(),
        ..IntegrityReport::default()
    };

    for constraint in &before.hard_constraints {
        if !after.hard_constraints.iter().any(|item| item == constraint) {
            report.missing_constraints.push(constraint.clone());
        }
    }

    let after_claims: BTreeMap<String, &CanonicalClaimFact> = after
        .claims
        .iter()
        .map(|claim| (claim.display_key.clone(), claim))
        .collect();
    let before_claims: BTreeMap<String, &CanonicalClaimFact> = before
        .claims
        .iter()
        .map(|claim| (claim.display_key.clone(), claim))
        .collect();
    let evidence_ids: BTreeSet<String> = after
        .evidence
        .iter()
        .map(|record| record.id.clone())
        .collect();

    for (key, before_claim) in &before_claims {
        match after_claims.get(key) {
            None => {
                if before_claim.state == ClaimState::Confirmed {
                    report.missing_confirmed_claims.push(key.clone());
                }
            }
            Some(after_claim) => {
                // Confirmed must not regress unless new verified counter-evidence.
                if before_claim.state == ClaimState::Confirmed
                    && after_claim.state != ClaimState::Confirmed
                {
                    let has_new_counter = after.evidence.iter().any(|record| {
                        record.direction == EvidenceDirection::Counter
                            && record.verified
                            && after_claim.evidence_ids.contains(&record.id)
                    });
                    if !has_new_counter {
                        report.state_regressions.push(key.clone());
                    }
                }
                for id in &after_claim.evidence_ids {
                    if !evidence_ids.contains(id) {
                        report.dangling_evidence_ids.push(id.clone());
                    }
                }
            }
        }
    }

    for key in after_claims.keys() {
        if !before_claims.contains_key(key) {
            report.hallucinated_claims.push(key.clone());
        }
    }
    for record in &after.evidence {
        if !before.evidence.iter().any(|prior| prior.id == record.id)
            && record.id.starts_with("ev_")
        {
            // New tool evidence is allowed; fabricated ids without provenance are not.
            if record.source_refs.is_empty() {
                report.hallucinated_evidence.push(record.id.clone());
            }
        }
    }

    report.invalid_claim_ids.sort();
    report.invalid_claim_ids.dedup();
    report.invalid_evidence_ids.sort();
    report.invalid_evidence_ids.dedup();
    report.pass = report.invalid_claim_ids.is_empty()
        && report.invalid_evidence_ids.is_empty()
        && report.missing_constraints.is_empty()
        && report.missing_confirmed_claims.is_empty()
        && report.dangling_evidence_ids.is_empty()
        && report.state_regressions.is_empty()
        && report.hallucinated_claims.is_empty()
        && report.hallucinated_evidence.is_empty();
    report
}

fn invalid_context_ids<'a>(ids: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    ids.filter(|id| id.trim().is_empty() || !seen.insert(*id))
        .map(str::to_owned)
        .collect()
}

impl CanonicalClaimFact {
    /// Kept for hosts that classify basis; integrity uses evidence direction instead.
    #[must_use]
    pub fn basis(&self) -> ClaimBasis {
        self.basis
    }
}

/// Rebuild gate: activate only on PASS; else keep previous context.
#[must_use]
pub fn rebuild_gate(
    previous: &ContextSnapshot,
    candidate: &ContextSnapshot,
) -> (ContextSnapshot, IntegrityReport) {
    let mut report = check_rebuild_integrity(&previous.canonical, &candidate.canonical);
    // Apply authoritative Fusion/Verifier changes before taking `previous`.
    // Context rebuilding cannot introduce even an apparently justified update.
    report.canonical_changed = previous.canonical != candidate.canonical;
    report.stale_refuted_in_active =
        stale_refuted_in_active(&candidate.canonical, &candidate.active);
    // Stage goal changes in the authoritative snapshot before rebuilding, just
    // like canonical changes. JEV only selects a representation of that state.
    report.active_goal_changed = previous.active.current_goal != candidate.active.current_goal;
    // Evidence facts are authoritative projections, not compressible raw logs.
    // Apply new facts to `previous` before allowing JEV to rebuild its context.
    report.changed_active_evidence = previous
        .active
        .evidence_facts
        .iter()
        .collect::<BTreeSet<_>>()
        .symmetric_difference(&candidate.active.evidence_facts.iter().collect())
        .map(|fact| (**fact).clone())
        .collect();
    report.missing_active_claims =
        previous
            .active
            .priority_claims
            .iter()
            .filter(|key| {
                !candidate.active.priority_claims.contains(key)
                    && !previous.canonical.claims.iter().any(|claim| {
                        &claim.display_key == *key && claim.state == ClaimState::Refuted
                    })
            })
            .cloned()
            .collect();
    report.unknown_active_claims = candidate
        .active
        .priority_claims
        .iter()
        .filter(|key| {
            !candidate
                .canonical
                .claims
                .iter()
                .any(|claim| &claim.display_key == *key)
        })
        .cloned()
        .collect();
    report.missing_active_constraints = previous
        .canonical
        .hard_constraints
        .iter()
        .chain(&previous.active.constraints)
        .filter(|constraint| !candidate.active.constraints.contains(constraint))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let archive: BTreeMap<_, _> = candidate
        .archive
        .iter()
        .map(|entry| (&entry.id, entry))
        .collect();
    for entries in [&previous.archive, &candidate.archive] {
        let mut ids = BTreeSet::new();
        for entry in entries {
            if entry.id.trim().is_empty() || !ids.insert(&entry.id) {
                report.invalid_archive_ids.push(entry.id.clone());
            }
        }
    }
    for entry in &previous.archive {
        if archive.get(&entry.id).copied() != Some(entry) {
            report.invalid_archive_ids.push(entry.id.clone());
        }
    }
    report.invalid_archive_ids.sort();
    report.invalid_archive_ids.dedup();
    report.pass &= !report.canonical_changed
        && !report.active_goal_changed
        && report.changed_active_evidence.is_empty()
        && report.missing_active_claims.is_empty()
        && report.unknown_active_claims.is_empty()
        && report.missing_active_constraints.is_empty()
        && report.stale_refuted_in_active.is_empty()
        && report.invalid_archive_ids.is_empty();
    if report.pass {
        (candidate.clone(), report)
    } else {
        (previous.clone(), report)
    }
}

// ---------------------------------------------------------------------------
// Stale / hallucination helpers
// ---------------------------------------------------------------------------

/// Refuted claims must not re-enter active priority lists.
#[must_use]
pub fn stale_refuted_in_active(canonical: &CanonicalState, active: &ActiveContext) -> Vec<String> {
    let mut stale = Vec::new();
    for key in &active.priority_claims {
        if let Some(claim) = canonical
            .claims
            .iter()
            .find(|claim| &claim.display_key == key)
            && claim.state == ClaimState::Refuted
        {
            stale.push(key.clone());
        }
    }
    stale
}

// ---------------------------------------------------------------------------
// Joint metrics (Fusion × JEV)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FusionQualityMetrics {
    pub fusion_score: usize,
    pub best_fixed_single: usize,
    pub best_single_oracle: usize,
    pub oracle_union: usize,
    pub total_claims: usize,
    pub fusion_gain: usize,
    pub fusion_regret: usize,
    pub minority_truth_recovery_num: usize,
    pub minority_truth_recovery_den: usize,
    pub false_consensus_recovery: usize,
    pub investigation_gain: usize,
}

impl FusionQualityMetrics {
    /// Ratio for report display only; sub-ULP precision on `usize` counts is
    /// irrelevant here, so the `usize`→`f64` narrowing is deliberate.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn oracle_capture_rate(&self) -> Option<f64> {
        if self.oracle_union == 0 {
            None
        } else {
            Some(self.fusion_score as f64 / self.oracle_union as f64)
        }
    }

    /// Ratio for report display only; see [`Self::oracle_capture_rate`].
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn minority_truth_recovery_rate(&self) -> Option<f64> {
        if self.minority_truth_recovery_den == 0 {
            None
        } else {
            Some(self.minority_truth_recovery_num as f64 / self.minority_truth_recovery_den as f64)
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JevIntegrityMetrics {
    pub constraint_recall_num: usize,
    pub constraint_recall_den: usize,
    pub confirmed_claim_recall_num: usize,
    pub confirmed_claim_recall_den: usize,
    pub verified_evidence_recall_num: usize,
    pub verified_evidence_recall_den: usize,
    pub evidence_binding_errors: usize,
    pub state_regressions: usize,
    pub hallucinated_states: usize,
    pub stale_fact_rate_num: usize,
    pub stale_fact_rate_den: usize,
    pub rebuilds: usize,
    pub integrity_pass: usize,
    pub post_rebuild_success_num: usize,
    pub post_rebuild_success_den: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostMetrics {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub rebuild_tokens: u64,
    pub tool_tokens: u64,
    pub fusion_tokens: u64,
    pub cache_hit_tokens: u64,
    pub cache_miss_tokens: u64,
    pub wall_clock_ms: u64,
}

impl CostMetrics {
    #[must_use]
    pub fn net_token_saving_vs(&self, baseline: &Self) -> i64 {
        let before = baseline.input_tokens + baseline.output_tokens + baseline.rebuild_tokens;
        let after = self.input_tokens + self.output_tokens + self.rebuild_tokens;
        i64::try_from(before).unwrap_or(i64::MAX) - i64::try_from(after).unwrap_or(i64::MAX)
    }
}

/// Four-arm baseline (ADR-0038 §四组基线).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum BenchArm {
    /// Single agent, no JEV.
    A,
    /// Single agent + JEV.
    B,
    /// Fusion, no JEV.
    C,
    /// Fusion + JEV (product).
    D,
}

// ---------------------------------------------------------------------------
// Pinned vs compressible (JEV policy)
// ---------------------------------------------------------------------------

/// Pinned content that rebuild must retain (ADR-0038 §压缩边界).
#[must_use]
pub fn pinned_payload(state: &CanonicalState) -> Vec<String> {
    let mut pinned = vec![state.task_objective.clone()];
    pinned.extend(state.acceptance_criteria.iter().cloned());
    pinned.extend(state.hard_constraints.iter().cloned());
    if let Some(plan) = &state.current_plan {
        pinned.push(plan.clone());
    }
    for claim in &state.claims {
        if matches!(
            claim.state,
            ClaimState::Confirmed | ClaimState::Refuted | ClaimState::Disputed
        ) {
            pinned.push(format!("{}={:?}", claim.display_key, claim.state));
        }
    }
    for record in &state.evidence {
        if record.verified && !record.invalidated {
            pinned.push(format!("{}:{}", record.id, record.facts.join(";")));
        }
    }
    pinned
}

/// High-risk claims stay in Active until resolved.
#[must_use]
pub fn is_pinned_claim(display_key: &str) -> bool {
    let hay = display_key.to_ascii_lowercase();
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
    ]
    .iter()
    .any(|needle| hay.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(id: &str) -> FusionEvidenceRecord {
        FusionEvidenceRecord {
            id: id.to_owned(),
            claim_id: "c1".to_owned(),
            provider: "codegraph".to_owned(),
            direction: EvidenceDirection::Support,
            kind: "trace_ownership".to_owned(),
            strength: EvidenceStrength::Direct,
            facts: vec!["fact".to_owned()],
            source_refs: vec!["src".to_owned()],
            independence_group: "ig".to_owned(),
            verified: true,
            invalidated: false,
        }
    }

    #[test]
    fn rebuild_rejects_ambiguous_or_blank_canonical_ids_even_when_unchanged() {
        let claim = CanonicalClaimFact {
            display_key: "claim-1".into(),
            state: ClaimState::Supported,
            evidence_ids: vec![],
            basis: ClaimBasis::ToolEvidence,
        };
        let evidence = FusionEvidenceRecord {
            id: "evidence-1".into(),
            claim_id: "claim-1".into(),
            provider: "tool".into(),
            direction: EvidenceDirection::Support,
            kind: "command".into(),
            strength: EvidenceStrength::Direct,
            facts: vec![],
            source_refs: vec![],
            independence_group: "group-1".into(),
            verified: false,
            invalidated: false,
        };
        for blank in [false, true] {
            let mut canonical = base_state();
            canonical.claims = vec![claim.clone(), claim.clone()];
            canonical.evidence = vec![evidence.clone(), evidence.clone()];
            if blank {
                canonical.claims = vec![CanonicalClaimFact {
                    display_key: " ".into(),
                    ..claim.clone()
                }];
                canonical.evidence = vec![FusionEvidenceRecord {
                    id: String::new(),
                    ..evidence.clone()
                }];
            }
            let snapshot = ContextSnapshot {
                label: "ambiguous".into(),
                canonical,
                active: ActiveContext::default(),
                archive: vec![],
            };
            let (retained, report) = rebuild_gate(&snapshot, &snapshot);
            assert_eq!(retained, snapshot);
            assert!(!report.pass);
            assert_eq!(
                report.invalid_claim_ids,
                vec![if blank { " " } else { "claim-1" }]
            );
            assert_eq!(
                report.invalid_evidence_ids,
                vec![if blank { "" } else { "evidence-1" }]
            );
        }
    }

    fn base_state() -> CanonicalState {
        CanonicalState {
            task_objective: "fix flake".to_owned(),
            acceptance_criteria: vec!["tests green".to_owned()],
            hard_constraints: vec!["do not change API".to_owned()],
            claims: vec![CanonicalClaimFact {
                display_key: "defect:shared-map-race".to_owned(),
                state: ClaimState::Confirmed,
                evidence_ids: vec!["ev1".to_owned()],
                basis: ClaimBasis::Verifier,
            }],
            evidence: vec![evidence("ev1")],
            current_plan: Some("investigate race".to_owned()),
        }
    }

    #[test]
    fn rebuild_gate_rejects_missing_confirmed_claim() {
        let before = base_state();
        let mut after = base_state();
        after.claims.clear();
        let report = check_rebuild_integrity(&before, &after);
        assert!(!report.pass);
        assert!(
            report
                .missing_confirmed_claims
                .iter()
                .any(|key| key == "defect:shared-map-race")
        );
        let previous = ContextSnapshot {
            label: "before".to_owned(),
            canonical: before.clone(),
            active: ActiveContext::default(),
            archive: Vec::new(),
        };
        let candidate = ContextSnapshot {
            label: "after".to_owned(),
            canonical: after,
            active: ActiveContext::default(),
            archive: Vec::new(),
        };
        let (kept, _) = rebuild_gate(&previous, &candidate);
        assert_eq!(kept.label, "before");
    }

    #[test]
    fn rebuild_gate_rejects_confirmed_regression() {
        let before = base_state();
        let mut after = base_state();
        after.claims[0].state = ClaimState::Supported;
        let report = check_rebuild_integrity(&before, &after);
        assert!(
            report
                .state_regressions
                .iter()
                .any(|k| k.contains("shared-map"))
        );
    }

    #[test]
    fn rebuild_gate_accepts_clean_rebuild() {
        let before = base_state();
        let after = base_state();
        let report = check_rebuild_integrity(&before, &after);
        assert!(report.pass);
    }

    #[test]
    fn stale_refuted_not_in_active() {
        let mut state = base_state();
        state.claims.push(CanonicalClaimFact {
            display_key: "root:network-flake".to_owned(),
            state: ClaimState::Refuted,
            evidence_ids: Vec::new(),
            basis: ClaimBasis::Verifier,
        });
        let active = ActiveContext {
            priority_claims: vec!["root:network-flake".to_owned()],
            ..ActiveContext::default()
        };
        assert_eq!(
            stale_refuted_in_active(&state, &active),
            vec!["root:network-flake".to_owned()]
        );
    }

    #[test]
    fn rebuild_preserves_canonical_truth_and_rejects_reactivated_refutations() {
        let mut previous = ContextSnapshot {
            label: "before".to_owned(),
            canonical: base_state(),
            active: ActiveContext::default(),
            archive: Vec::new(),
        };
        previous.active.constraints = previous.canonical.hard_constraints.clone();
        previous.canonical.claims[0].state = ClaimState::Refuted;
        let mut candidate = previous.clone();
        candidate.label = "rebuilt".to_owned();
        candidate.active.recent_observations = vec!["compressed history".to_owned()];
        assert_eq!(
            rebuild_gate(&previous, &candidate),
            (
                candidate.clone(),
                IntegrityReport {
                    pass: true,
                    ..IntegrityReport::default()
                }
            )
        );

        let mutations: [fn(&mut CanonicalState); 6] = [
            |state| state.task_objective.clear(),
            |state| state.acceptance_criteria.clear(),
            |state| state.current_plan = None,
            |state| state.claims[0].state = ClaimState::Confirmed,
            |state| state.evidence[0].facts = vec!["altered fact".to_owned()],
            |state| state.evidence[0].source_refs.clear(),
        ];
        for mutate in mutations {
            let mut altered = candidate.clone();
            mutate(&mut altered.canonical);
            let (kept, report) = rebuild_gate(&previous, &altered);
            assert!(!report.pass, "rebuild changed canonical truth");
            assert!(report.canonical_changed);
            assert_eq!(kept, previous);
        }
        candidate.active.priority_claims = vec![previous.canonical.claims[0].display_key.clone()];
        let (kept, report) = rebuild_gate(&previous, &candidate);
        assert!(!report.pass);
        assert_eq!(
            report.stale_refuted_in_active,
            candidate.active.priority_claims
        );
        assert_eq!(kept, previous);
    }

    #[test]
    fn rebuild_cannot_drop_active_goal_or_hard_constraints() {
        let previous = ContextSnapshot {
            label: "before".to_owned(),
            canonical: base_state(),
            active: ActiveContext {
                current_goal: "investigate the shared map".to_owned(),
                constraints: vec!["do not change API".to_owned()],
                ..ActiveContext::default()
            },
            archive: Vec::new(),
        };
        for change in [
            ("", vec!["do not change API".to_owned()]),
            ("implement a new API", vec!["do not change API".to_owned()]),
            ("investigate the shared map", Vec::new()),
        ] {
            let mut candidate = previous.clone();
            candidate.active.current_goal = change.0.to_owned();
            candidate.active.constraints = change.1;
            let (kept, report) = rebuild_gate(&previous, &candidate);
            assert!(!report.pass, "lost active task requirements were accepted");
            assert_eq!(
                report.active_goal_changed,
                candidate.active.current_goal != previous.active.current_goal
            );
            assert_eq!(
                report.missing_active_constraints.is_empty(),
                !candidate.active.constraints.is_empty()
            );
            assert_eq!(kept, previous);
        }
        let mut candidate = previous.clone();
        candidate.active.recent_observations = vec!["compacted log".to_owned()];
        assert!(rebuild_gate(&previous, &candidate).1.pass);
    }

    #[test]
    fn rebuild_preserves_active_claims_and_evidence() {
        let previous = ContextSnapshot {
            canonical: base_state(),
            active: ActiveContext {
                constraints: vec!["do not change API".to_owned()],
                priority_claims: vec!["defect:shared-map-race".to_owned()],
                evidence_facts: vec!["ev1:fact; source=src".to_owned()],
                ..ActiveContext::default()
            },
            label: "before".to_owned(),
            archive: Vec::new(),
        };
        let mutations: [fn(&mut ActiveContext); 5] = [
            |active| active.priority_claims.clear(),
            |active| active.evidence_facts.clear(),
            |active| active.evidence_facts[0] = "ev1:altered fact".to_owned(),
            |active| active.evidence_facts.push("ev2:fabricated fact".to_owned()),
            |active| active.priority_claims.push("fabricated claim".to_owned()),
        ];
        for mutate in mutations {
            let mut candidate = previous.clone();
            mutate(&mut candidate.active);
            let (kept, report) = rebuild_gate(&previous, &candidate);
            assert!(
                !report.pass,
                "rebuild altered model-visible claims or evidence"
            );
            assert_eq!(kept, previous);
        }
        let mut candidate = previous.clone();
        candidate.active.recent_observations = vec!["compacted log".to_owned()];
        assert!(rebuild_gate(&previous, &candidate).1.pass);

        let mut authoritative = previous;
        authoritative.canonical.claims[0].state = ClaimState::Refuted;
        let mut candidate = authoritative.clone();
        candidate.active.priority_claims.clear();
        assert!(rebuild_gate(&authoritative, &candidate).1.pass);
    }

    #[test]
    fn rebuild_preserves_archive_entries_and_unique_identities() {
        let previous = ContextSnapshot {
            label: "before".to_owned(),
            canonical: CanonicalState::default(),
            active: ActiveContext::default(),
            archive: vec![ArchiveEntry {
                id: "archive-1".to_owned(),
                kind: "tool-output".to_owned(),
                body: "original evidence".to_owned(),
            }],
        };
        let mutations: [fn(&mut Vec<ArchiveEntry>); 6] = [
            Vec::clear,
            |entries| entries[0].body = "altered evidence".to_owned(),
            |entries| entries[0].kind = "reasoning".to_owned(),
            |entries| entries[0].id = "renamed".to_owned(),
            |entries| entries.push(entries[0].clone()),
            |entries| entries[0].id = " ".to_owned(),
        ];
        for mutate in mutations {
            let mut candidate = previous.clone();
            mutate(&mut candidate.archive);
            let (kept, report) = rebuild_gate(&previous, &candidate);
            assert!(!report.pass);
            assert!(!report.invalid_archive_ids.is_empty());
            assert_eq!(kept, previous);
        }
        let mut candidate = previous.clone();
        candidate.archive.insert(
            0,
            ArchiveEntry {
                id: "archive-2".to_owned(),
                kind: "reasoning".to_owned(),
                body: "new archived observation".to_owned(),
            },
        );
        assert!(rebuild_gate(&previous, &candidate).1.pass);
        let duplicate = candidate.archive[0].clone();
        candidate.archive.push(duplicate);
        assert!(!rebuild_gate(&previous, &candidate).1.pass);
        assert!(!rebuild_gate(&candidate, &candidate).1.pass);
    }

    #[test]
    fn metrics_oracle_capture_and_pin_policy() {
        let metrics = FusionQualityMetrics {
            fusion_score: 5,
            oracle_union: 6,
            fusion_gain: 2,
            fusion_regret: 0,
            minority_truth_recovery_num: 2,
            minority_truth_recovery_den: 2,
            ..FusionQualityMetrics::default()
        };
        assert!((metrics.oracle_capture_rate().unwrap() - 5.0 / 6.0).abs() < 1e-9);
        assert!(is_pinned_claim("defect:shared-map-race"));
        assert!(!is_pinned_claim("style:only-style-issues"));
        let pinned = pinned_payload(&base_state());
        assert!(pinned.iter().any(|line| line.contains("do not change API")));
    }
}
