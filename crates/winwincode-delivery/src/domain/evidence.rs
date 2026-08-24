// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use winwincode_api::generated::{DeliveryId, EvidenceId, StageRunId};

use super::{
    DeliverySpecId, DeliveryValidationError, MAX_REFERENCE_LENGTH, SessionBindingId, bounded_text,
    portable_identifier, positive, safe_non_negative, schema_version,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvidenceRefType {
    #[serde(rename = "test")]
    Test,
    #[serde(rename = "command")]
    Command,
    #[serde(rename = "diff")]
    Diff,
    #[serde(rename = "file")]
    File,
    #[serde(rename = "commit")]
    Commit,
    #[serde(rename = "pull_request")]
    PullRequest,
    #[serde(rename = "runtime_event")]
    RuntimeEvent,
    #[serde(rename = "review_finding")]
    ReviewFinding,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvidenceRef {
    pub schema_version: u8,
    pub id: EvidenceId,
    pub delivery_id: DeliveryId,
    pub delivery_spec_id: DeliverySpecId,
    pub delivery_spec_revision: u64,
    pub stage_run_id: StageRunId,
    pub session_binding_id: SessionBindingId,
    pub candidate_ref: String,
    #[serde(rename = "type")]
    pub evidence_type: EvidenceRefType,
    pub source_ref: String,
    pub created_at_millis: u64,
}

pub(crate) fn validate(evidence: &EvidenceRef, path: &str) -> Result<(), DeliveryValidationError> {
    schema_version(evidence.schema_version, &format!("{path}.schemaVersion"))?;
    portable_identifier(&evidence.id.0, &format!("{path}.id"))?;
    portable_identifier(&evidence.delivery_id.0, &format!("{path}.deliveryId"))?;
    portable_identifier(
        &evidence.delivery_spec_id.0,
        &format!("{path}.deliverySpecId"),
    )?;
    positive(
        evidence.delivery_spec_revision,
        &format!("{path}.deliverySpecRevision"),
    )?;
    portable_identifier(&evidence.stage_run_id.0, &format!("{path}.stageRunId"))?;
    portable_identifier(
        &evidence.session_binding_id.0,
        &format!("{path}.sessionBindingId"),
    )?;
    bounded_text(
        &evidence.candidate_ref,
        &format!("{path}.candidateRef"),
        MAX_REFERENCE_LENGTH,
    )?;
    bounded_text(
        &evidence.source_ref,
        &format!("{path}.sourceRef"),
        MAX_REFERENCE_LENGTH,
    )?;
    safe_non_negative(
        evidence.created_at_millis,
        &format!("{path}.createdAtMillis"),
    )
}

#[cfg(test)]
mod tests {
    use winwincode_api::generated::StageRunId;

    use crate::domain::{Delivery, SessionBindingId, test_fixture};

    #[test]
    fn evidence_matches_current_spec_revision() {
        let mut fixture = test_fixture();
        fixture.evidence[0].delivery_spec_revision += 1;
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn evidence_matches_existing_stage_run() {
        let mut fixture = test_fixture();
        fixture.evidence[0].stage_run_id = StageRunId("foreign".into());
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn evidence_matches_stage_run_session_binding() {
        let mut fixture = test_fixture();
        fixture.evidence[0].session_binding_id = SessionBindingId("foreign".into());
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn evidence_does_not_predate_run_or_binding() {
        let mut fixture = test_fixture();
        fixture.evidence[0].created_at_millis = fixture.session_bindings[0].bound_at_millis - 1;
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn criterion_evidence_matches_current_candidate() {
        let mut fixture = test_fixture();
        fixture.evidence[0].candidate_ref =
            "git-tree:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into();
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }
}
