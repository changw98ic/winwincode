// SPDX-License-Identifier: Apache-2.0
use super::*;
use crate::SqliteConfig;
use crate::ToolAttemptClaim;
use crate::ToolCoalescingPermission;
use crate::ToolExecutionStatus;
use crate::ToolOutputDisposition;
use crate::ToolRequestIdentity;
use crate::ToolRequestObservation;
use crate::runtime::test_support::unique_temp_dir;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

pub(super) fn snapshot(input: &str) -> ToolDependencySnapshot {
    ToolDependencySnapshot {
        policy_revision: "1".repeat(64),
        dependency_digest: input.repeat(64),
        account_scope_digest: "2".repeat(64),
        session_scope_digest: "3".repeat(64),
        validity_epoch: "4".repeat(64),
        reuse: ToolReusePermission::ImmutableValue,
        coalescing: ToolCoalescingPermission::SharedRead,
    }
}
pub(super) async fn attempt(
    store: &StateRuntime,
    logical: &str,
    snapshot: ToolDependencySnapshot,
    actual: Option<&str>,
) -> i64 {
    let request = ToolRequestIdentity {
        thread_id: "thread".into(),
        logical_id: logical.into(),
        turn_id: "turn".into(),
        scope_id: "scope".into(),
        cell_id: None,
        parent_call_id: None,
        tool_name: "mcp.fixture.public_smoke".into(),
        source: "code_mode".into(),
        binding: "binding".into(),
    };
    let ToolRequestObservation::New(fact) = store.observe_tool_request(&request).await.unwrap()
    else {
        panic!("new request")
    };
    let seq = fact.request_sequence;
    let ToolAttemptClaim::Claimed(_) = store
        .claim_tool_attempt(seq, logical, "owner", &"a".repeat(64), "{}")
        .await
        .unwrap()
    else {
        panic!("new execution")
    };
    let binding = ToolInputBindingFact {
        schema_version: 1,
        thread_id: "thread".into(),
        request_sequence: seq,
        operation_digest: "a".repeat(64),
        snapshot,
    };
    store.bind_tool_input(&binding).await.unwrap();
    store.bind_tool_input(&binding).await.unwrap();
    store
        .complete_tool_attempt(
            seq,
            logical,
            "owner",
            ToolExecutionStatus::Completed,
            Some("execution"),
        )
        .await
        .unwrap();
    let proof = actual.map(|actual| ToolInputProof {
        input_digest: actual.repeat(64),
        evidence_digest: actual.repeat(64),
    });
    store
        .validate_tool_input("thread", seq, proof.clone())
        .await
        .unwrap();
    store
        .validate_tool_input("thread", seq, proof)
        .await
        .unwrap();
    store
        .decide_tool_output(
            seq,
            logical,
            "owner",
            ToolOutputDisposition::Accepted,
            Some("accepted"),
        )
        .await
        .unwrap();
    seq
}

#[tokio::test]
async fn reuse_requires_verified_input_and_exact_current_policy_account_session_and_validity() {
    let home = unique_temp_dir();
    let config = SqliteConfig::new_for_testing(home.as_path().abs());
    let store = StateRuntime::init(config.clone(), "test".into())
        .await
        .unwrap();
    let original = snapshot("b");
    let first = attempt(&store, "first", original.clone(), Some("b")).await;
    let operation = "a".repeat(64);
    assert_eq!(
        store
            .reusable_tool_execution("thread", &operation, &original)
            .await
            .unwrap()
            .unwrap()
            .request_sequence,
        first
    );
    for field in 0..7 {
        let mut changed = original.clone();
        match field {
            0 => changed.policy_revision = "5".repeat(64),
            1 => changed.dependency_digest = "c".repeat(64),
            2 => changed.account_scope_digest = "6".repeat(64),
            3 => changed.session_scope_digest = "7".repeat(64),
            4 => changed.validity_epoch = "8".repeat(64),
            5 => changed.reuse = ToolReusePermission::Denied,
            6 => changed.coalescing = ToolCoalescingPermission::Denied,
            _ => unreachable!(),
        }
        assert!(
            store
                .reusable_tool_execution("thread", &operation, &changed)
                .await
                .unwrap()
                .is_none()
        );
    }
    assert!(
        store
            .reusable_tool_execution("foreign-thread", &operation, &original)
            .await
            .unwrap()
            .is_none()
    );
    // A source change during dispatch and an unknown result source cannot shadow
    // the original verified result, even though those executions were accepted.
    attempt(
        &store,
        "changed-during-execution",
        original.clone(),
        Some("c"),
    )
    .await;
    attempt(&store, "unknown-input", original.clone(), None).await;
    let reopened = StateRuntime::init(config, "test".into()).await.unwrap();
    assert_eq!(
        reopened
            .reusable_tool_execution("thread", &operation, &original)
            .await
            .unwrap()
            .unwrap()
            .request_sequence,
        first
    );
    let mut rejected = original.clone();
    rejected.dependency_digest = "d".repeat(64);
    let seq = attempt(&store, "new-result", rejected.clone(), Some("d")).await;
    sqlx::query("UPDATE tool_attempts SET disposition='rejected' WHERE request_sequence=?")
        .bind(seq)
        .execute(store.pool.as_ref())
        .await
        .unwrap();
    assert!(
        store
            .reusable_tool_execution("thread", &operation, &rejected)
            .await
            .unwrap()
            .is_none()
    );
    sqlx::query("UPDATE tool_attempts SET accepted_result=NULL WHERE request_sequence=?")
        .bind(first)
        .execute(store.pool.as_ref())
        .await
        .unwrap();
    assert!(
        store
            .reusable_tool_execution("thread", &operation, &original)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn only_new_verified_information_receipts_advance_progress_across_source_a_b_cycles() {
    let home = unique_temp_dir();
    let store = StateRuntime::init(
        SqliteConfig::new_for_testing(home.as_path().abs()),
        "test".into(),
    )
    .await
    .unwrap();
    for (logical, input, new_information) in [
        ("a1", "a", true),
        ("b1", "b", true),
        ("a2", "a", false),
        ("b2", "b", false),
    ] {
        let seq = attempt(&store, logical, snapshot(input), Some(input)).await;
        let progress = ToolProgressFact {
            schema_version: 1,
            thread_id: "thread".into(),
            request_sequence: seq,
            evidence_digest: input.repeat(64),
            source: "adapter-verified-new-information".into(),
        };
        assert_eq!(
            store.record_tool_progress(&progress).await.unwrap(),
            new_information
        );
        assert!(!store.record_tool_progress(&progress).await.unwrap());
        let forged = ToolProgressFact {
            evidence_digest: "f".repeat(64),
            ..progress
        };
        assert!(store.record_tool_progress(&forged).await.is_err());
    }
    let seq = attempt(&store, "unknown", snapshot("c"), None).await;
    let progress = ToolProgressFact {
        schema_version: 1,
        thread_id: "thread".into(),
        request_sequence: seq,
        evidence_digest: "c".repeat(64),
        source: "adapter".into(),
    };
    assert!(store.record_tool_progress(&progress).await.is_err());
    let events = store
        .list_tool_runtime_events("thread", 0, 200)
        .await
        .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.fact, ToolRuntimeFact::Progress(_)))
            .count(),
        2
    );
}
