// SPDX-License-Identifier: Apache-2.0
pub use codex_state::ToolCoalescingPermission;
pub use codex_state::ToolDependencySnapshot;
pub use codex_state::ToolInputProof;
pub use codex_state::ToolReusePermission;

/// Effective post-hook operation resolved by Core. The host selects an adapter
/// from trusted registration; model annotations never grant sharing permission.
#[derive(Clone)]
pub struct ToolInputContext {
    pub request: crate::ToolCallGateRequest,
    pub request_sequence: i64,
    pub operation_digest: String,
    pub mcp_server: Option<String>,
}

/// Private execution output provided only to the trusted adapter. It must check
/// its original frozen input or host-owned receipt before issuing a proof.
#[derive(Clone)]
pub struct ToolInputProofRequest {
    pub context: ToolInputContext,
    pub snapshot: ToolDependencySnapshot,
    pub output: serde_json::Value,
}
