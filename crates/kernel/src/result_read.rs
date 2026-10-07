// SPDX-License-Identifier: Apache-2.0

/// Exact historical receipt requested by Core. Reading it grants no operation
/// execution or live handle authority.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct KernelToolResultReadRequest {
    pub session_id: String,
    pub turn_id: String,
    pub operation_id: String,
    pub request_sequence: i64,
    pub logical_id: String,
    pub attempt_id: String,
    pub operation_digest: String,
    pub revision: i64,
}

impl From<codex_core_api::ToolResultReadRequest> for KernelToolResultReadRequest {
    fn from(request: codex_core_api::ToolResultReadRequest) -> Self {
        Self {
            session_id: request.thread_id,
            turn_id: request.turn_id,
            operation_id: request.call_id,
            request_sequence: request.request_sequence,
            logical_id: request.logical_id,
            attempt_id: request.attempt_id,
            operation_digest: request.operation_digest,
            revision: request.revision,
        }
    }
}
