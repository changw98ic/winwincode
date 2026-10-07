// SPDX-License-Identifier: Apache-2.0
use super::ContextualUserFragment;
use codex_state::ToolDiagnostic;

/// Compact references into durable evidence, delivered at a normal Code Mode boundary.
pub(crate) struct ToolDiagnosticFeedback(pub Vec<ToolDiagnostic>);
impl ContextualUserFragment for ToolDiagnosticFeedback {
    fn role(&self) -> &'static str {
        "developer"
    }
    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }
    fn type_markers() -> (&'static str, &'static str) {
        ("<model_behavior_diagnosis>", "</model_behavior_diagnosis>")
    }
    fn body(&self) -> String {
        let diagnostics: Vec<_> = self.0.iter()
            .filter(|diagnostic| diagnostic.diagnostic_id.len() <= 80).take(2)
            .map(|diagnostic| serde_json::json!({
                "diagnostic_id": diagnostic.diagnostic_id, "evidence_version": diagnostic.evidence_version,
                "kind": diagnostic.kind, "progress": diagnostic.progress_source_sequence.map_or("unknown", |_| "verified_receipt"),
                "calls": diagnostic.evidence.iter().rev().take(2).map(|call| serde_json::json!({
                    "request_sequence": call.request_sequence,
                    "tool": call.tool_name.chars().take(64).collect::<String>(),
                })).collect::<Vec<_>>(),
                "question": diagnostic.question.chars().take(256).collect::<String>(),
            })).collect();
        serde_json::json!({
            "type": "model_behavior_diagnosis", "schema_version": 1,
            "assessment": "possible", "diagnostics": diagnostics,
            "next_step": "Assess the evidence, explain whether the waiting or repetition is justified, and choose the next action.",
        }).to_string()
    }
}
