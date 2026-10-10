// SPDX-License-Identifier: Apache-2.0

use serde::Deserialize;
use serde::Serialize;

/// Immutable identity of one model or runtime request. Digests contain no raw input.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolRequestIdentity {
    pub thread_id: String,
    pub logical_id: String,
    pub turn_id: String,
    pub scope_id: String,
    pub cell_id: Option<String>,
    pub parent_call_id: Option<String>,
    pub tool_name: String,
    pub source: String,
    pub binding: String,
}

/// Execution facts do not assert that a handler error had no side effects.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionStatus {
    Running,
    Completed,
    Uncertain,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutputDisposition {
    Pending,
    Accepted,
    Rejected,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutputDelivery {
    Pending,
    /// Core offered the output to its caller; this is not a model acknowledgement.
    Offered,
}

/// Payload-free receipt suitable for rebuilding projections and diagnostics.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolAttemptReceipt {
    pub attempt_id: String,
    pub owner_id: String,
    pub operation_digest: String,
    pub execution: ToolExecutionStatus,
    pub disposition: ToolOutputDisposition,
    pub delivery: ToolOutputDelivery,
    pub revision: i64,
    pub execution_result_retained: bool,
    pub accepted_result_retained: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRequestResolution {
    Observed,
    Denied,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolExecutionFact {
    pub schema_version: u32,
    pub request_sequence: i64,
    pub request: ToolRequestIdentity,
    pub resolution: ToolRequestResolution,
    pub attempt: Option<ToolAttemptReceipt>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolFactEvent {
    pub sequence: i64,
    pub fact: ToolExecutionFact,
}

/// Exact transport replay preserves the original observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolRequestObservation {
    New(ToolExecutionFact),
    Replay(ToolExecutionFact),
    IdentityConflict,
}

/// A successful claim is the only permission to dispatch a handler.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolAttemptClaim {
    Claimed(ToolExecutionFact),
    Existing(ToolExecutionFact),
}

/// Bound result bytes are separate from payload-free projection receipts.
/// Deliberately has no Debug implementation.
pub struct ToolStoredResults {
    pub effective_input: String,
    pub execution: Option<String>,
    pub accepted: Option<String>,
    pub rejection: Option<String>,
}

/// Receipt retention is independent of the optional result body retention.
pub const MAX_TOOL_RESULT_BYTES: usize = 4 * 1024 * 1024;
