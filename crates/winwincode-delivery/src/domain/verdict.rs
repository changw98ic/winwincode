// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use winwincode_api::generated::{DeliveryId, EvidenceId};

use super::{
    AcceptanceCriterionId, CriterionResultId, DeliverySpecId, DeliveryValidationError,
    DeliveryValidationErrorCode, DeliveryVerdictId, MAX_REFERENCE_LENGTH, MAX_TEXT_LENGTH,
    bounded_text, collection_length, duplicate_ids, portable_identifier, safe_non_negative,
    schema_version, unique_texts, validation_error,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CriterionVerdict {
    #[serde(rename = "pass")]
    Pass,
    #[serde(rename = "fail")]
    Fail,
    #[serde(rename = "inconclusive")]
    Inconclusive,
    #[serde(rename = "infra_error")]
    InfraError,
}

pub type DeliveryVerdictStatus = CriterionVerdict;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CriterionResult {
    pub schema_version: u8,
    pub id: CriterionResultId,
    pub delivery_id: DeliveryId,
    pub delivery_spec_id: DeliverySpecId,
    pub criterion_id: AcceptanceCriterionId,
    pub candidate_ref: String,
    pub verdict: CriterionVerdict,
    pub evidence_refs: Vec<EvidenceId>,
    pub explanation: String,
    pub evaluated_at_millis: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeliveryVerdict {
    pub schema_version: u8,
    pub id: DeliveryVerdictId,
    pub delivery_id: DeliveryId,
    pub delivery_spec_id: DeliverySpecId,
    pub candidate_ref: String,
    pub status: DeliveryVerdictStatus,
    pub criteria: Vec<CriterionResult>,
    pub unresolved_findings: Vec<String>,
    pub produced_at_millis: u64,
}

pub(crate) fn validate(
    verdict: &DeliveryVerdict,
    path: &str,
) -> Result<(), DeliveryValidationError> {
    schema_version(verdict.schema_version, &format!("{path}.schemaVersion"))?;
    portable_identifier(&verdict.id.0, &format!("{path}.id"))?;
    portable_identifier(&verdict.delivery_id.0, &format!("{path}.deliveryId"))?;
    portable_identifier(
        &verdict.delivery_spec_id.0,
        &format!("{path}.deliverySpecId"),
    )?;
    bounded_text(
        &verdict.candidate_ref,
        &format!("{path}.candidateRef"),
        MAX_REFERENCE_LENGTH,
    )?;
    collection_length(verdict.criteria.len(), &format!("{path}.criteria"))?;
    if verdict.criteria.is_empty() {
        return Err(validation_error(
            DeliveryValidationErrorCode::InvalidVerdict,
            format!("{path}.criteria"),
            "delivery verdict must evaluate criteria",
        ));
    }
    for (index, result) in verdict.criteria.iter().enumerate() {
        validate_result(result, &format!("{path}.criteria[{index}]"))?;
    }
    duplicate_ids(
        verdict.criteria.iter().map(|result| result.id.0.as_str()),
        &format!("{path}.criteria"),
    )?;
    duplicate_ids(
        verdict
            .criteria
            .iter()
            .map(|result| result.criterion_id.0.as_str()),
        &format!("{path}.criteria"),
    )?;
    unique_texts(
        &verdict.unresolved_findings,
        &format!("{path}.unresolvedFindings"),
    )?;
    safe_non_negative(
        verdict.produced_at_millis,
        &format!("{path}.producedAtMillis"),
    )
}

fn validate_result(result: &CriterionResult, path: &str) -> Result<(), DeliveryValidationError> {
    schema_version(result.schema_version, &format!("{path}.schemaVersion"))?;
    portable_identifier(&result.id.0, &format!("{path}.id"))?;
    portable_identifier(&result.delivery_id.0, &format!("{path}.deliveryId"))?;
    portable_identifier(
        &result.delivery_spec_id.0,
        &format!("{path}.deliverySpecId"),
    )?;
    portable_identifier(&result.criterion_id.0, &format!("{path}.criterionId"))?;
    bounded_text(
        &result.candidate_ref,
        &format!("{path}.candidateRef"),
        MAX_REFERENCE_LENGTH,
    )?;
    collection_length(result.evidence_refs.len(), &format!("{path}.evidenceRefs"))?;
    for (index, evidence_id) in result.evidence_refs.iter().enumerate() {
        portable_identifier(&evidence_id.0, &format!("{path}.evidenceRefs[{index}]"))?;
    }
    duplicate_ids(
        result
            .evidence_refs
            .iter()
            .map(|evidence_id| evidence_id.0.as_str()),
        &format!("{path}.evidenceRefs"),
    )?;
    if matches!(
        result.verdict,
        CriterionVerdict::Pass | CriterionVerdict::Fail
    ) && result.evidence_refs.is_empty()
    {
        return Err(validation_error(
            DeliveryValidationErrorCode::InvalidVerdict,
            format!("{path}.evidenceRefs"),
            "pass or fail criterion result must cite evidence",
        ));
    }
    bounded_text(
        &result.explanation,
        &format!("{path}.explanation"),
        MAX_TEXT_LENGTH,
    )?;
    safe_non_negative(
        result.evaluated_at_millis,
        &format!("{path}.evaluatedAtMillis"),
    )
}

#[cfg(test)]
mod tests {
    use crate::domain::{
        CriterionVerdict, Delivery, DeliveryStatus, DeliveryTaskStatus, test_fixture,
    };

    #[test]
    fn ready_or_delivered_requires_passing_verdict() {
        let mut fixture = test_fixture();
        fixture.verdict.as_mut().expect("verdict").status = CriterionVerdict::Fail;
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn ready_or_delivered_requires_completed_tasks() {
        let mut fixture = test_fixture();
        fixture.tasks[0].status = DeliveryTaskStatus::Active;
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn pass_or_fail_criterion_requires_evidence() {
        let mut fixture = test_fixture();
        fixture.verdict.as_mut().expect("verdict").criteria[0]
            .evidence_refs
            .clear();
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn delivery_verdict_covers_every_current_criterion_exactly_once() {
        let mut fixture = test_fixture();
        fixture.verdict.as_mut().expect("verdict").criteria.pop();
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn delivery_verdict_status_folds_required_results_and_findings() {
        let mut fixture = test_fixture();
        fixture.status = DeliveryStatus::Verifying;
        let verdict = fixture.verdict.as_mut().expect("verdict");
        verdict
            .unresolved_findings
            .push("Review is incomplete".into());
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }
}
