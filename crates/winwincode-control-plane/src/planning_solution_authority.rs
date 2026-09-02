// SPDX-License-Identifier: Apache-2.0

//! Exact durable Planner result used for the Planning-to-PlanReview handoff.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use sha2::{Digest as _, Sha256};
use winwincode_api::generated::RepositoryScope;
use winwincode_delivery::{
    application::{
        solution_review::{PreparedPlannerSolutionReview, prepare_planner_solution_review},
        stage::{AdvanceStageInput, StageAdvanceResult},
    },
    domain::{Delivery, DeliveryStage, StageRunActorType},
};
use winwincode_execution_port::generated::ExecutionEventCategory;
use winwincode_storage::{ProductStateStorage, StorageError};

use crate::{
    DeliveryAuthorityError,
    runtime_event_transaction::{RuntimeLedgerState, runtime_stream_id_for_projection},
};

const PLANNER_SOLUTION_MEDIA_TYPE: &str = "application/vnd.winwincode.planner-solution+json";
const MAX_PLANNER_SOLUTION_BYTES: usize = 1024 * 1024;

pub(crate) fn prepare(
    storage: &dyn ProductStateStorage,
    scope: &RepositoryScope,
    delivery: &Delivery,
    input: AdvanceStageInput,
    attention_title: String,
    assigned_to: String,
) -> Result<StageAdvanceResult, DeliveryAuthorityError> {
    let run = current_planning_run(delivery)?;
    let binding = delivery
        .snapshot()
        .session_bindings
        .iter()
        .filter(|binding| binding.stage_run_id == run.id)
        .collect::<Vec<_>>();
    let [binding] = binding.as_slice() else {
        return Err(authority_error(
            "current Planner StageRun has no exact SessionBinding",
        ));
    };
    let scope_key = crate::repository_scope_key(scope).map_err(|error| storage_error(&error))?;
    let stream_id = runtime_stream_id_for_projection(&scope_key, &binding.execution_job_id);
    let state = storage
        .load_state(&stream_id)
        .map_err(|error| storage_error(&error))?
        .ok_or_else(|| authority_error("current Planner runtime ledger is missing"))?;
    let ledger: RuntimeLedgerState = serde_json::from_slice(&state.payload)
        .map_err(|_| authority_error("current Planner runtime ledger is invalid"))?;
    if serde_json::to_vec(&ledger).map_err(|_| authority_error("Planner ledger encode failed"))?
        != state.payload
        || state.revision != ledger.highest_sequence
        || ledger.delivery_id.as_ref() != Some(delivery.id())
        || ledger.delivery_task_id.is_some()
        || ledger.stage_run_id.as_ref() != Some(&run.id)
        || ledger.product_session_id != binding.product_session_id
        || ledger.execution_job_id != binding.execution_job_id
        || Some(&ledger.worker_session_id) != binding.worker_session_id.as_ref()
        || Some(&ledger.codex_thread_id) != binding.codex_thread_id.as_ref()
        || Some(&ledger.lease_id) != binding.lease_id.as_ref()
        || ledger.attempt != binding.attempt
        || Some(&ledger.fencing_token) != binding.fencing_token.as_ref()
        || Some(&ledger.worker_id) != binding.worker_id.as_ref()
        || Some(&ledger.worker_instance_id) != binding.worker_instance_id.as_ref()
        || usize::try_from(ledger.highest_sequence).ok() != Some(ledger.events.len())
    {
        return Err(authority_error(
            "current Planner runtime ledger identity is stale or foreign",
        ));
    }
    let outputs = ledger
        .events
        .iter()
        .filter_map(|entry| {
            let payload = entry.event.payload.as_ref()?;
            (entry.event.category == ExecutionEventCategory::Activity
                && payload.content_type == PLANNER_SOLUTION_MEDIA_TYPE)
                .then_some(payload)
        })
        .collect::<Vec<_>>();
    let [output] = outputs.as_slice() else {
        return Err(authority_error(
            "current Planner runtime must contain one exact Solution Review result",
        ));
    };
    let bytes = STANDARD
        .decode(&output.data_base64)
        .map_err(|_| authority_error("Planner Solution Review payload is not base64"))?;
    if bytes.is_empty() || bytes.len() > MAX_PLANNER_SOLUTION_BYTES {
        return Err(authority_error(
            "Planner Solution Review payload is outside the supported bound",
        ));
    }
    let digest = winwincode_domain::Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes)));
    if digest != output.payload_digest {
        return Err(authority_error(
            "Planner Solution Review payload digest does not match",
        ));
    }
    prepare_planner_solution_review(delivery, input, &bytes, attention_title, assigned_to)
        .map(PreparedPlannerSolutionReview::into_transition)
        .map_err(|error| authority_error(error.to_string()))
}

fn current_planning_run(
    delivery: &Delivery,
) -> Result<&winwincode_delivery::domain::StageRun, DeliveryAuthorityError> {
    let attempt = delivery
        .snapshot()
        .stage_runs
        .iter()
        .filter(|run| {
            run.delivery_task_id.is_none()
                && run.stage == DeliveryStage::Planning
                && run.actor_type == StageRunActorType::Codex
                && run.role == "planner"
        })
        .map(|run| run.attempt)
        .max()
        .ok_or_else(|| authority_error("Delivery has no Planner StageRun"))?;
    let runs = delivery
        .snapshot()
        .stage_runs
        .iter()
        .filter(|run| {
            run.delivery_task_id.is_none()
                && run.stage == DeliveryStage::Planning
                && run.actor_type == StageRunActorType::Codex
                && run.role == "planner"
                && run.attempt == attempt
        })
        .collect::<Vec<_>>();
    let [run] = runs.as_slice() else {
        return Err(authority_error(
            "Delivery has an ambiguous current Planner StageRun",
        ));
    };
    Ok(run)
}

fn storage_error(error: &StorageError) -> DeliveryAuthorityError {
    authority_error(error.to_string())
}

fn authority_error(message: impl Into<String>) -> DeliveryAuthorityError {
    DeliveryAuthorityError::new(message)
}
