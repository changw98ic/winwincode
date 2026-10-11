// SPDX-License-Identifier: Apache-2.0
use super::*;
use crate::SqliteConfig;
use crate::ToolAttemptClaim;
use crate::ToolRecoveryEvidence;
use crate::ToolRequestIdentity;
use crate::ToolRequestObservation;
use crate::runtime::test_support::unique_temp_dir;
use codex_utils_absolute_path::test_support::PathExt;
use serde_json::json;

async fn fixture() -> (
    SqliteConfig,
    std::sync::Arc<StateRuntime>,
    ControlInputRecovery,
    Vec<ControlInputResultPage>,
    ToolReconciliationFact,
) {
    let home = unique_temp_dir();
    let config = SqliteConfig::new_for_testing(home.as_path().abs());
    let store = StateRuntime::init(config.clone(), "test".into())
        .await
        .unwrap();
    let mut request = ToolRequestIdentity {
        thread_id: "thread".into(),
        logical_id: "parent".into(),
        turn_id: "turn".into(),
        scope_id: "turn".into(),
        cell_id: None,
        parent_call_id: None,
        tool_name: "exec".into(),
        source: "model".into(),
        binding: "binding-parent".into(),
    };
    let ToolRequestObservation::New(observed) = store.observe_tool_request(&request).await.unwrap()
    else {
        panic!("new")
    };
    let ToolAttemptClaim::Claimed(parent) = store
        .claim_tool_attempt(
            observed.request_sequence,
            "parent-attempt",
            "original-owner",
            "parent-digest",
            "{}",
        )
        .await
        .unwrap()
    else {
        panic!("claim")
    };
    store
        .open_tool_cell("thread", "parent", "cell", "scope", "original-owner")
        .await
        .unwrap();
    request.logical_id = "input".into();
    request.scope_id = "scope".into();
    request.cell_id = Some("cell".into());
    request.parent_call_id = Some("parent-call".into());
    request.tool_name = "request_user_input".into();
    request.source = "code_mode".into();
    request.binding = "binding-input".into();
    let ToolRequestObservation::New(observed) = store.observe_tool_request(&request).await.unwrap()
    else {
        panic!("new")
    };
    let ToolAttemptClaim::Claimed(input) = store
        .claim_tool_attempt(
            observed.request_sequence,
            "input-attempt",
            "original-owner",
            "input-digest",
            "{}",
        )
        .await
        .unwrap()
    else {
        panic!("claim")
    };
    assert_eq!(
        store
            .pending_control_input_for_parent(&parent, "parent-call")
            .await
            .unwrap(),
        Some(input.clone())
    );
    assert!(
        store
            .pending_control_input_for_parent(&parent, "foreign-call")
            .await
            .unwrap()
            .is_none()
    );
    let response_json =
        json!({"answers":{"same":{"answers":["汉🦀\n\u{0000}\\\"".repeat(2_000)]}}}).to_string();
    let response_digest = private_string_digest(&response_json).unwrap();
    let record = ControlInputRecovery {
        result_ref: "input-result:test".into(),
        parent,
        input,
        response_digest,
        response_json,
    };
    let chars = record.response_json.chars().collect::<Vec<_>>();
    let count = chars.len().div_ceil(512);
    let pages=chars.chunks(512).enumerate().map(|(index,chunk)| {
        let cursor=index.to_string();
        let page_json=json!({"resultRef":record.result_ref,"responseDigest":record.response_digest,"cursor":cursor,
            "nextCursor":(index+1<count).then(||(index+1).to_string()),"totalBytes":record.response_json.len(),"content":chunk.iter().collect::<String>()}).to_string();
        let page_digest=private_string_digest(&page_json).unwrap();
        ControlInputResultPage {cursor,page_json,page_digest}
    }).collect::<Vec<_>>();
    let fact = ToolReconciliationFact {
        schema_version: 1,
        request_sequence: record.parent.request_sequence,
        thread_id: "thread".into(),
        observer_id: "new-owner".into(),
        attempt: record.parent.attempt.clone().unwrap(),
        evidence: ToolRecoveryEvidence::ControlInputResolved {
            input_request_sequence: record.input.request_sequence,
            input_operation_digest: "input-digest".into(),
            response_digest: record.response_digest.clone(),
            result_ref: record.result_ref.clone(),
            continuation_lost: true,
        },
    };
    (config, store, record, pages, fact)
}

#[tokio::test]
async fn control_input_recovery_preserves_full_body_and_reads_one_bounded_indexed_page_after_reopen()
 {
    let (config, store, record, pages, fact) = fixture().await;
    store
        .record_control_input_recovery(&record, &pages, &fact)
        .await
        .unwrap();
    store
        .record_control_input_recovery(&record, &pages, &fact)
        .await
        .unwrap();
    let reopened = StateRuntime::init(config, "test".into()).await.unwrap();
    let full = reopened
        .control_input_recovery("thread", &record.result_ref)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(full.response_json, record.response_json);
    let header = reopened
        .control_input_recovery_header("thread", &record.result_ref)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(header.page_count, pages.len());
    assert_eq!(header.total_bytes, record.response_json.len());
    let mut reconstructed = String::new();
    let mut bytes = 0;
    for expected in &pages {
        let actual = reopened
            .control_input_result_page("thread", &record.result_ref, &expected.cursor)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(actual.page_json, expected.page_json);
        assert!(actual.page_json.len() <= 8192);
        bytes += actual.page_json.len();
        let value: serde_json::Value = serde_json::from_str(&actual.page_json).unwrap();
        reconstructed.push_str(value["content"].as_str().unwrap());
    }
    assert_eq!(reconstructed, record.response_json);
    assert!(
        reopened
            .control_input_result_page("foreign-thread", &record.result_ref, "0")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        reopened
            .control_input_result_page("thread", &record.result_ref, "foreign-cursor")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        reopened
            .tool_execution_fact(record.parent.request_sequence)
            .await
            .unwrap(),
        record.parent
    );
    assert_eq!(
        reopened
            .tool_execution_fact(record.input.request_sequence)
            .await
            .unwrap(),
        record.input
    );
    eprintln!(
        "CONTROL_INPUT_INDEXED_READ rows={} serializedBytes={} originalResponseBytes={}",
        pages.len(),
        bytes,
        record.response_json.len()
    );
}

#[tokio::test]
async fn control_input_page_fault_rolls_back_full_body_pages_seal_and_evidence() {
    let (config, store, record, pages, fact) = fixture().await;
    sqlx::query("CREATE TRIGGER fail_input_page BEFORE INSERT ON tool_control_input_result_pages BEGIN SELECT RAISE(ABORT,'injected page failure'); END;").execute(store.pool.as_ref()).await.unwrap();
    assert!(
        store
            .record_control_input_recovery(&record, &pages, &fact)
            .await
            .is_err()
    );
    let reopened = StateRuntime::init(config, "test".into()).await.unwrap();
    assert_private_publication_absent(&reopened).await;
}

#[tokio::test]
async fn control_input_invalid_digests_never_publish_private_bytes_or_evidence() {
    for corrupt_response in [true, false] {
        let (_, store, mut record, mut pages, fact) = fixture().await;
        if corrupt_response {
            record.response_digest = "bad-response-sha".into();
        } else {
            pages[0].page_digest = "bad-page-sha".into();
        }
        assert!(
            store
                .record_control_input_recovery(&record, &pages, &fact)
                .await
                .is_err()
        );
        assert_private_publication_absent(&store).await;
    }
}

async fn assert_private_publication_absent(store: &StateRuntime) {
    let recoveries: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tool_control_input_recoveries")
        .fetch_one(store.pool.as_ref())
        .await
        .unwrap();
    let pages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tool_control_input_result_pages")
        .fetch_one(store.pool.as_ref())
        .await
        .unwrap();
    assert_eq!((recoveries, pages), (0, 0));
    let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM tool_fact_events WHERE json_extract(fact_json,'$.kind')='reconciliation'").fetch_one(store.pool.as_ref()).await.unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn control_input_ignored_publication_rolls_back_full_body_pages_seal_and_evidence() {
    let (config, store, record, pages, fact) = fixture().await;
    sqlx::query("CREATE TRIGGER ignore_input_publication BEFORE UPDATE OF sealed ON tool_control_input_recoveries WHEN NEW.sealed=1 BEGIN SELECT RAISE(IGNORE); END;").execute(store.pool.as_ref()).await.unwrap();
    assert!(
        store
            .record_control_input_recovery(&record, &pages, &fact)
            .await
            .is_err()
    );
    let reopened = StateRuntime::init(config, "test".into()).await.unwrap();
    assert_private_publication_absent(&reopened).await;
}

#[tokio::test]
async fn control_input_private_mutations_revoke_the_seal_and_header_mutations_are_rejected() {
    for mutation in [
        "UPDATE tool_control_input_result_pages SET page_json='{}' WHERE cursor='0'",
        "DELETE FROM tool_control_input_result_pages WHERE cursor='0'",
        "INSERT INTO tool_control_input_result_pages SELECT input_request_sequence,'foreign','{}','digest' FROM tool_control_input_recoveries",
        "UPDATE tool_control_input_recoveries SET record_json='{}'",
        "UPDATE tool_control_input_recoveries SET sealed=0",
        "UPDATE tool_control_input_recoveries SET invalidated=1",
    ] {
        let (_, store, record, pages, fact) = fixture().await;
        store
            .record_control_input_recovery(&record, &pages, &fact)
            .await
            .unwrap();
        assert!(
            sqlx::query("UPDATE tool_control_input_recoveries SET metadata_json='{}'")
                .execute(store.pool.as_ref())
                .await
                .is_err()
        );
        sqlx::query(mutation)
            .execute(store.pool.as_ref())
            .await
            .unwrap();
        assert!(
            sqlx::query("UPDATE tool_control_input_recoveries SET sealed=1")
                .execute(store.pool.as_ref())
                .await
                .is_err()
        );
        assert!(
            sqlx::query("UPDATE tool_control_input_recoveries SET invalidated=0")
                .execute(store.pool.as_ref())
                .await
                .is_err()
        );
        assert!(
            store
                .control_input_recovery_header("thread", &record.result_ref)
                .await
                .is_err()
        );
        assert!(
            store
                .control_input_result_page("thread", &record.result_ref, "0")
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn control_input_matching_forged_page_digest_cannot_reseal_changed_answer() {
    let (_, store, record, pages, fact) = fixture().await;
    store
        .record_control_input_recovery(&record, &pages, &fact)
        .await
        .unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&pages[0].page_json).unwrap();
    value["content"] = json!("forged answer with valid page SHA");
    let page_json = value.to_string();
    let page_digest = private_string_digest(&page_json).unwrap();
    sqlx::query(
        "UPDATE tool_control_input_result_pages SET page_json=?,page_digest=? WHERE cursor='0'",
    )
    .bind(page_json)
    .bind(page_digest)
    .execute(store.pool.as_ref())
    .await
    .unwrap();
    assert!(
        sqlx::query("UPDATE tool_control_input_recoveries SET sealed=1")
            .execute(store.pool.as_ref())
            .await
            .is_err()
    );
    assert!(
        store
            .control_input_recovery_header("thread", &record.result_ref)
            .await
            .is_err()
    );
    assert!(
        store
            .control_input_result_page("thread", &record.result_ref, "0")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn control_input_direct_delete_and_reinsert_cannot_replace_sealed_binding() {
    let (_, store, record, pages, fact) = fixture().await;
    store
        .record_control_input_recovery(&record, &pages, &fact)
        .await
        .unwrap();
    assert!(
        sqlx::query("DELETE FROM tool_control_input_recoveries")
            .execute(store.pool.as_ref())
            .await
            .is_err()
    );
    assert!(sqlx::query("INSERT OR REPLACE INTO tool_control_input_recoveries SELECT input_request_sequence,parent_request_sequence,thread_id,result_ref,'{}',metadata_json,page_count,1,0 FROM tool_control_input_recoveries").execute(store.pool.as_ref()).await.is_err());
    assert_eq!(
        store
            .control_input_recovery("thread", &record.result_ref)
            .await
            .unwrap()
            .unwrap()
            .response_json,
        record.response_json
    );
}

#[tokio::test]
async fn control_input_legal_attempt_cleanup_cascades_all_private_answers_and_pages() {
    for remove_parent in [true, false] {
        let (_, store, record, pages, fact) = fixture().await;
        store
            .record_control_input_recovery(&record, &pages, &fact)
            .await
            .unwrap();
        let sequence = if remove_parent {
            record.parent.request_sequence
        } else {
            record.input.request_sequence
        };
        sqlx::query("DELETE FROM tool_attempts WHERE request_sequence=?")
            .bind(sequence)
            .execute(store.pool.as_ref())
            .await
            .unwrap();
        let recoveries: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM tool_control_input_recoveries")
                .fetch_one(store.pool.as_ref())
                .await
                .unwrap();
        let pages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tool_control_input_result_pages")
            .fetch_one(store.pool.as_ref())
            .await
            .unwrap();
        assert_eq!((recoveries, pages), (0, 0));
    }
}
