// SPDX-License-Identifier: Apache-2.0
use crate::ToolDependencySnapshot;
use crate::ToolOutputDelivery;
use crate::ToolOutputDisposition;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSharingKind {
    Reuse,
    Merged,
}

/// One logical caller's independent disposition, linked to the original actual
/// attempt. Sharing does not create an additional execution attempt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolSharingFact {
    pub schema_version: u32,
    pub thread_id: String,
    pub request_sequence: i64,
    pub source_request_sequence: i64,
    pub source_attempt_id: String,
    pub operation_digest: String,
    pub snapshot: ToolDependencySnapshot,
    pub kind: ToolSharingKind,
    pub disposition: ToolOutputDisposition,
    pub delivery: ToolOutputDelivery,
    pub cancelled: bool,
}

/// Cancellation belongs to one logical request. The source actual attempt may
/// continue while another waiter still needs it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolWaiterCancellationFact {
    pub schema_version: u32,
    pub thread_id: String,
    pub request_sequence: i64,
    pub source_request_sequence: i64,
}
