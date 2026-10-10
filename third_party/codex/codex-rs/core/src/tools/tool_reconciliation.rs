// SPDX-License-Identifier: Apache-2.0
use super::context::ToolInvocation;
use super::execution_facts::ToolFactReplay;
use super::execution_facts::digest;
use super::execution_facts::semantic_input;
use super::execution_facts::storage_error;
use super::registry::AnyToolResult;
use super::registry::CoreToolRuntime;
use crate::ToolCallGateAttachment;
use crate::ToolResultReadRequest;
use crate::function_tool::FunctionCallError;
use codex_state::ToolReconciliationFact;
use std::sync::Arc;

pub(super) async fn reconcile(
    replay: ToolFactReplay,
    invocation: &ToolInvocation,
    runtime: Option<Arc<dyn CoreToolRuntime>>,
) -> Result<AnyToolResult, FunctionCallError> {
    let active = replay
        .service
        .active_requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(&replay.fact.request_sequence);
    if active {
        return Err(replay.response());
    }
    let Some(runtime) = runtime else {
        return Err(replay.response());
    };
    let Some(attempt) = replay.fact.attempt.as_ref() else {
        return Err(replay.response());
    };
    let Some(attachment) = invocation
        .session
        .services
        .thread_extension_data
        .get::<ToolCallGateAttachment>()
    else {
        return Err(read_error(&replay, "result_read_authority_unavailable"));
    };
    let gate = attachment.gate();
    let request = ToolResultReadRequest {
        thread_id: invocation.session.thread_id.to_string(),
        turn_id: invocation.turn.sub_id.clone(),
        call_id: invocation.call_id.clone(),
        request_sequence: replay.fact.request_sequence,
        logical_id: replay.fact.request.logical_id.clone(),
        attempt_id: attempt.attempt_id.clone(),
        operation_digest: attempt.operation_digest.clone(),
        revision: attempt.revision,
    };
    let authorization = gate
        .authorize_result_read(request.clone())
        .await
        .map_err(|_| read_error(&replay, "result_read_denied"))?;
    let stored = replay
        .store
        .read_tool_results(replay.fact.request_sequence)
        .await
        .map_err(storage_error)?;
    let value = serde_json::from_str(&stored.effective_input)
        .map_err(|_| error("stored_result_invalid"))?;
    let mut original = invocation.clone();
    original.payload = super::result_recovery::decode_payload(&value)?;
    let operation = digest(
        serde_json::json!({"tool": original.tool_name.to_string(), "definition": runtime.spec(), "input": semantic_input(&original.payload)}),
    );
    if operation != attempt.operation_digest {
        return Err(error("stored_result_scope_expired"));
    }
    let evidence = runtime.reconcile_execution(&original).await;
    gate.revalidate_result_read(request, authorization)
        .await
        .map_err(|_| read_error(&replay, "result_read_denied"))?;
    let fact = ToolReconciliationFact {
        schema_version: 1,
        request_sequence: replay.fact.request_sequence,
        thread_id: replay.fact.request.thread_id.clone(),
        observer_id: replay.owner_id.clone(),
        attempt: attempt.clone(),
        evidence,
    };
    replay
        .store
        .record_tool_reconciliation(&fact)
        .await
        .map_err(storage_error)?;
    Err(FunctionCallError::RespondToModel(serde_json::json!({
        "type": "tool_recovery", "schema_version": 1, "state": "reconciled_execution_evidence", "fact": fact,
    }).to_string()))
}
fn read_error(replay: &ToolFactReplay, reason: &str) -> FunctionCallError {
    let response = replay.response();
    if let FunctionCallError::RespondToModel(message) = &response
        && let Ok(mut value) = serde_json::from_str::<serde_json::Value>(message)
    {
        value["validation_state"] = serde_json::json!(reason);
        return FunctionCallError::RespondToModel(value.to_string());
    }
    response
}

fn error(state: &str) -> FunctionCallError {
    FunctionCallError::RespondToModel(
        serde_json::json!({"type": "tool_recovery", "schema_version": 1, "state": state})
            .to_string(),
    )
}

#[cfg(test)]
#[path = "tool_reconciliation_tests.rs"]
mod tests;
