// SPDX-License-Identifier: Apache-2.0

use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;

pub(super) async fn authorize(
    invocation: &ToolInvocation,
    server: &str,
    method: &str,
    arguments: &Option<serde_json::Value>,
) -> Result<(), FunctionCallError> {
    crate::tools::authorization::authorize_payload(
        invocation,
        crate::ToolCallGatePayload::McpResource {
            server: server.to_string(),
            method: method.to_string(),
            arguments: serde_json::to_string(arguments).map_err(|_| {
                FunctionCallError::RespondToModel("invalid MCP resource arguments".to_string())
            })?,
        },
    )
    .await
}
