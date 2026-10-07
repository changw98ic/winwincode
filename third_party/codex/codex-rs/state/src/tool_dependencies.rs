// SPDX-License-Identifier: Apache-2.0
use serde::Deserialize;
use serde::Serialize;

/// Permission issued by a trusted adapter for a self-contained immutable value.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolReusePermission {
    Denied,
    ImmutableValue,
}

/// A shared read may continue after one logical waiter cancels its own wait.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCoalescingPermission {
    Denied,
    SharedRead,
}

/// Opaque trusted identities, never model-provided cache or idempotency keys.
/// A change in any field invalidates a prior sharing decision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolDependencySnapshot {
    pub policy_revision: String,
    pub dependency_digest: String,
    pub account_scope_digest: String,
    pub session_scope_digest: String,
    pub validity_epoch: String,
    pub reuse: ToolReusePermission,
    pub coalescing: ToolCoalescingPermission,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolInputBindingFact {
    pub schema_version: u32,
    pub thread_id: String,
    pub request_sequence: i64,
    pub operation_digest: String,
    pub snapshot: ToolDependencySnapshot,
}

/// The adapter checks its own receipt or frozen input, rather than trusting
/// fields returned by an arbitrary tool. Absence of proof remains unknown.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolInputProof {
    pub input_digest: String,
    pub evidence_digest: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolInputValidation {
    Verified,
    Mismatch,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolInputValidationFact {
    pub schema_version: u32,
    pub thread_id: String,
    pub request_sequence: i64,
    pub validation: ToolInputValidation,
    pub proof: Option<ToolInputProof>,
}
