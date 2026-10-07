// SPDX-License-Identifier: Apache-2.0
use super::*;
use crate::SqliteConfig;
use crate::ToolDiagnosticCall;
use crate::ToolDiagnosticKind;
use crate::ToolExecutionStatus;
use crate::ToolOutputDisposition;
use crate::ToolRequestIdentity;
use crate::ToolRequestObservation;
use crate::runtime::test_support::unique_temp_dir;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
async fn request(store: &StateRuntime, logical: &str) -> i64 {
    let identity = ToolRequestIdentity {
        thread_id: "thread".into(),
        logical_id: logical.into(),
        turn_id: "turn".into(),
        scope_id: "scope".into(),
        cell_id: None,
        parent_call_id: None,
        tool_name: "exec".into(),
        source: "model".into(),
        binding: logical.into(),
    };
    let ToolRequestObservation::New(fact) = store.observe_tool_request(&identity).await.unwrap()
    else {
        panic!("new");
    };
    store
        .claim_tool_attempt(fact.request_sequence, logical, "owner", "digest", "{}")
        .await
        .unwrap();
    fact.request_sequence
}
#[tokio::test]
async fn feedback_survives_interruption_and_is_offered_atomically_with_original_output() {
    let home = unique_temp_dir();
    let config = SqliteConfig::new_for_testing(home.as_path().abs());
    let store = StateRuntime::init(config.clone(), "test".into())
        .await
        .unwrap();
    let original = request(&store, "original").await;
    let mut diagnostic = ToolDiagnostic {
        schema_version: 1,
        diagnostic_id: "stable-diagnosis".into(),
        thread_id: "thread".into(),
        kind: ToolDiagnosticKind::RepeatedOperation,
        evidence_version: original,
        progress_source_sequence: None,
        evidence: vec![ToolDiagnosticCall {
            request_sequence: original,
            logical_id: "original".into(),
            tool_name: "exec".into(),
            operation_digest: "digest".into(),
            parent_call_id: None,
            cell_id: None,
        }],
        question: "What can advance the task?".into(),
    };
    store.enqueue_tool_diagnostic(&diagnostic).await.unwrap();
    store.enqueue_tool_diagnostic(&diagnostic).await.unwrap();
    request(&store, "interrupted-boundary").await;
    assert_eq!(
        store
            .stage_tool_diagnostic_feedback("thread", "interrupted-boundary")
            .await
            .unwrap(),
        vec![diagnostic.clone()]
    );
    let restarted = StateRuntime::init(config, "test".into()).await.unwrap();
    let boundary = request(&restarted, "next-boundary").await;
    assert_eq!(
        restarted
            .stage_tool_diagnostic_feedback("thread", "next-boundary")
            .await
            .unwrap(),
        vec![diagnostic.clone()]
    );
    assert!(
        restarted
            .offer_tool_output(boundary, "next-boundary", "owner")
            .await
            .is_err()
    );
    restarted
        .complete_tool_attempt(
            boundary,
            "next-boundary",
            "owner",
            ToolExecutionStatus::Completed,
            Some("original output + diagnosis"),
        )
        .await
        .unwrap();
    restarted
        .decide_tool_output(
            boundary,
            "next-boundary",
            "owner",
            ToolOutputDisposition::Accepted,
            Some("original output + diagnosis"),
        )
        .await
        .unwrap();
    restarted
        .offer_tool_output(boundary, "next-boundary", "owner")
        .await
        .unwrap();
    restarted
        .offer_tool_output(boundary, "next-boundary", "owner")
        .await
        .unwrap();
    request(&restarted, "later-boundary").await;
    assert!(
        restarted
            .stage_tool_diagnostic_feedback("thread", "later-boundary")
            .await
            .unwrap()
            .is_empty()
    );
    let events = restarted
        .list_tool_runtime_events("thread", 0, 200)
        .await
        .unwrap();
    let deliveries: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.fact {
            ToolRuntimeFact::Diagnostic(fact) => Some(fact.delivery),
            _ => None,
        })
        .collect();
    assert_eq!(
        deliveries,
        vec![
            ToolDiagnosticDelivery::Queued,
            ToolDiagnosticDelivery::Offered
        ]
    );
    restarted
        .record_tool_diagnostic_response("thread", "other-turn", &"a".repeat(64))
        .await
        .unwrap();
    restarted
        .record_tool_diagnostic_response("thread", "turn", &"a".repeat(64))
        .await
        .unwrap();
    restarted
        .record_tool_diagnostic_response("thread", "turn", &"a".repeat(64))
        .await
        .unwrap();
    let responses = restarted
        .list_tool_runtime_events("thread", events.last().unwrap().sequence, 200)
        .await
        .unwrap();
    assert_eq!(responses.len(), 1);
    assert!(
        matches!(&responses[0].fact, ToolRuntimeFact::DiagnosticResponse(fact) if fact.evidence_version == original && fact.boundary_request_sequence == boundary)
    );
    diagnostic.evidence_version = boundary;
    restarted
        .enqueue_tool_diagnostic(&diagnostic)
        .await
        .unwrap();
    assert_eq!(
        restarted
            .stage_tool_diagnostic_feedback("thread", "later-boundary")
            .await
            .unwrap(),
        vec![diagnostic]
    );
}
