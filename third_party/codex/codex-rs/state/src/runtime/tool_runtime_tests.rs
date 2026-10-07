// SPDX-License-Identifier: Apache-2.0
use super::*;
use crate::SqliteConfig;
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
        tool_name: "functions.exec".into(),
        source: "model".into(),
        binding: "input".into(),
    };
    let ToolRequestObservation::New(fact) = store.observe_tool_request(&identity).await.unwrap()
    else {
        panic!("new")
    };
    store
        .claim_tool_attempt(fact.request_sequence, logical, "owner", "digest", "{}")
        .await
        .unwrap();
    fact.request_sequence
}

#[tokio::test]
async fn cells_and_waits_rebuild_from_one_ordered_stream_and_survive_restart() {
    let home = unique_temp_dir();
    let config = SqliteConfig::new_for_testing(home.as_path().abs());
    let store = StateRuntime::init(config.clone(), "test".into())
        .await
        .unwrap();
    let parent = request(&store, "exec").await;
    let cell = store
        .open_tool_cell("thread", "exec", "cell", "scope", "owner")
        .await
        .unwrap();
    assert_eq!(cell.parent_request_sequence, parent);
    let waiter = request(&store, "wait").await;
    store
        .begin_tool_cell_wait("thread", "wait", "cell", "scope", "owner")
        .await
        .unwrap();
    let reopened = StateRuntime::init(config, "test".into()).await.unwrap();
    let events = reopened
        .list_tool_runtime_events("thread", 0, 200)
        .await
        .unwrap();
    assert_eq!(events.len(), 6);
    assert!(
        matches!(&events[5].fact, ToolRuntimeFact::Wait(fact) if fact.state == ToolWaitState::Waiting && fact.waiter_request_sequence == waiter && fact.target_cell_sequence == cell.sequence)
    );
    assert!(
        reopened
            .settle_tool_cell_wait("thread", waiter, "another-owner")
            .await
            .is_err()
    );
    reopened
        .settle_tool_cell_wait("thread", waiter, "owner")
        .await
        .unwrap();
    reopened
        .close_tool_cell("thread", "cell", "scope", "another-owner")
        .await
        .unwrap();
    assert_eq!(
        reopened
            .list_tool_runtime_events("thread", events[5].sequence, 200)
            .await
            .unwrap()
            .len(),
        1
    );
    reopened
        .close_tool_cell("thread", "cell", "scope", "owner")
        .await
        .unwrap();
    request(&reopened, "late-wait").await;
    assert!(
        reopened
            .begin_tool_cell_wait("thread", "late-wait", "cell", "scope", "owner")
            .await
            .unwrap()
            .is_none()
    );
    let request_events = reopened
        .list_tool_fact_events("thread", 0, 200)
        .await
        .unwrap();
    assert_eq!(request_events.len(), 6);
    assert!(
        reopened
            .list_tool_runtime_events("another-thread", 0, 200)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn foreign_owner_and_unknown_cell_cannot_create_trusted_wait_edges() {
    let home = unique_temp_dir();
    let store = StateRuntime::init(
        SqliteConfig::new_for_testing(home.as_path().abs()),
        "test".into(),
    )
    .await
    .unwrap();
    request(&store, "exec").await;
    assert!(
        store
            .open_tool_cell("thread", "exec", "cell", "scope", "other")
            .await
            .is_err()
    );
    store
        .open_tool_cell("thread", "exec", "cell", "scope", "owner")
        .await
        .unwrap();
    request(&store, "wait").await;
    assert!(
        store
            .begin_tool_cell_wait("thread", "wait", "unknown", "scope", "owner")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .begin_tool_cell_wait("thread", "wait", "cell", "scope", "other")
            .await
            .is_err()
    );
    assert!(
        store
            .list_tool_runtime_events("thread", 0, 200)
            .await
            .unwrap()
            .iter()
            .all(|event| !matches!(event.fact, ToolRuntimeFact::Wait(_)))
    );
}
