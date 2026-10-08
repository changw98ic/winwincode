// SPDX-License-Identifier: Apache-2.0
use crate::ToolExecutionFact;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCellLifecycle {
    Live,
    Closed,
}

/// A recorded live lifecycle requires a current owner check after restart.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolCellFact {
    pub schema_version: u32,
    pub sequence: i64,
    pub thread_id: String,
    pub parent_request_sequence: i64,
    pub cell_id: String,
    pub scope_id: String,
    pub owner_id: String,
    pub lifecycle: ToolCellLifecycle,
    pub revision: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolWaitState {
    Waiting,
    Settled,
}

/// Created only after Core resolves a current cell, never from model-declared edges.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolWaitFact {
    pub schema_version: u32,
    pub thread_id: String,
    pub waiter_request_sequence: i64,
    pub target_cell_sequence: i64,
    pub owner_id: String,
    pub state: ToolWaitState,
    pub revision: i64,
}

/// A Core-resolved dependency. The owner identifies a live session incarnation.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolWaitNode {
    Thread {
        thread_id: String,
        owner_id: String,
    },
    Cell {
        thread_id: String,
        owner_id: String,
        cell_id: String,
        scope_id: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolWaitEdge {
    pub tree_id: String,
    pub request_sequence: i64,
    pub logical_id: String,
    pub source: ToolWaitNode,
    pub targets: Vec<ToolWaitNode>,
    pub deadline_unix_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolAgentWaitFact {
    pub schema_version: u32,
    pub thread_id: String,
    pub edge: ToolWaitEdge,
    pub state: ToolWaitState,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "fact", rename_all = "snake_case")]
pub enum ToolRuntimeFact {
    Request(ToolExecutionFact),
    Cell(ToolCellFact),
    Wait(ToolWaitFact),
    AgentWait(ToolAgentWaitFact),
    Reconciliation(ToolReconciliationFact),
    Diagnostic(crate::ToolDiagnosticFact),
    Progress(crate::ToolProgressFact),
    DiagnosticResponse(crate::ToolDiagnosticResponse),
    Sharing(crate::ToolSharingFact),
    WaiterCancellation(crate::ToolWaiterCancellationFact),
    InputBinding(crate::ToolInputBindingFact),
    InputValidation(crate::ToolInputValidationFact),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolRuntimeEvent {
    pub sequence: i64,
    pub fact: ToolRuntimeFact,
}

/// Read-only downstream evidence never takes ownership of the original execution.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ToolRecoveryEvidence {
    Unavailable,
    Unconfirmed,
    Running {
        business_id: String,
    },
    Exited {
        business_id: String,
        exit_code: Option<i32>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolReconciliationFact {
    pub schema_version: u32,
    pub request_sequence: i64,
    pub thread_id: String,
    pub observer_id: String,
    pub attempt: crate::ToolAttemptReceipt,
    pub evidence: ToolRecoveryEvidence,
}
