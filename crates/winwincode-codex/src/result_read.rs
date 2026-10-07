// SPDX-License-Identifier: Apache-2.0

use super::{ActionBridgeError, ActionGateState, current_binding_and_time, current_generation};
use sha2::{Digest, Sha256};
use winwincode_kernel::{KernelActionAuthorization, KernelToolResultReadRequest};

/// A live root `WorkRun` may read its own Core thread's historical receipts.
/// Delegated reads require a separately scoped result-read policy.
pub(super) fn authorize(
    state: &ActionGateState,
    request: &KernelToolResultReadRequest,
) -> Result<KernelActionAuthorization, ActionBridgeError> {
    let (binding, _) = current_binding_and_time(state, &request.session_id)?;
    if state
        .read_only_runs
        .read()
        .map_err(|_| ActionBridgeError::Unavailable)?
        .contains_key(&binding.run_key)
        || request.request_sequence <= 0
        || request.revision <= 0
        || request.logical_id.is_empty()
        || request.attempt_id.is_empty()
        || request.operation_digest.len() != 64
    {
        return Err(ActionBridgeError::Rejected);
    }
    let generation = current_generation(state, &request.session_id)?;
    let bytes = serde_json::to_vec(&(request, generation, &binding.run_key))
        .map_err(|_| ActionBridgeError::InvalidAction)?;
    Ok(KernelActionAuthorization::new(
        format!("result-read:{:x}", Sha256::digest(bytes)),
        None,
    ))
}

#[cfg(test)]
#[path = "result_read_tests.rs"]
mod tests;
