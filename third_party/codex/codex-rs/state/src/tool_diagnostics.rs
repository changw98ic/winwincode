// SPDX-License-Identifier: Apache-2.0
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolDiagnosticKind {
    RepeatedOperation,
    AlternatingCycle,
    BranchExpansion,
    WaitCycle,
    UnavailableWait,
}

/// Payload-free evidence keeps original logical requests and parent relationships.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolDiagnosticCall {
    pub request_sequence: i64,
    pub logical_id: String,
    pub tool_name: String,
    pub operation_digest: String,
    pub parent_call_id: Option<String>,
    pub cell_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolDiagnostic {
    pub schema_version: u32,
    pub diagnostic_id: String,
    pub thread_id: String,
    pub kind: ToolDiagnosticKind,
    pub evidence_version: i64,
    pub progress_source_sequence: Option<i64>,
    pub evidence: Vec<ToolDiagnosticCall>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wait_graph: Vec<crate::ToolWaitEdge>,
    pub question: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolDiagnosticDelivery {
    Queued,
    Offered,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolDiagnosticFact {
    pub schema_version: u32,
    pub diagnostic: ToolDiagnostic,
    pub delivery: ToolDiagnosticDelivery,
    pub boundary_request_sequence: Option<i64>,
}

/// A trusted adapter supplies a verifiable receipt; model claims do not create progress.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolProgressFact {
    pub schema_version: u32,
    pub thread_id: String,
    pub request_sequence: i64,
    pub evidence_digest: String,
    pub source: String,
}

/// Records the model's subsequent response; it does not verify a progress claim.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolDiagnosticResponse {
    pub schema_version: u32,
    pub thread_id: String,
    pub diagnostic_id: String,
    pub evidence_version: i64,
    pub boundary_request_sequence: i64,
    pub turn_id: String,
    pub response_digest: String,
}
