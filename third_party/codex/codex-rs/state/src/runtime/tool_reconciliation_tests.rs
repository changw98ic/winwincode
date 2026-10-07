// SPDX-License-Identifier: Apache-2.0
use super::*;
use crate::SqliteConfig;
use crate::ToolAttemptClaim;
use crate::ToolExecutionStatus;
use crate::ToolRecoveryEvidence;
use crate::ToolRequestIdentity;
use crate::ToolRequestObservation;
use crate::runtime::test_support::unique_temp_dir;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn reconciliation_deduplicates_evidence_and_preserves_original_execution() {
    let home = unique_temp_dir();
    let config = SqliteConfig::new_for_testing(home.as_path().abs());
    let store = StateRuntime::init(config.clone(), "test".into())
        .await
        .unwrap();
    let request = ToolRequestIdentity {
        thread_id: "thread".into(),
        logical_id: "logical".into(),
        turn_id: "turn".into(),
        scope_id: "scope".into(),
        cell_id: None,
        parent_call_id: None,
        tool_name: "exec_command".into(),
        source: "model".into(),
        binding: "binding".into(),
    };
    let ToolRequestObservation::New(observed) = store.observe_tool_request(&request).await.unwrap()
    else {
        panic!("new");
    };
    let ToolAttemptClaim::Claimed(claimed) = store
        .claim_tool_attempt(
            observed.request_sequence,
            "attempt",
            "original-owner",
            "digest",
            "{}",
        )
        .await
        .unwrap()
    else {
        panic!("claim");
    };
    let mut fact = ToolReconciliationFact {
        schema_version: 1,
        request_sequence: observed.request_sequence,
        thread_id: "thread".into(),
        observer_id: "new-owner".into(),
        attempt: claimed.attempt.clone().unwrap(),
        evidence: ToolRecoveryEvidence::Unconfirmed,
    };
    store.record_tool_reconciliation(&fact).await.unwrap();
    store.record_tool_reconciliation(&fact).await.unwrap();
    fact.evidence = ToolRecoveryEvidence::Running {
        business_id: "process-original".into(),
    };
    store.record_tool_reconciliation(&fact).await.unwrap();
    let reopened = StateRuntime::init(config, "test".into()).await.unwrap();
    assert_eq!(
        reopened.observe_tool_request(&request).await.unwrap(),
        ToolRequestObservation::Replay(claimed)
    );
    let events = reopened
        .list_tool_runtime_events("thread", 0, 200)
        .await
        .unwrap();
    assert_eq!(events.len(), 4);
    assert_eq!(
        events.last().unwrap().fact,
        ToolRuntimeFact::Reconciliation(fact.clone())
    );
    store
        .complete_tool_attempt(
            fact.request_sequence,
            "attempt",
            "original-owner",
            ToolExecutionStatus::Completed,
            None,
        )
        .await
        .unwrap();
    assert!(store.record_tool_reconciliation(&fact).await.is_err());
}
