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
async fn feedback_survives_interruption_and_is_offered_after_original_output() {
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
        wait_graph: Vec::new(),
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

const ORIGINAL_DIAGNOSTIC_OUTPUT: &str = "original output + diagnosis";

async fn diagnostic_offer_fixture(
    wait_graph: Vec::new(),
    question: String,
) -> (
    StateRuntime,
    i64,
    ToolDiagnostic,
    std::sync::Arc<std::sync::atomic::AtomicI64>,
) {
    let home = unique_temp_dir();
    let config = SqliteConfig::new_for_testing(home.as_path().abs());
    let initialized = StateRuntime::init(config.clone(), "test".into())
        .await
        .unwrap();
    let mut store = initialized.as_ref().clone();
    let options = store.pool.connect_options().as_ref().clone();
    store.pool.close().await;
    // Every replacement connection receives the current shared page budget.
    // A discarded FULL connection must not silently remove the fault condition.
    let page_limit = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(2147483646));
    let pool =
        crate::sqlite::open_page_limited_test_pool(options, std::sync::Arc::clone(&page_limit))
            .await
            .unwrap();
    store.pool = std::sync::Arc::new(pool);
    let original = request(&store, "original").await;
    let diagnostic = ToolDiagnostic {
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
        question,
    };
    store.enqueue_tool_diagnostic(&diagnostic).await.unwrap();
    let boundary = request(&store, "boundary").await;
    assert_eq!(
        store
            .stage_tool_diagnostic_feedback("thread", "boundary")
            .await
            .unwrap(),
        vec![diagnostic.clone()]
    );
    accept_diagnostic_boundary(&store, boundary, "boundary").await;
    (store, boundary, diagnostic, page_limit)
}

async fn accept_diagnostic_boundary(store: &StateRuntime, boundary: i64, logical: &str) {
    store
        .complete_tool_attempt(
            boundary,
            logical,
            "owner",
            ToolExecutionStatus::Completed,
            Some(ORIGINAL_DIAGNOSTIC_OUTPUT),
        )
        .await
        .unwrap();
    store
        .decide_tool_output(
            boundary,
            logical,
            "owner",
            ToolOutputDisposition::Accepted,
            Some(ORIGINAL_DIAGNOSTIC_OUTPUT),
        )
        .await
        .unwrap();
}

async fn offer_event_counts(store: &StateRuntime, boundary: i64) -> (i64, i64) {
    sqlx::query_as(
        "SELECT
        COALESCE(SUM(json_extract(fact_json,'$.kind')='request'
            AND json_extract(fact_json,'$.fact.attempt.delivery')='offered'),0),
        COALESCE(SUM(json_extract(fact_json,'$.kind')='diagnostic'
            AND json_extract(fact_json,'$.fact.delivery')='offered'),0)
        FROM tool_fact_events WHERE request_sequence=?",
    )
    .bind(boundary)
    .fetch_one(store.pool.as_ref())
    .await
    .unwrap()
}

async fn assert_required_offer_and_staged_feedback(store: &StateRuntime, boundary: i64) {
    let fact = store.tool_execution_fact(boundary).await.unwrap();
    let attempt = fact.attempt.unwrap();
    assert_eq!(attempt.execution, ToolExecutionStatus::Completed);
    assert_eq!(attempt.disposition, ToolOutputDisposition::Accepted);
    assert_eq!(attempt.delivery, crate::ToolOutputDelivery::Offered);
    assert_eq!(
        store
            .read_tool_results(boundary)
            .await
            .unwrap()
            .accepted
            .as_deref(),
        Some(ORIGINAL_DIAGNOSTIC_OUTPUT)
    );
    assert_eq!(offer_event_counts(store, boundary).await, (1, 0));
    let (offered, version): (i64, i64) = sqlx::query_as(
        "SELECT f.offered,d.offered_version
        FROM tool_diagnostic_feedback f JOIN tool_diagnostics d
        ON d.thread_id=f.thread_id AND d.diagnostic_id=f.diagnostic_id
        WHERE f.boundary_request_sequence=?",
    )
    .bind(boundary)
    .fetch_one(store.pool.as_ref())
    .await
    .unwrap();
    assert_eq!((offered, version), (0, 0));
}

async fn assert_same_boundary_retry(store: &StateRuntime, boundary: i64) {
    for _ in 0..2 {
        store
            .offer_tool_output(boundary, "boundary", "owner")
            .await
            .unwrap();
        assert_eq!(offer_event_counts(store, boundary).await, (1, 1));
    }
    let (required, optional): (i64, i64) = sqlx::query_as(
        "SELECT
        MAX(CASE WHEN json_extract(fact_json,'$.kind')='request'
            AND json_extract(fact_json,'$.fact.attempt.delivery')='offered' THEN sequence END),
        MAX(CASE WHEN json_extract(fact_json,'$.kind')='diagnostic'
            AND json_extract(fact_json,'$.fact.delivery')='offered' THEN sequence END)
        FROM tool_fact_events WHERE request_sequence=?",
    )
    .bind(boundary)
    .fetch_one(store.pool.as_ref())
    .await
    .unwrap();
    assert!(
        required < optional,
        "required output must commit before its optional accounting"
    );
}

const FAIL_DIAGNOSTIC_OFFER: &str = "CREATE TRIGGER fail_diagnostic_offer
    BEFORE INSERT ON tool_fact_events
    WHEN json_extract(NEW.fact_json,'$.kind')='diagnostic'
        AND json_extract(NEW.fact_json,'$.fact.delivery')='offered'
    BEGIN SELECT RAISE(ROLLBACK,'diagnostic offer fixture'); END";

#[tokio::test]
async fn optional_diagnostic_rollback_or_invalid_json_preserves_required_offer_and_retry() {
    for malformed_json in [false, true] {
        let (store, boundary, diagnostic, _) =
            diagnostic_offer_fixture("What can advance the task?".into()).await;
        if malformed_json {
            sqlx::query("UPDATE tool_diagnostic_feedback SET diagnostic_json='{' WHERE boundary_request_sequence=?")
                .bind(boundary).execute(store.pool.as_ref()).await.unwrap();
        } else {
            sqlx::query(FAIL_DIAGNOSTIC_OFFER)
                .execute(store.pool.as_ref())
                .await
                .unwrap();
        }
        store
            .offer_tool_output(boundary, "boundary", "owner")
            .await
            .unwrap();
        assert_required_offer_and_staged_feedback(&store, boundary).await;
        if malformed_json {
            sqlx::query("UPDATE tool_diagnostic_feedback SET diagnostic_json=? WHERE boundary_request_sequence=?")
                .bind(serde_json::to_string(&diagnostic).unwrap()).bind(boundary)
                .execute(store.pool.as_ref()).await.unwrap();
        } else {
            sqlx::query("DROP TRIGGER fail_diagnostic_offer")
                .execute(store.pool.as_ref())
                .await
                .unwrap();
        }
        assert_same_boundary_retry(&store, boundary).await;
    }
}

#[tokio::test]
async fn required_offer_event_failure_still_propagates_and_rolls_back_delivery() {
    let (store, boundary, _, _) =
        diagnostic_offer_fixture("What can advance the task?".into()).await;
    sqlx::query(
        "CREATE TRIGGER fail_required_offer BEFORE INSERT ON tool_fact_events
        WHEN json_extract(NEW.fact_json,'$.kind')='request'
            AND json_extract(NEW.fact_json,'$.fact.attempt.delivery')='offered'
        BEGIN SELECT RAISE(FAIL,'required offer fixture'); END",
    )
    .execute(store.pool.as_ref())
    .await
    .unwrap();
    let error = store
        .offer_tool_output(boundary, "boundary", "owner")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("required offer fixture"));
    let fact = store.tool_execution_fact(boundary).await.unwrap();
    let attempt = fact.attempt.unwrap();
    assert_eq!(attempt.disposition, ToolOutputDisposition::Accepted);
    assert_eq!(attempt.delivery, crate::ToolOutputDelivery::Pending);
    assert_eq!(offer_event_counts(&store, boundary).await, (0, 0));
    let offered: i64 = sqlx::query_scalar(
        "SELECT offered FROM tool_diagnostic_feedback WHERE boundary_request_sequence=?",
    )
    .bind(boundary)
    .fetch_one(store.pool.as_ref())
    .await
    .unwrap();
    assert_eq!(offered, 0);
    sqlx::query("DROP TRIGGER fail_required_offer")
        .execute(store.pool.as_ref())
        .await
        .unwrap();
    assert_same_boundary_retry(&store, boundary).await;
}

fn is_sqlite_full(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<sqlx::Error>()
        .and_then(sqlx::Error::as_database_error)
        .and_then(sqlx::error::DatabaseError::code)
        .is_some_and(|code| code == "13")
}

// These probes establish the storage condition without invoking the function
// under test. Both start from the same accepted/pending state and roll back.
enum OfferStorageProbe {
    Required,
    Diagnostic,
}

async fn probe_offer_storage(
    store: &StateRuntime,
    boundary: i64,
    body: &str,
    kind: OfferStorageProbe,
) -> anyhow::Result<()> {
    let mut connection = store.pool.acquire().await?;
    connection.close_on_drop();
    let mut tx = sqlx::Connection::begin_with(&mut *connection, "BEGIN IMMEDIATE").await?;
    if matches!(kind, OfferStorageProbe::Required) {
        sqlx::query(
            "UPDATE tool_attempts SET delivery='offered',revision=revision+1
            WHERE request_sequence=? AND disposition='accepted' AND delivery='pending'",
        )
        .bind(boundary)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "INSERT INTO tool_fact_events(request_sequence,thread_id,fact_json) VALUES (?,'thread',?)",
    )
    .bind(boundary)
    .bind(body)
    .execute(&mut *tx)
    .await?;
    tx.rollback().await?;
    connection.return_to_pool().await;
    Ok(())
}

#[tokio::test]
async fn sqlite_full_only_in_optional_diagnosis_preserves_output_and_retries_after_space_returns() {
    let (store, boundary, diagnostic, page_limit) =
        diagnostic_offer_fixture("?".repeat(12 * 1024)).await;
    let mut required = store.tool_execution_fact(boundary).await.unwrap();
    let attempt = required.attempt.as_mut().unwrap();
    attempt.delivery = crate::ToolOutputDelivery::Offered;
    attempt.revision += 1;
    let required_body = serde_json::to_string(&ToolRuntimeFact::Request(required)).unwrap();
    let optional_body = serde_json::to_string(&ToolRuntimeFact::Diagnostic(ToolDiagnosticFact {
        schema_version: 1,
        diagnostic,
        delivery: ToolDiagnosticDelivery::Offered,
        boundary_request_sequence: Some(boundary),
    }))
    .unwrap();
    let pages: i64 = sqlx::query_scalar("PRAGMA page_count")
        .fetch_one(store.pool.as_ref())
        .await
        .unwrap();
    let mut budget = None;
    for extra_pages in 0..=8 {
        let candidate = pages + extra_pages;
        page_limit.store(candidate, std::sync::atomic::Ordering::SeqCst);
        let actual: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "PRAGMA max_page_count={candidate}"
        )))
        .fetch_one(store.pool.as_ref())
        .await
        .unwrap();
        assert_eq!(actual, candidate);
        match probe_offer_storage(
            &store,
            boundary,
            &required_body,
            OfferStorageProbe::Required,
        )
        .await
        {
            Ok(()) => {}
            Err(error) => {
                assert!(
                    is_sqlite_full(&error),
                    "unexpected necessary probe failure: {error}"
                );
                continue;
            }
        }
        match probe_offer_storage(
            &store,
            boundary,
            &optional_body,
            OfferStorageProbe::Diagnostic,
        )
        .await
        {
            Ok(()) => {}
            Err(error) => {
                assert!(
                    is_sqlite_full(&error),
                    "the optional event must fail with real SQLITE_FULL: {error}"
                );
                budget = Some(candidate);
                break;
            }
        }
    }
    assert!(
        budget.is_some(),
        "fixture must prove required offer fits while the diagnosis does not"
    );
    assert_eq!(
        store
            .tool_execution_fact(boundary)
            .await
            .unwrap()
            .attempt
            .unwrap()
            .delivery,
        crate::ToolOutputDelivery::Pending
    );
    store
        .offer_tool_output(boundary, "boundary", "owner")
        .await
        .unwrap();
    assert_required_offer_and_staged_feedback(&store, boundary).await;
    page_limit.store(2147483646, std::sync::atomic::Ordering::SeqCst);
    sqlx::query_scalar::<_, i64>("PRAGMA max_page_count=2147483646")
        .fetch_one(store.pool.as_ref())
        .await
        .unwrap();
    assert_same_boundary_retry(&store, boundary).await;
}

#[tokio::test]
async fn retrying_older_feedback_preserves_newer_boundary_snapshot_and_version() {
    let (store, boundary, old, _) = diagnostic_offer_fixture("Older question?".into()).await;
    sqlx::query(FAIL_DIAGNOSTIC_OFFER)
        .execute(store.pool.as_ref())
        .await
        .unwrap();
    store
        .offer_tool_output(boundary, "boundary", "owner")
        .await
        .unwrap();
    assert_required_offer_and_staged_feedback(&store, boundary).await;
    let later = request(&store, "later-boundary").await;
    let mut newer = old.clone();
    newer.evidence_version = later;
    newer.question = "Newer question?".into();
    newer.evidence.push(ToolDiagnosticCall {
        request_sequence: later,
        logical_id: "later-boundary".into(),
        tool_name: "exec".into(),
        operation_digest: "digest".into(),
        parent_call_id: None,
        cell_id: None,
    });
    store.enqueue_tool_diagnostic(&newer).await.unwrap();
    assert_eq!(
        store
            .stage_tool_diagnostic_feedback("thread", "later-boundary")
            .await
            .unwrap(),
        vec![newer.clone()]
    );
    accept_diagnostic_boundary(&store, later, "later-boundary").await;
    sqlx::query("DROP TRIGGER fail_diagnostic_offer")
        .execute(store.pool.as_ref())
        .await
        .unwrap();
    store
        .offer_tool_output(later, "later-boundary", "owner")
        .await
        .unwrap();
    assert_same_boundary_retry(&store, boundary).await;
    let version: i64 = sqlx::query_scalar(
        "SELECT offered_version FROM tool_diagnostics WHERE diagnostic_id='stable-diagnosis'",
    )
    .fetch_one(store.pool.as_ref())
    .await
    .unwrap();
    assert_eq!(version, later);
    for (sequence, expected) in [(boundary, old), (later, newer)] {
        let encoded: String = sqlx::query_scalar(
            "SELECT fact_json FROM tool_fact_events
            WHERE request_sequence=? AND json_extract(fact_json,'$.kind')='diagnostic'
            AND json_extract(fact_json,'$.fact.delivery')='offered'",
        )
        .bind(sequence)
        .fetch_one(store.pool.as_ref())
        .await
        .unwrap();
        let ToolRuntimeFact::Diagnostic(fact) = serde_json::from_str(&encoded).unwrap() else {
            panic!("diagnostic event");
        };
        assert_eq!(fact.boundary_request_sequence, Some(sequence));
        assert_eq!(fact.diagnostic, expected);
        assert_eq!(offer_event_counts(&store, sequence).await, (1, 1));
    }
}
