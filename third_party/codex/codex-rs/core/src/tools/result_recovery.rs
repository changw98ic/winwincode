// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use codex_protocol::models::ResponseInputItem;
use codex_state::ToolExecutionStatus;
use codex_state::ToolOutputDisposition;
use serde::Deserialize;
use serde_json::Value;

use super::context::ToolInvocation;
use super::context::ToolOutput;
use super::context::ToolPayload;
use super::execution_facts::ToolFactReplay;
use super::execution_facts::digest;
use super::execution_facts::semantic_input;
use super::execution_facts::storage_error;
use super::registry::AnyToolResult;
use super::registry::CoreToolRuntime;
use crate::ToolCallGateAttachment;
use crate::ToolResultReadRequest;
use crate::function_tool::FunctionCallError;

/// Historical values do not recreate callbacks, continuations or external handles.
#[derive(Deserialize)]
pub(super) struct StoredOutput {
    schema_version: u32,
    response: ResponseInputItem,
    #[serde(default)]
    response_success: Option<bool>,
    code_mode_result: Value,
    success: bool,
    effective_input: Value,
    continuation: Option<Value>,
    #[serde(default)]
    post_hook_input: Option<Value>,
    #[serde(default)]
    post_hook_response: Option<Value>,
}

impl ToolOutput for StoredOutput {
    fn log_preview(&self) -> String {
        "recovered accepted tool output".into()
    }
    fn success_for_logging(&self) -> bool {
        self.success
    }
    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        let mut response = self.response.clone();
        match &mut response {
            ResponseInputItem::FunctionCallOutput {
                call_id: original_id,
                output,
                ..
            }
            | ResponseInputItem::CustomToolCallOutput {
                call_id: original_id,
                output,
                ..
            } => {
                *original_id = call_id.to_owned();
                output.success = self.response_success
            }
            _ => {}
        }
        response
    }
    fn post_tool_use_input(&self, _payload: &ToolPayload) -> Option<Value> {
        self.post_hook_input.clone()
    }
    fn post_tool_use_response(&self, _call_id: &str, _payload: &ToolPayload) -> Option<Value> {
        self.post_hook_response.clone()
    }
    fn code_mode_result(&self, _payload: &ToolPayload) -> Value {
        self.code_mode_result.clone()
    }
}

/// Rebind a self-contained raw result to a new logical request. Current hooks
/// receive the original handler's hook input/response and this caller's id.
pub(super) fn shared_output(
    encoded: &str,
    invocation: &ToolInvocation,
    runtime: &dyn CoreToolRuntime,
) -> Result<AnyToolResult, FunctionCallError> {
    let output: StoredOutput = serde_json::from_str(encoded).map_err(storage_error)?;
    if output.schema_version != 1 || output.continuation.is_some() {
        return Err(recovery_error("shared_result_not_self_contained"));
    }
    let post_tool_use_payload = runtime.post_tool_use_payload(invocation, &output);
    Ok(AnyToolResult {
        call_id: invocation.call_id.clone(),
        payload: invocation.payload.clone(),
        result: Box::new(output),
        post_tool_use_payload,
        continuation: None,
    })
}

pub(super) async fn recover(
    replay: ToolFactReplay,
    invocation: &ToolInvocation,
    runtime: Option<Arc<dyn CoreToolRuntime>>,
) -> Result<AnyToolResult, FunctionCallError> {
    if replay.fact.attempt.is_none()
        && let Some(shared) = replay
            .store
            .tool_sharing_fact(replay.fact.request_sequence)
            .await
            .map_err(storage_error)?
    {
        let runtime = runtime.ok_or_else(|| recovery_error("shared_runtime_unavailable"))?;
        return super::tool_sharing::recover_shared(replay, invocation, runtime, shared).await;
    }
    if replay.fact.attempt.as_ref().is_some_and(|attempt| {
        matches!(
            attempt.execution,
            ToolExecutionStatus::Running | ToolExecutionStatus::Uncertain
        )
    }) {
        return super::tool_reconciliation::reconcile(replay, invocation, runtime).await;
    }
    let attempt = replay.fact.attempt.as_ref();
    let Some(attempt) = attempt.filter(|attempt| {
        attempt.execution == ToolExecutionStatus::Completed
            && attempt.disposition == ToolOutputDisposition::Accepted
            && attempt.accepted_result_retained
    }) else {
        return Err(replay.response());
    };
    let Some(runtime) = runtime else {
        return Err(replay.response());
    };
    if !runtime.supports_result_replay() {
        let context =
            super::tool_sharing::has_trusted_recovery_policy(&replay, runtime.as_ref()).await?;
        if context {
            return super::tool_sharing::recover_actual(replay, invocation, runtime).await;
        }
        return Err(replay.response());
    }
    let Some(attachment) = invocation
        .session
        .services
        .thread_extension_data
        .get::<ToolCallGateAttachment>()
    else {
        return Err(recovery_error("result_read_authority_unavailable"));
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
        .map_err(|_| recovery_error("result_read_denied"))?;
    let stored = replay
        .store
        .read_accepted_tool_output(
            replay.fact.request_sequence,
            &attempt.attempt_id,
            attempt.revision,
        )
        .await
        .map_err(storage_error)?
        .ok_or_else(|| recovery_error("stored_result_unavailable"))?;
    let output: StoredOutput = serde_json::from_str(&stored.accepted.unwrap_or_default())
        .map_err(|_| recovery_error("stored_result_invalid"))?;
    let effective: Value = serde_json::from_str(&stored.effective_input)
        .map_err(|_| recovery_error("stored_result_invalid"))?;
    let payload = decode_payload(&effective)?;
    let operation = digest(serde_json::json!({
        "tool": invocation.tool_name.to_string(), "definition": runtime.spec(),
        "input": semantic_input(&payload),
    }));
    if output.schema_version != 1
        || output.continuation.is_some()
        || output.effective_input != effective
        || operation != attempt.operation_digest
    {
        return Err(recovery_error("stored_result_scope_expired"));
    }
    gate.revalidate_result_read(request, authorization)
        .await
        .map_err(|_| recovery_error("result_read_denied"))?;
    replay
        .store
        .offer_tool_output(
            replay.fact.request_sequence,
            &attempt.attempt_id,
            &attempt.owner_id,
        )
        .await
        .map_err(storage_error)?;
    Ok(AnyToolResult {
        call_id: invocation.call_id.clone(),
        payload,
        result: Box::new(output),
        post_tool_use_payload: None,
        continuation: None,
    })
}

pub(super) fn decode_payload(value: &Value) -> Result<ToolPayload, FunctionCallError> {
    let input = &value["input"];
    match value["kind"].as_str() {
        Some("function") => input.as_str().map(|arguments| ToolPayload::Function {
            arguments: arguments.into(),
        }),
        Some("custom") => input.as_str().map(|input| ToolPayload::Custom {
            input: input.into(),
        }),
        Some("tool_search") => serde_json::from_value(input.clone())
            .ok()
            .map(|arguments| ToolPayload::ToolSearch { arguments }),
        _ => None,
    }
    .ok_or_else(|| recovery_error("stored_result_invalid"))
}

fn recovery_error(state: &str) -> FunctionCallError {
    FunctionCallError::RespondToModel(
        serde_json::json!({
            "type": "tool_recovery", "schema_version": 1, "state": state,
        })
        .to_string(),
    )
}

#[cfg(test)]
#[path = "result_recovery_tests.rs"]
mod tests;
