// SPDX-License-Identifier: Apache-2.0
use super::ContextualUserFragment;
use super::ToolDiagnosticFeedback;
use codex_state::ToolDiagnostic;
use codex_state::ToolDiagnosticCall;
use codex_state::ToolDiagnosticKind;
use codex_utils_output_truncation::approx_token_count;
#[test]
fn diagnostic_context_caps_evidence_and_question_size() {
    let diagnosis = ToolDiagnostic {
        schema_version: 1,
        diagnostic_id: "d".repeat(80),
        thread_id: "thread".into(),
        kind: ToolDiagnosticKind::RepeatedOperation,
        evidence_version: 1,
        progress_source_sequence: None,
        question: "😀".repeat(10_000),
        evidence: (1..20)
            .map(|request_sequence| ToolDiagnosticCall {
                request_sequence,
                logical_id: "logical".into(),
                tool_name: "😀".repeat(10_000),
                operation_digest: "digest".into(),
                parent_call_id: None,
                cell_id: None,
            })
            .collect(),
    };
    let fragment = ToolDiagnosticFeedback(vec![diagnosis; 8]);
    let body: serde_json::Value = serde_json::from_str(&fragment.body()).unwrap();
    let diagnoses = body["diagnostics"].as_array().unwrap();
    assert_eq!(diagnoses.len(), 2);
    assert!(
        diagnoses
            .iter()
            .all(|item| item["calls"].as_array().unwrap().len() == 2)
    );
    assert_eq!(
        diagnoses[0]["question"].as_str().unwrap().chars().count(),
        256
    );
    assert!(approx_token_count(&fragment.render()) < 1_000);
}
