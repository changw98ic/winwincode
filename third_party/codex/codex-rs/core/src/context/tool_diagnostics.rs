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
            .filter(|diagnostic| bounded_reference(&diagnostic.diagnostic_id).is_some()).take(2)
            .map(|diagnostic| serde_json::json!({
                "diagnostic_id": diagnostic.diagnostic_id, "evidence_version": diagnostic.evidence_version,
                "kind": diagnostic.kind, "progress": diagnostic.progress_source_sequence.map_or("unknown", |_| "verified_receipt"),
                "calls": diagnostic.evidence.iter().rev().take(2).map(|call| serde_json::json!({
                    "request_sequence": call.request_sequence,
                    "tool": bounded_text(&call.tool_name, 64, 96),
                    "parent_call_id": call.parent_call_id.as_deref().and_then(bounded_reference),
                    "cell_id": call.cell_id.as_deref().and_then(bounded_reference),
                    "operation_digest": bounded_reference(&call.operation_digest),
                })).collect::<Vec<_>>(),
                "question": bounded_text(&diagnostic.question, 256, 256),
                "wait_graph": bounded_wait_graph(&diagnostic.wait_graph),
            })).collect();
        serde_json::json!({
            "type": "model_behavior_diagnosis", "schema_version": 1,
            "assessment": "possible", "diagnostics": diagnostics,
            "next_step": "Assess the evidence, explain whether the waiting or repetition is justified, and choose the next action.",
        }).to_string()
    }
}

fn bounded_wait_graph(edges: &[codex_state::ToolWaitEdge]) -> serde_json::Value {
    if edges.is_empty() {
        return serde_json::Value::Null;
    }
    let value = serde_json::json!(edges);
    if edges.len() <= 8 && value.to_string().len() <= 4096 {
        value
    } else {
        serde_json::json!({"state":"evidence_exceeds_context_bound","edge_count":edges.len()})
    }
}

/// Omit an oversized identity rather than truncating it into a different identity.
fn bounded_reference(value: &str) -> Option<&str> {
    if value.len() > 80 {
        return None;
    }
    (serde_json::to_string(value).ok()?.len() <= 96).then_some(value)
}

/// Bound JSON-escaped bytes as well as characters, including control characters.
fn bounded_text(value: &str, max_chars: usize, max_json_bytes: usize) -> String {
    let mut remaining = max_json_bytes.saturating_sub(2);
    value
        .chars()
        .take(max_chars)
        .take_while(|character| {
            let size = match *character {
                '"' | '\\' => 2,
                '\u{0000}'..='\u{001f}' => 6,
                _ => character.len_utf8(),
            };
            if size > remaining {
                return false;
            }
            remaining -= size;
            true
        })
        .collect()
}
