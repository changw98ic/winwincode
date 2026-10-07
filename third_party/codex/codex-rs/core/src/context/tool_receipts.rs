// SPDX-License-Identifier: Apache-2.0
use super::ContextualUserFragment;
use codex_state::ToolExecutionStatus;
use codex_state::ToolOutputDelivery;
use codex_state::ToolOutputDisposition;
use serde::Serialize;

#[derive(Serialize)]
pub(crate) struct ToolReceipt {
    pub source_id: String,
    pub tool: String,
    pub request_sequence: i64,
    pub execution: Option<ToolExecutionStatus>,
    pub disposition: ToolOutputDisposition,
    pub delivery: ToolOutputDelivery,
    pub source_request_sequence: Option<i64>,
}
/// Four bounded metadata references; input and result bodies never enter this fragment.
pub(crate) struct ToolReceipts(pub Vec<ToolReceipt>);
impl ContextualUserFragment for ToolReceipts {
    fn role(&self) -> &'static str {
        "developer"
    }
    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }
    fn type_markers() -> (&'static str, &'static str) {
        ("<core_tool_receipts>", "</core_tool_receipts>")
    }
    fn body(&self) -> String {
        serde_json::json!({"type":"core_tool_receipts","schema_version":1,"receipts":self.0.iter().filter(|receipt| receipt.source_id.len() <= 80 && receipt.tool.chars().count() <= 64).take(4).collect::<Vec<_>>()}).to_string()
    }
}
