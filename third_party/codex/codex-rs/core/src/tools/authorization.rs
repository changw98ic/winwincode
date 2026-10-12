// SPDX-License-Identifier: Apache-2.0

use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::registry::CoreToolRuntime;

/// Trusted runtime admission policy. Model arguments cannot select this policy.
#[derive(Clone, Copy)]
pub(crate) enum AuthorizationPolicy {
    /// The host interprets the effective raw request.
    HostOperation,
    /// Core orchestration. Each nested I/O operation is admitted separately.
    CoreControl,
    /// The handler authorizes its parsed input at the final execution boundary.
    ParsedOperation,
}

/// Authorize the effective input after PreToolUse hooks have accepted it.
pub(super) async fn authorize(
    invocation: &ToolInvocation,
    runtime: &dyn CoreToolRuntime,
) -> Result<(), FunctionCallError> {
    // These handlers authorize parsed commands or file changes at their final
    // execution boundary, after their own validation and sandbox planning.
    if matches!(
        runtime.authorization_policy(),
        AuthorizationPolicy::ParsedOperation
    ) {
        return Ok(());
    }
    let payload = match &invocation.payload {
        ToolPayload::Function { arguments } => crate::ToolCallGatePayload::Function {
            arguments: arguments.clone(),
        },
        ToolPayload::ToolSearch { arguments } => crate::ToolCallGatePayload::ToolSearch {
            arguments_json: serde_json::to_string(arguments).map_err(|_| {
                FunctionCallError::RespondToModel(
                    "tool authorization payload is invalid".to_string(),
                )
            })?,
        },
        ToolPayload::Custom { input } => crate::ToolCallGatePayload::Custom {
            input: input.clone(),
        },
    };
    let payload = match runtime.authorization_policy() {
        AuthorizationPolicy::CoreControl => {
            let input = match payload {
                crate::ToolCallGatePayload::Function { arguments } => {
                    serde_json::json!({"kind":"function", "input":arguments})
                }
                crate::ToolCallGatePayload::Custom { input } => {
                    serde_json::json!({"kind":"custom", "input":input})
                }
                crate::ToolCallGatePayload::ToolSearch { arguments_json } => {
                    serde_json::json!({"kind":"tool_search", "input":arguments_json})
                }
                _ => {
                    return Err(FunctionCallError::RespondToModel(
                        "unsupported Core control payload".to_string(),
                    ));
                }
            };
            crate::ToolCallGatePayload::CoreControl {
                input: input.to_string(),
            }
        }
        AuthorizationPolicy::HostOperation => payload,
        AuthorizationPolicy::ParsedOperation => {
            unreachable!("parsed operations authorize in their handler")
        }
    };
    authorize_payload(invocation, payload).await
}

/// Admit a handler's parsed operation using the same host boundary as raw calls.
pub(crate) async fn authorize_payload(
    invocation: &ToolInvocation,
    payload: crate::ToolCallGatePayload,
) -> Result<(), FunctionCallError> {
    let request = crate::ToolCallGateRequest {
        thread_id: invocation.session.thread_id.to_string(),
        turn_id: invocation.turn.sub_id.clone(),
        call_id: invocation.call_id.clone(),
        namespace: invocation.tool_name.namespace.clone(),
        tool_name: invocation.tool_name.name.clone(),
        payload,
    };
    authorize_request(invocation.session.as_ref(), request).await
}

pub(crate) async fn authorize_request(
    session: &crate::session::session::Session,
    request: crate::ToolCallGateRequest,
) -> Result<(), FunctionCallError> {
    let Some(attachment) = session
        .services
        .thread_extension_data
        .get::<crate::ToolCallGateAttachment>()
    else {
        return Ok(());
    };
    let authorization = attachment
        .gate()
        .authorize(request.clone())
        .await
        .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
    attachment
        .gate()
        .revalidate(request, authorization)
        .await
        .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))
}

#[cfg(test)]
#[path = "authorization_tests.rs"]
mod tests;
