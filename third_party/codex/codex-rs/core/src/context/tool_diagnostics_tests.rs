// SPDX-License-Identifier: Apache-2.0
use super::ContextualUserFragment;
use super::ToolDiagnosticFeedback;
use codex_state::ToolDiagnostic;
use codex_state::ToolDiagnosticCall;
use codex_state::ToolDiagnosticKind;
use codex_utils_output_truncation::approx_token_count;

fn diagnosis() -> ToolDiagnostic {
    ToolDiagnostic {
        schema_version: 1,
        diagnostic_id: "d".repeat(80),
        thread_id: "thread".into(),
        kind: ToolDiagnosticKind::RepeatedOperation,
        evidence_version: i64::MAX,
        progress_source_sequence: Some(i64::MAX),
        question: "Is this repetition justified?".into(),
        evidence: vec![ToolDiagnosticCall {
            request_sequence: i64::MAX,
            logical_id: "logical".into(),
            tool_name: "mcp.fixture.check".into(),
            operation_digest: "a".repeat(64),
            parent_call_id: Some("parent-exec".into()),
            cell_id: Some("cell-1".into()),
        }],
    }
}

#[test]
fn diagnostic_context_caps_evidence_and_question_size() {
    for text in ["😀", "\u{0000}", "\\\""] {
        let mut diagnosis = diagnosis();
        diagnosis.question = text.repeat(10_000);
        diagnosis.evidence = (1..20)
            .map(|request_sequence| ToolDiagnosticCall {
                request_sequence,
                logical_id: "logical".into(),
                tool_name: text.repeat(10_000),
                operation_digest: "a".repeat(80),
                parent_call_id: Some("p".repeat(80)),
                cell_id: Some("c".repeat(80)),
            })
            .collect();
        let fragment = ToolDiagnosticFeedback(vec![diagnosis; 8]);
        let body: serde_json::Value = serde_json::from_str(&fragment.body()).unwrap();
        let diagnoses = body["diagnostics"].as_array().unwrap();
        assert_eq!(diagnoses.len(), 2);
        for diagnosis in diagnoses {
            assert!(serde_json::to_string(&diagnosis["question"]).unwrap().len() <= 256);
            let calls = diagnosis["calls"].as_array().unwrap();
            assert_eq!(calls.len(), 2);
            for call in calls {
                assert!(serde_json::to_string(&call["tool"]).unwrap().len() <= 96);
                assert_eq!(call["parent_call_id"], "p".repeat(80));
                assert_eq!(call["cell_id"], "c".repeat(80));
                assert_eq!(call["operation_digest"], "a".repeat(80));
            }
        }
        assert!(approx_token_count(&fragment.render()) < 1_000);
    }
}

#[test]
fn diagnostic_context_keeps_exact_call_provenance() {
    let diagnosis = diagnosis();
    let fragment = ToolDiagnosticFeedback(vec![diagnosis.clone()]);
    let body: serde_json::Value = serde_json::from_str(&fragment.body()).unwrap();
    let call = &body["diagnostics"][0]["calls"][0];
    assert_eq!(call["request_sequence"], i64::MAX);
    assert_eq!(call["parent_call_id"], "parent-exec");
    assert_eq!(call["cell_id"], "cell-1");
    assert_eq!(call["operation_digest"], "a".repeat(64));
    assert_eq!(body["diagnostics"][0]["question"], diagnosis.question);
}

#[test]
fn diagnostic_context_omits_oversized_references_without_aliasing_them() {
    for identity in [
        "x".repeat(81),
        "😀".repeat(80),
        "\u{0000}".repeat(80),
        "\\".repeat(80),
    ] {
        let mut diagnosis = diagnosis();
        diagnosis.evidence[0].parent_call_id = Some(identity.clone());
        diagnosis.evidence[0].cell_id = Some(identity.clone());
        diagnosis.evidence[0].operation_digest = identity;
        let fragment = ToolDiagnosticFeedback(vec![diagnosis; 8]);
        let body: serde_json::Value = serde_json::from_str(&fragment.body()).unwrap();
        for diagnosis in body["diagnostics"].as_array().unwrap() {
            let call = &diagnosis["calls"][0];
            assert!(call["parent_call_id"].is_null());
            assert!(call["cell_id"].is_null());
            assert!(call["operation_digest"].is_null());
            assert_eq!(call["request_sequence"], i64::MAX);
        }
        assert!(approx_token_count(&fragment.render()) < 1_000);
    }
}

#[test]
fn malformed_diagnostic_identity_cannot_expand_feedback() {
    let mut diagnosis = diagnosis();
    diagnosis.diagnostic_id = "\u{0000}".repeat(80);
    let fragment = ToolDiagnosticFeedback(vec![diagnosis; 8]);
    let body: serde_json::Value = serde_json::from_str(&fragment.body()).unwrap();
    assert!(body["diagnostics"].as_array().unwrap().is_empty());
    assert!(approx_token_count(&fragment.render()) < 1_000);
}
