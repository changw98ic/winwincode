// SPDX-License-Identifier: Apache-2.0

use super::UnifiedExecError;
use super::WriteStdinRequest;

/// Called while holding the exact process interaction lock. A queued permit
/// therefore cannot redirect input into a replacement process with the same id.
pub(super) async fn authorize_interaction(
    request: &WriteStdinRequest<'_>,
    origin_call_id: &str,
) -> Result<(), UnifiedExecError> {
    let Some(event) = &request.interaction_event else {
        return Ok(());
    };
    crate::tools::authorize_request(
        event.session.as_ref(),
        crate::ToolCallGateRequest {
            thread_id: event.session.thread_id.to_string(),
            turn_id: event.turn.sub_id.clone(),
            call_id: event.call_id.to_string(),
            namespace: None,
            tool_name: "write_stdin".to_string(),
            payload: crate::ToolCallGatePayload::ProcessInteraction {
                process_id: request.process_id,
                origin_call_id: origin_call_id.to_string(),
                input: request.input.to_string(),
            },
        },
    )
    .await
    .map_err(|error| UnifiedExecError::process_failed(error.to_string()))
}
