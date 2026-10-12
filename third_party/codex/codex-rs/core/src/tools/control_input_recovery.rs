// SPDX-License-Identifier: Apache-2.0

//! Restores only a durable input control waiter. An old JavaScript continuation
//! and its effects remain lost. Neither execute nor wait is called here.

use super::context::ToolInvocation;
use super::context::ToolPayload;
use super::execution_facts::ToolFactReplay;
use super::execution_facts::digest;
use super::execution_facts::semantic_input;
use super::execution_facts::storage_error;
use super::handlers::RequestUserInputHandler;
use super::registry::CoreToolRuntime;
use super::registry::ToolExecutor;
use crate::ToolCallGateAttachment;
use crate::ToolResultReadRequest;
use crate::function_tool::FunctionCallError;
use codex_protocol::models::ResponseInputItem;
use codex_state::ControlInputRecovery;
use codex_state::ControlInputRecoveryHeader;
use codex_state::StateRuntime;
use codex_state::ToolExecutionFact;
use codex_state::ToolReconciliationFact;
use codex_state::ToolRecoveryEvidence;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;

struct RecoveryClaim {
    service: Arc<super::execution_facts::ExecutionFacts>,
    sequence: i64,
}
impl RecoveryClaim {
    fn acquire(replay: &ToolFactReplay) -> Result<Self, FunctionCallError> {
        let inserted = replay
            .service
            .active_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(replay.fact.request_sequence);
        if !inserted {
            return Err(error("control_input_recovery_in_progress"));
        }
        Ok(Self {
            service: Arc::clone(&replay.service),
            sequence: replay.fact.request_sequence,
        })
    }
}
impl Drop for RecoveryClaim {
    fn drop(&mut self) {
        self.service
            .active_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.sequence);
    }
}

pub(super) fn error(code: &str) -> FunctionCallError {
    FunctionCallError::RespondToModel(
        json!({"type":"tool_recovery","schema_version":1,"state":code,"success":false}).to_string(),
    )
}

pub(super) fn read_request(
    fact: &ToolExecutionFact,
    call_id: &str,
) -> Result<ToolResultReadRequest, FunctionCallError> {
    let attempt = fact
        .attempt
        .as_ref()
        .ok_or_else(|| error("control_input_receipt_missing"))?;
    Ok(ToolResultReadRequest {
        thread_id: fact.request.thread_id.clone(),
        turn_id: fact.request.turn_id.clone(),
        call_id: call_id.to_owned(),
        request_sequence: fact.request_sequence,
        logical_id: fact.request.logical_id.clone(),
        attempt_id: attempt.attempt_id.clone(),
        operation_digest: attempt.operation_digest.clone(),
        revision: attempt.revision,
    })
}

pub(super) fn result_ref(parent: &ToolExecutionFact, input: &ToolExecutionFact) -> String {
    format!("input-result:{}", digest(json!([parent, input])))
}

pub(super) fn interrupted_output(record: &ControlInputRecovery) -> FunctionCallError {
    let mut output = json!({"type":"tool_recovery","schema_version":1,"state":"code_cell_interrupted",
        "continuationLost":true,"success":false,"inputConsumed":true,
        "resultRef":record.result_ref,"responseDigest":record.response_digest,
        "resultReader":"request_user_input_result","cursor":"0"});
    if let Ok(response) = serde_json::from_str::<Value>(&record.response_json) {
        output["answers"] = response["answers"].clone();
        if output.to_string().len() > 8 * 1024 {
            let Some(fields) = output.as_object_mut() else {
                return error("control_input_result_invalid");
            };
            fields.remove("answers");
        }
    }
    FunctionCallError::RespondToModel(output.to_string())
}

async fn read_recovery_header(
    store: &StateRuntime,
    invocation: &ToolInvocation,
    reference: &str,
) -> Result<ControlInputRecoveryHeader, FunctionCallError> {
    if invocation.turn.session_source.is_non_root_agent() || reference.len() > 128 {
        return Err(error("control_input_result_denied"));
    }
    let thread = invocation.session.thread_id.to_string();
    let (parent, input) = store
        .control_input_recovery_receipts(&thread, reference)
        .await
        .map_err(storage_error)?
        .ok_or_else(|| error("control_input_result_unavailable"))?;
    if parent.request.turn_id != invocation.turn.sub_id
        || input.request.turn_id != invocation.turn.sub_id
        || reference != result_ref(&parent, &input)
    {
        return Err(error("control_input_result_scope_expired"));
    }
    let attachment = invocation
        .session
        .services
        .thread_extension_data
        .get::<ToolCallGateAttachment>()
        .ok_or_else(|| error("result_read_authority_unavailable"))?;
    let gate = attachment.gate();
    let parent_read = read_request(
        &parent,
        input.request.parent_call_id.as_deref().unwrap_or(""),
    )?;
    let input_read = read_request(&input, &input.request.logical_id)?;
    let parent_auth = gate
        .authorize_result_read(parent_read.clone())
        .await
        .map_err(|_| error("result_read_denied"))?;
    let input_auth = gate
        .authorize_result_read(input_read.clone())
        .await
        .map_err(|_| error("result_read_denied"))?;
    let header = store
        .control_input_recovery_header(&thread, reference)
        .await
        .map_err(storage_error)?
        .ok_or_else(|| error("control_input_result_unavailable"))?;
    if header.parent != parent
        || header.input != input
        || store
            .tool_execution_fact(parent.request_sequence)
            .await
            .map_err(storage_error)?
            != parent
        || store
            .tool_execution_fact(input.request_sequence)
            .await
            .map_err(storage_error)?
            != input
        || store
            .control_input_recovery_header(&thread, reference)
            .await
            .map_err(storage_error)?
            .as_ref()
            != Some(&header)
    {
        return Err(error("control_input_result_scope_expired"));
    }
    gate.revalidate_result_read(parent_read, parent_auth)
        .await
        .map_err(|_| error("result_read_denied"))?;
    gate.revalidate_result_read(input_read, input_auth)
        .await
        .map_err(|_| error("result_read_denied"))?;
    Ok(header)
}

/// Returns None for tools outside this narrowly allowed control recovery.
/// A successful recovery provides an interrupted exec result for the model.
pub(super) async fn recover(
    replay: &ToolFactReplay,
    invocation: &ToolInvocation,
    runtime: Option<&dyn CoreToolRuntime>,
) -> Result<Option<FunctionCallError>, FunctionCallError> {
    if invocation.tool_name.name != "exec"
        || !invocation
            .turn
            .config
            .experimental_request_user_input_enabled
        || invocation.turn.session_source.is_non_root_agent()
        || replay.fact.request.turn_id != invocation.turn.sub_id
        || replay
            .fact
            .attempt
            .as_ref()
            .is_none_or(|attempt| attempt.owner_id == replay.owner_id)
        || replay
            .service
            .active_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&replay.fact.request_sequence)
    {
        return Ok(None);
    }
    let Some(runtime) = runtime else {
        return Ok(None);
    };
    if !matches!(
        runtime.authorization_policy(),
        super::authorization::AuthorizationPolicy::CoreControl
    ) {
        return Ok(None);
    }
    // The current Core incarnation owns one reconstruction for this exact
    // parent at a time. The claim spans lookup, waiter consumption and COMMIT.
    let _claim = RecoveryClaim::acquire(replay)?;
    let Some(input) = replay
        .store
        .pending_control_input_for_parent(&replay.fact, &invocation.call_id)
        .await
        .map_err(storage_error)?
    else {
        return Ok(None);
    };
    let reference = result_ref(&replay.fact, &input);
    if replay
        .store
        .control_input_recovery_receipts(&input.request.thread_id, &reference)
        .await
        .map_err(storage_error)?
        .is_some()
    {
        let header = read_recovery_header(&replay.store, invocation, &reference).await?;
        return Ok(Some(FunctionCallError::RespondToModel(json!({"type":"tool_recovery","schema_version":1,
            "state":"code_cell_interrupted","continuationLost":true,"success":false,"inputConsumed":true,
            "resultRef":header.result_ref,"responseDigest":header.response_digest,
            "resultReader":"request_user_input_result","cursor":"0"}).to_string())));
    }
    let attachment = invocation
        .session
        .services
        .thread_extension_data
        .get::<ToolCallGateAttachment>()
        .ok_or_else(|| error("result_read_authority_unavailable"))?;
    let gate = attachment.gate();
    let parent_read = read_request(&replay.fact, &invocation.call_id)?;
    let input_read = read_request(&input, &input.request.logical_id)?;
    let parent_auth = gate
        .authorize_result_read(parent_read.clone())
        .await
        .map_err(|_| error("result_read_denied"))?;
    let input_auth = gate
        .authorize_result_read(input_read.clone())
        .await
        .map_err(|_| error("result_read_denied"))?;
    let parent_stored = replay
        .store
        .read_tool_results(replay.fact.request_sequence)
        .await
        .map_err(storage_error)?;
    let parent_effective: Value =
        serde_json::from_str(&parent_stored.effective_input).map_err(storage_error)?;
    let parent_payload = super::result_recovery::decode_payload(&parent_effective)?;
    if digest(
        json!({"tool":invocation.tool_name.to_string(),"definition":runtime.spec(),"input":semantic_input(&parent_payload)}),
    ) != replay
        .fact
        .attempt
        .as_ref()
        .ok_or_else(|| error("control_input_receipt_missing"))?
        .operation_digest
    {
        return Err(error("stored_result_scope_expired"));
    }
    let handler = RequestUserInputHandler {
        available_modes: codex_tools::request_user_input_available_modes(
            &invocation.turn.config.features,
        ),
    };
    let stored = replay
        .store
        .read_tool_results(input.request_sequence)
        .await
        .map_err(storage_error)?;
    let effective: Value = serde_json::from_str(&stored.effective_input).map_err(storage_error)?;
    let mut control = invocation.clone();
    control.call_id = input.request.logical_id.clone();
    control.tool_name = handler.tool_name();
    control.payload = super::result_recovery::decode_payload(&effective)?;
    if !matches!(&control.payload, ToolPayload::Function { .. })
        || digest(
            json!({"tool":control.tool_name.to_string(),"definition":handler.spec(),"input":semantic_input(&control.payload)}),
        ) != input
            .attempt
            .as_ref()
            .ok_or_else(|| error("control_input_receipt_missing"))?
            .operation_digest
    {
        return Err(error("stored_result_scope_expired"));
    }
    gate.revalidate_result_read(parent_read.clone(), parent_auth.clone())
        .await
        .map_err(|_| error("result_read_denied"))?;
    gate.revalidate_result_read(input_read.clone(), input_auth.clone())
        .await
        .map_err(|_| error("result_read_denied"))?;
    super::authorization::authorize(&control, &handler).await?;
    // Only this built-in control handler may reconstruct a callback. The old
    // executable script, host runtime and cell store are never invoked.
    let output = handler.handle(control.clone()).await?;
    let ResponseInputItem::FunctionCallOutput {
        output: response, ..
    } = output.to_response_item(&control.call_id, &control.payload)
    else {
        return Err(error("control_input_result_invalid"));
    };
    let response_json = response
        .body
        .to_text()
        .ok_or_else(|| error("control_input_result_invalid"))?;
    let _: codex_protocol::request_user_input::RequestUserInputResponse =
        serde_json::from_str(&response_json).map_err(storage_error)?;
    gate.revalidate_result_read(parent_read, parent_auth)
        .await
        .map_err(|_| error("result_read_denied"))?;
    gate.revalidate_result_read(input_read, input_auth)
        .await
        .map_err(|_| error("result_read_denied"))?;
    let record = ControlInputRecovery {
        result_ref: reference,
        parent: (*replay.fact).clone(),
        input,
        response_digest: digest(json!(response_json)),
        response_json,
    };
    let input_attempt = record
        .input
        .attempt
        .as_ref()
        .ok_or_else(|| error("control_input_receipt_missing"))?;
    let fact = ToolReconciliationFact {
        schema_version: 1,
        request_sequence: record.parent.request_sequence,
        thread_id: record.parent.request.thread_id.clone(),
        observer_id: replay.owner_id.clone(),
        attempt: record
            .parent
            .attempt
            .clone()
            .ok_or_else(|| error("control_input_receipt_missing"))?,
        evidence: ToolRecoveryEvidence::ControlInputResolved {
            input_request_sequence: record.input.request_sequence,
            input_operation_digest: input_attempt.operation_digest.clone(),
            response_digest: record.response_digest.clone(),
            result_ref: record.result_ref.clone(),
            continuation_lost: true,
        },
    };
    replay
        .store
        .record_control_input_recovery(
            &record,
            &super::handlers::request_user_input_result::prepare_pages(&record)?,
            &fact,
        )
        .await
        .map_err(storage_error)?;
    // BEGIN IMMEDIATE may wait across the original authority deadline. Keep
    // the consumed answer as private history, but authorize delivery again
    // after COMMIT using the exact current receipts and sealed header.
    let header = read_recovery_header(&replay.store, invocation, &record.result_ref).await?;
    if header.parent != record.parent
        || header.input != record.input
        || header.response_digest != record.response_digest
        || header.total_bytes != record.response_json.len()
    {
        return Err(error("control_input_result_scope_expired"));
    }
    Ok(Some(interrupted_output(&record)))
}

#[cfg(test)]
#[path = "control_input_recovery_tests.rs"]
mod tests;
