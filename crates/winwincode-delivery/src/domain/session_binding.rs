// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use winwincode_domain::{
    CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionJobId, ProductSessionId, StageRunId,
    WorkerSessionId,
};

use super::{
    DeliveryValidationError, SessionBindingId, portable_identifier, safe_non_negative,
    schema_version,
};

/// Exact link between a Codex-backed Delivery stage and separately owned sessions.
///
/// Product, Delivery, task, StageRun, and ExecutionJob identities are immutable.
/// WorkerSession and CodexThread are filled only when their respective owners
/// report them. There is deliberately no generic `sessionId` or legacy DSH
/// session field in the canonical Rust model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionBinding {
    pub schema_version: u8,
    pub id: SessionBindingId,
    pub delivery_id: DeliveryId,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    pub delivery_task_id: Option<DeliveryTaskId>,
    pub stage_run_id: StageRunId,
    pub product_session_id: ProductSessionId,
    pub execution_job_id: ExecutionJobId,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    pub worker_session_id: Option<WorkerSessionId>,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    pub codex_thread_id: Option<CodexThreadId>,
    pub bound_at_millis: u64,
}

pub(crate) fn validate(
    binding: &SessionBinding,
    path: &str,
) -> Result<(), DeliveryValidationError> {
    schema_version(binding.schema_version, &format!("{path}.schemaVersion"))?;
    portable_identifier(&binding.id.0, &format!("{path}.id"))?;
    portable_identifier(&binding.delivery_id.0, &format!("{path}.deliveryId"))?;
    if let Some(task_id) = &binding.delivery_task_id {
        portable_identifier(&task_id.0, &format!("{path}.deliveryTaskId"))?;
    }
    portable_identifier(&binding.stage_run_id.0, &format!("{path}.stageRunId"))?;
    portable_identifier(
        &binding.product_session_id.0,
        &format!("{path}.productSessionId"),
    )?;
    portable_identifier(
        &binding.execution_job_id.0,
        &format!("{path}.executionJobId"),
    )?;
    if let Some(session_id) = &binding.worker_session_id {
        portable_identifier(&session_id.0, &format!("{path}.workerSessionId"))?;
    }
    if let Some(thread_id) = &binding.codex_thread_id {
        portable_identifier(&thread_id.0, &format!("{path}.codexThreadId"))?;
    }
    safe_non_negative(binding.bound_at_millis, &format!("{path}.boundAtMillis"))
}

#[cfg(test)]
mod tests {
    use winwincode_domain::{DeliveryId, DeliveryTaskId};

    use crate::domain::{Delivery, test_fixture};

    #[test]
    fn session_binding_matches_delivery_stage_run_and_task() {
        let mut fixture = test_fixture();
        fixture.session_bindings[0].delivery_task_id =
            Some(DeliveryTaskId("foreign-task".into()));
        assert!(Delivery::try_from_snapshot(fixture).is_err());

        let mut fixture = test_fixture();
        fixture.session_bindings[0].delivery_id = DeliveryId("foreign".into());
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }
}
