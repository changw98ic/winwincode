// SPDX-License-Identifier: Apache-2.0

//! Deterministic policy reduction for Jev context and memory decisions.
//!
//! The module consumes provider-neutral NLI probabilities. Model adapters stay
//! outside this crate; only system policy may turn probabilities into actions.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Provider-neutral entailment result for one hypothesis.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NliProbabilities {
    pub entailment: f64,
    pub contradiction: f64,
    pub neutral: f64,
}

/// H1-H4 scores used by context retention policy.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextHypotheses {
    /// H1: required system fact or task constraint.
    pub critical: NliProbabilities,
    /// H2: useful for the current task.
    pub relevant: NliProbabilities,
    /// H3: useful in compact or archived form.
    pub compressible: NliProbabilities,
    /// H4: safe to discard as noise or superseded output.
    pub disposable: NliProbabilities,
}

/// System-owned decision thresholds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JevPolicy {
    pub version: String,
    pub minimum_confidence: f64,
    pub pin_threshold: f64,
    pub keep_threshold: f64,
    pub compact_threshold: f64,
    pub drop_threshold: f64,
    pub task_memory_threshold: f64,
    pub project_memory_threshold: f64,
    pub long_term_memory_threshold: f64,
}

/// Context item facts owned by the runtime, not the NLI provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextDecisionInput {
    pub provider: String,
    pub model: String,
    pub protected: bool,
    pub archive_eligible: bool,
    pub hypotheses: ContextHypotheses,
}

/// Closed context-retention action set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ContextRetention {
    Pin,
    Keep,
    Truncate,
    Drop,
    Archive,
}

/// Closed memory-write action set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MemoryWrite {
    Ignore,
    TaskMemory,
    ProjectMemory,
    LongTermMemory,
}

/// Stable audit reasons emitted by the deterministic reducer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionReason {
    ProtectedBySystem,
    CriticalEntailment,
    RelevantEntailment,
    LowConfidenceKeep,
    Compactable,
    ArchiveEligible,
    DisposableEntailment,
    ConservativeKeep,
    Duplicate,
    LowValueToolOutput,
    ImportanceBelowThreshold,
    TaskImportance,
    ProjectImportance,
    LongTermImportance,
}

/// Auditable context decision. Provider and model are observations only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextDecision {
    pub decision: ContextRetention,
    pub confidence: f64,
    pub provider: String,
    pub model: String,
    pub policy_version: String,
    pub reason_codes: Vec<DecisionReason>,
}

/// System-computed inputs for memory importance.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportanceSignals {
    pub relevance: f64,
    pub durability: f64,
    pub specificity: f64,
    pub confidence: f64,
}

impl ImportanceSignals {
    fn score(self) -> f64 {
        (self.relevance + self.durability + self.specificity + self.confidence) / 4.0
    }
}

/// Runtime facts considered before writing memory.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryDecisionInput {
    pub duplicate: bool,
    pub low_value_tool_output: bool,
    pub importance: ImportanceSignals,
}

/// Auditable memory-write decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryDecision {
    pub decision: MemoryWrite,
    pub confidence: f64,
    pub policy_version: String,
    pub reason_codes: Vec<DecisionReason>,
}

/// Relationship retained on an appended memory record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MemoryRelationKind {
    Supersedes,
    Contradicts,
    Complements,
}

/// Persistent lifecycle state. Supersession points forward and keeps history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "status")]
pub enum MemoryStatus {
    Active,
    Superseded { by: String },
}

/// Minimal provider-independent memory state used by the pure reducer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryRecord {
    pub id: String,
    pub status: MemoryStatus,
    pub relation: Option<MemoryRelation>,
}

/// Link from a new record to retained history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryRelation {
    pub kind: MemoryRelationKind,
    pub memory_id: String,
}

/// Candidate used for deterministic retrieval reranking.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MemoryRankCandidate {
    pub id: String,
    pub importance: ImportanceSignals,
}

/// Invalid provider output, policy, or memory state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JevDecisionError {
    InvalidProbability,
    InvalidPolicy,
    InvalidMetadata,
    InvalidMemoryState,
}

impl fmt::Display for JevDecisionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidProbability => "Jev probability is invalid",
            Self::InvalidPolicy => "Jev policy is invalid",
            Self::InvalidMetadata => "Jev decision metadata is invalid",
            Self::InvalidMemoryState => "Jev memory state is invalid",
        })
    }
}

impl std::error::Error for JevDecisionError {}

/// Maps H1-H4 probabilities to one context action. Low confidence fails open
/// to `KEEP`, and a system-protected item can never reach `DROP`.
///
/// # Errors
///
/// Rejects malformed probabilities, policy thresholds, or provider metadata.
pub fn decide_context(
    input: &ContextDecisionInput,
    policy: &JevPolicy,
) -> Result<ContextDecision, JevDecisionError> {
    validate_policy(policy)?;
    validate_metadata(&input.provider)?;
    validate_metadata(&input.model)?;
    for probabilities in [
        input.hypotheses.critical,
        input.hypotheses.relevant,
        input.hypotheses.compressible,
        input.hypotheses.disposable,
    ] {
        validate_probabilities(probabilities)?;
    }

    let scores = [
        input.hypotheses.critical.entailment,
        input.hypotheses.relevant.entailment,
        input.hypotheses.compressible.entailment,
        input.hypotheses.disposable.entailment,
    ];
    let maximum = scores.into_iter().fold(0.0, f64::max);
    let (decision, confidence, reason_codes) = if input.protected {
        (
            ContextRetention::Pin,
            1.0,
            vec![DecisionReason::ProtectedBySystem],
        )
    } else if scores[0] >= policy.pin_threshold {
        (
            ContextRetention::Pin,
            scores[0],
            vec![DecisionReason::CriticalEntailment],
        )
    } else if scores[1] >= policy.keep_threshold {
        (
            ContextRetention::Keep,
            scores[1],
            vec![DecisionReason::RelevantEntailment],
        )
    } else if maximum < policy.minimum_confidence {
        (
            ContextRetention::Keep,
            maximum,
            vec![DecisionReason::LowConfidenceKeep],
        )
    } else if scores[2] >= policy.compact_threshold {
        if input.archive_eligible {
            (
                ContextRetention::Archive,
                scores[2],
                vec![DecisionReason::Compactable, DecisionReason::ArchiveEligible],
            )
        } else {
            (
                ContextRetention::Truncate,
                scores[2],
                vec![DecisionReason::Compactable],
            )
        }
    } else if scores[3] >= policy.drop_threshold {
        (
            ContextRetention::Drop,
            scores[3],
            vec![DecisionReason::DisposableEntailment],
        )
    } else {
        (
            ContextRetention::Keep,
            maximum,
            vec![DecisionReason::ConservativeKeep],
        )
    };

    Ok(ContextDecision {
        decision,
        confidence,
        provider: input.provider.clone(),
        model: input.model.clone(),
        policy_version: policy.version.clone(),
        reason_codes,
    })
}

/// Maps system-computed importance to one memory scope.
///
/// # Errors
///
/// Rejects malformed importance signals or policy thresholds.
pub fn decide_memory(
    input: MemoryDecisionInput,
    policy: &JevPolicy,
) -> Result<MemoryDecision, JevDecisionError> {
    validate_policy(policy)?;
    validate_importance(input.importance)?;
    let score = input.importance.score();
    let (decision, reason) = if input.duplicate {
        (MemoryWrite::Ignore, DecisionReason::Duplicate)
    } else if input.low_value_tool_output {
        (MemoryWrite::Ignore, DecisionReason::LowValueToolOutput)
    } else if score >= policy.long_term_memory_threshold {
        (
            MemoryWrite::LongTermMemory,
            DecisionReason::LongTermImportance,
        )
    } else if score >= policy.project_memory_threshold {
        (
            MemoryWrite::ProjectMemory,
            DecisionReason::ProjectImportance,
        )
    } else if score >= policy.task_memory_threshold {
        (MemoryWrite::TaskMemory, DecisionReason::TaskImportance)
    } else {
        (
            MemoryWrite::Ignore,
            DecisionReason::ImportanceBelowThreshold,
        )
    };
    Ok(MemoryDecision {
        decision,
        confidence: score,
        policy_version: policy.version.clone(),
        reason_codes: vec![reason],
    })
}

/// Appends a related memory and marks a superseded record with a forward
/// pointer. Contradictions and complements append without rewriting history.
///
/// # Errors
///
/// Rejects empty, duplicate, or missing identities.
pub fn relate_memory(
    existing: &[MemoryRecord],
    new_id: &str,
    related_id: &str,
    kind: MemoryRelationKind,
) -> Result<Vec<MemoryRecord>, JevDecisionError> {
    validate_metadata(new_id)?;
    validate_metadata(related_id)?;
    if new_id == related_id
        || existing.iter().any(|record| record.id == new_id)
        || !existing.iter().any(|record| record.id == related_id)
    {
        return Err(JevDecisionError::InvalidMemoryState);
    }
    let mut records = existing.to_vec();
    if kind == MemoryRelationKind::Supersedes {
        let related = records
            .iter_mut()
            .find(|record| record.id == related_id)
            .ok_or(JevDecisionError::InvalidMemoryState)?;
        related.status = MemoryStatus::Superseded {
            by: new_id.to_owned(),
        };
    }
    records.push(MemoryRecord {
        id: new_id.to_owned(),
        status: MemoryStatus::Active,
        relation: Some(MemoryRelation {
            kind,
            memory_id: related_id.to_owned(),
        }),
    });
    Ok(records)
}

/// Reranks retrieved memory by importance and returns a bounded Top 3-10.
/// Invalid candidates are omitted because retrieval is fail-open advisory data.
#[must_use]
pub fn rerank_memory(
    candidates: &[MemoryRankCandidate],
    requested: usize,
) -> Vec<MemoryRankCandidate> {
    let mut ranked = candidates
        .iter()
        .filter(|candidate| {
            validate_metadata(&candidate.id).is_ok()
                && validate_importance(candidate.importance).is_ok()
        })
        .cloned()
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .importance
            .score()
            .total_cmp(&left.importance.score())
            .then_with(|| left.id.cmp(&right.id))
    });
    ranked.truncate(requested.clamp(3, 10));
    ranked
}

fn validate_probabilities(value: NliProbabilities) -> Result<(), JevDecisionError> {
    let values = [value.entailment, value.contradiction, value.neutral];
    let sum = values.into_iter().sum::<f64>();
    if values.into_iter().all(valid_score) && (sum - 1.0).abs() <= 0.02 {
        Ok(())
    } else {
        Err(JevDecisionError::InvalidProbability)
    }
}

fn validate_importance(value: ImportanceSignals) -> Result<(), JevDecisionError> {
    if [
        value.relevance,
        value.durability,
        value.specificity,
        value.confidence,
    ]
    .into_iter()
    .all(valid_score)
    {
        Ok(())
    } else {
        Err(JevDecisionError::InvalidProbability)
    }
}

fn validate_policy(policy: &JevPolicy) -> Result<(), JevDecisionError> {
    let thresholds = [
        policy.minimum_confidence,
        policy.pin_threshold,
        policy.keep_threshold,
        policy.compact_threshold,
        policy.drop_threshold,
        policy.task_memory_threshold,
        policy.project_memory_threshold,
        policy.long_term_memory_threshold,
    ];
    if policy.version.trim().is_empty()
        || !thresholds.into_iter().all(valid_score)
        || policy.minimum_confidence > policy.pin_threshold
        || policy.minimum_confidence > policy.keep_threshold
        || policy.minimum_confidence > policy.compact_threshold
        || policy.minimum_confidence > policy.drop_threshold
        || policy.task_memory_threshold > policy.project_memory_threshold
        || policy.project_memory_threshold > policy.long_term_memory_threshold
    {
        Err(JevDecisionError::InvalidPolicy)
    } else {
        Ok(())
    }
}

fn validate_metadata(value: &str) -> Result<(), JevDecisionError> {
    if value.trim().is_empty() || value.len() > 256 || value.contains(['\0', '\n', '\r']) {
        Err(JevDecisionError::InvalidMetadata)
    } else {
        Ok(())
    }
}

fn valid_score(value: f64) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> JevPolicy {
        JevPolicy {
            version: "jev-policy.v1".to_owned(),
            minimum_confidence: 0.55,
            pin_threshold: 0.90,
            keep_threshold: 0.75,
            compact_threshold: 0.75,
            drop_threshold: 0.90,
            task_memory_threshold: 0.40,
            project_memory_threshold: 0.60,
            long_term_memory_threshold: 0.80,
        }
    }

    fn nli(entailment: f64) -> NliProbabilities {
        NliProbabilities {
            entailment,
            contradiction: 1.0 - entailment,
            neutral: 0.0,
        }
    }

    fn context(scores: [f64; 4], archive_eligible: bool) -> ContextDecisionInput {
        ContextDecisionInput {
            provider: "mock".to_owned(),
            model: "fixture-nli".to_owned(),
            protected: false,
            archive_eligible,
            hypotheses: ContextHypotheses {
                critical: nli(scores[0]),
                relevant: nli(scores[1]),
                compressible: nli(scores[2]),
                disposable: nli(scores[3]),
            },
        }
    }

    fn importance(score: f64) -> ImportanceSignals {
        ImportanceSignals {
            relevance: score,
            durability: score,
            specificity: score,
            confidence: score,
        }
    }

    #[test]
    fn context_policy_covers_five_actions_and_audits_output() {
        for (input, expected) in [
            (
                context([0.95, 0.01, 0.01, 0.03], false),
                ContextRetention::Pin,
            ),
            (
                context([0.01, 0.95, 0.01, 0.03], false),
                ContextRetention::Keep,
            ),
            (
                context([0.01, 0.01, 0.95, 0.03], false),
                ContextRetention::Truncate,
            ),
            (
                context([0.01, 0.01, 0.03, 0.95], false),
                ContextRetention::Drop,
            ),
            (
                context([0.01, 0.01, 0.95, 0.03], true),
                ContextRetention::Archive,
            ),
        ] {
            let decision = decide_context(&input, &policy()).expect("valid context decision");
            assert_eq!(decision.decision, expected);
            assert!(decision.confidence > 0.0);
            assert_eq!(decision.policy_version, "jev-policy.v1");
            assert!(!decision.reason_codes.is_empty());
            assert_eq!(decision.provider, "mock");
            assert_eq!(decision.model, "fixture-nli");
            let json = serde_json::to_value(&decision).expect("decision JSON");
            for field in [
                "decision",
                "confidence",
                "provider",
                "model",
                "policyVersion",
                "reasonCodes",
            ] {
                assert!(json.get(field).is_some(), "missing JSON field {field}");
            }
        }
    }

    #[test]
    fn low_confidence_and_critical_fixtures_never_drop() {
        assert_eq!(
            decide_context(&context([0.25; 4], false), &policy())
                .expect("low-confidence decision")
                .decision,
            ContextRetention::Keep
        );
        let retained = (0..100)
            .filter(|index| {
                let mut input = context([0.01, 0.01, 0.01, 0.97], false);
                input.protected = true;
                input.hypotheses.critical = nli(f64::from(*index) / 100.0);
                decide_context(&input, &policy())
                    .expect("critical fixture decision")
                    .decision
                    != ContextRetention::Drop
            })
            .count();
        assert!(retained >= 99, "critical recall was {retained}%");
    }

    #[test]
    fn memory_policy_covers_four_actions_and_ignores_noise() {
        for (score, expected) in [
            (0.20, MemoryWrite::Ignore),
            (0.50, MemoryWrite::TaskMemory),
            (0.70, MemoryWrite::ProjectMemory),
            (0.90, MemoryWrite::LongTermMemory),
        ] {
            let decision = decide_memory(
                MemoryDecisionInput {
                    duplicate: false,
                    low_value_tool_output: false,
                    importance: importance(score),
                },
                &policy(),
            )
            .expect("valid memory decision");
            assert_eq!(decision.decision, expected);
            assert_eq!(decision.policy_version, "jev-policy.v1");
            assert!(!decision.reason_codes.is_empty());
        }
        let ignored = decide_memory(
            MemoryDecisionInput {
                duplicate: true,
                low_value_tool_output: true,
                importance: importance(1.0),
            },
            &policy(),
        )
        .expect("noise decision");
        assert_eq!(ignored.decision, MemoryWrite::Ignore);
    }

    #[test]
    fn supersede_keeps_history_and_rerank_reduces_top_twenty() {
        let records = relate_memory(
            &[MemoryRecord {
                id: "old".to_owned(),
                status: MemoryStatus::Active,
                relation: None,
            }],
            "new",
            "old",
            MemoryRelationKind::Supersedes,
        )
        .expect("supersede plan");
        assert_eq!(records.len(), 2);
        assert_eq!(
            records[0].status,
            MemoryStatus::Superseded {
                by: "new".to_owned()
            }
        );
        assert_eq!(records[1].status, MemoryStatus::Active);

        let candidates = (0..20)
            .map(|index| MemoryRankCandidate {
                id: format!("memory-{index:02}"),
                importance: importance(f64::from(index) / 20.0),
            })
            .collect::<Vec<_>>();
        let reranked = rerank_memory(&candidates, 3);
        assert_eq!(reranked.len(), 3);
        assert_eq!(reranked[0].id, "memory-19");
        assert_eq!(reranked[2].id, "memory-17");
    }
}
