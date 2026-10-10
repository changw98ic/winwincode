// SPDX-License-Identifier: Apache-2.0
use super::action_bridge::{ActionGateState, current_binding_and_time, current_generation};
use winwincode_kernel::{
    ToolDependencySnapshot, ToolInputContext, ToolInputProof, ToolInputProofRequest,
};

pub(super) fn freeze(
    state: &ActionGateState,
    context: &ToolInputContext,
) -> Option<ToolDependencySnapshot> {
    let (binding, _) = current_binding_and_time(state, &context.request.thread_id).ok()?;
    if state
        .read_only_runs
        .read()
        .ok()?
        .contains_key(&binding.run_key)
    {
        return None;
    }
    let server = context.mcp_server.as_ref()?;
    if !context.request.tool_name.ends_with("public_smoke") {
        return None;
    }
    let ToolInputContext { request, .. } = context;
    let codex_payload = &request.payload;
    // ToolInputContext preserves the Core wire payload. Restrict this adapter to
    // the registered empty-argument public-example operation.
    if !matches!(codex_payload, winwincode_kernel::ToolInputGatePayload::Function { arguments } if serde_json::from_str::<serde_json::Value>(arguments).ok() == Some(serde_json::json!({})))
    {
        return None;
    }
    let adapters = state.tool_dependencies.read().ok()?;
    let adapter = adapters.get(&binding.run_key)?.get(server)?;
    let catalog = state.catalog.read().ok()?;
    let validity = format!(
        "{}:{}:{}:{}",
        binding.authority.lease.lease_id.0,
        binding.authority.lease.fencing_token.0,
        binding.authority.lease.attempt,
        current_generation(state, &context.request.thread_id).ok()?
    );
    // This adapter runs offline public examples. Its account is the installed
    // local server and worker; it has no provider credential or OAuth account.
    adapter.snapshot(
        &catalog.catalog_digest().0,
        &binding.authority.lease.worker_id.0,
        &context.request.thread_id,
        &validity,
    )
}
pub(super) fn verify(
    state: &ActionGateState,
    request: &ToolInputProofRequest,
) -> Option<ToolInputProof> {
    let current = freeze(state, &request.context)?;
    let mut expected = request.snapshot.clone();
    expected
        .dependency_digest
        .clone_from(&current.dependency_digest);
    if expected != current {
        return None;
    }
    let (binding, _) = current_binding_and_time(state, &request.context.request.thread_id).ok()?;
    let adapters = state.tool_dependencies.read().ok()?;
    adapters
        .get(&binding.run_key)?
        .get(request.context.mcp_server.as_ref()?)?
        .verify(request)
}
