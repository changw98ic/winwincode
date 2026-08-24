// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use winwincode_api::generated::{DeliveryId, DeliveryTaskId, StageRunId};

use super::{
    DeliveryValidationError, DeliveryValidationErrorCode, portable_identifier, positive,
    safe_non_negative, schema_version, validation_error,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryStage {
    #[serde(rename = "clarifying")]
    Clarifying,
    #[serde(rename = "planning")]
    Planning,
    #[serde(rename = "plan-review")]
    PlanReview,
    #[serde(rename = "executing")]
    Executing,
    #[serde(rename = "verifying")]
    Verifying,
    #[serde(rename = "reworking")]
    Reworking,
    #[serde(rename = "delivery-review")]
    DeliveryReview,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StageRunStatus {
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "waiting")]
    Waiting,
    #[serde(rename = "succeeded")]
    Succeeded,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "cancelled")]
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StageRunActorType {
    #[serde(rename = "codex")]
    Codex,
    #[serde(rename = "human")]
    Human,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StageRun {
    pub schema_version: u8,
    pub id: StageRunId,
    pub delivery_id: DeliveryId,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    pub delivery_task_id: Option<DeliveryTaskId>,
    pub stage: DeliveryStage,
    pub actor_type: StageRunActorType,
    pub role: String,
    pub status: StageRunStatus,
    pub attempt: u64,
    pub started_at_millis: u64,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    pub finished_at_millis: Option<u64>,
}

pub(crate) fn validate(run: &StageRun, path: &str) -> Result<(), DeliveryValidationError> {
    schema_version(run.schema_version, &format!("{path}.schemaVersion"))?;
    portable_identifier(&run.id.0, &format!("{path}.id"))?;
    portable_identifier(&run.delivery_id.0, &format!("{path}.deliveryId"))?;
    if let Some(task_id) = &run.delivery_task_id {
        portable_identifier(&task_id.0, &format!("{path}.deliveryTaskId"))?;
    }
    portable_identifier(&run.role, &format!("{path}.role"))?;
    positive(run.attempt, &format!("{path}.attempt"))?;
    safe_non_negative(run.started_at_millis, &format!("{path}.startedAtMillis"))?;
    if let Some(finished) = run.finished_at_millis {
        safe_non_negative(finished, &format!("{path}.finishedAtMillis"))?;
    }
    let active = matches!(
        run.status,
        StageRunStatus::Running | StageRunStatus::Waiting
    );
    let invalid = (active && run.finished_at_millis.is_some())
        || (!active && run.finished_at_millis.is_none())
        || run
            .finished_at_millis
            .is_some_and(|finished| finished < run.started_at_millis);
    if invalid {
        return Err(validation_error(
            DeliveryValidationErrorCode::InvalidValue,
            format!("{path}.finishedAtMillis"),
            "stage run finish time does not match its status or start time",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::domain::{Delivery, DeliveryStage, StageRunStatus, test_fixture};

    #[test]
    fn stage_run_status_requires_consistent_finish_time() {
        let mut fixture = test_fixture();
        fixture.stage_runs[0].status = StageRunStatus::Running;
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn rework_stage_run_requires_codex_remediator() {
        let mut fixture = test_fixture();
        fixture.stage_runs[0].stage = DeliveryStage::Reworking;
        fixture.stage_runs[0].role = "executor".into();
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn rework_stage_runs_respect_approved_attempt_limit() {
        let mut fixture = test_fixture();
        fixture.spec.max_rework_attempts = 0;
        fixture.stage_runs[0].stage = DeliveryStage::Reworking;
        fixture.stage_runs[0].role = "remediator".into();
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }
}
