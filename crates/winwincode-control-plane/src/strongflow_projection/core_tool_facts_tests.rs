// SPDX-License-Identifier: Apache-2.0
use super::*;
use winwincode_domain::{ExecutionEventId, ExecutionSequence, Instant, Sha256Digest};
use winwincode_execution_port::generated::{
    CoreToolRuntimeFactPayloadSchemaVersion, EncodedPayload, ExecutionEventCategory,
};

fn event(fact: &Value) -> ExecutionEventRecord {
    let mut fact = fact.clone();
    let kind = fact["kind"].as_str().unwrap().to_owned();
    let body = fact["fact"].as_object_mut().unwrap();
    if kind == "request" {
        body.entry("request_sequence")
            .or_insert(serde_json::json!(7));
    }
    if kind == "cell" {
        body.entry("parent_request_sequence")
            .or_insert(serde_json::json!(1));
    }
    if kind == "sharing" {
        body.entry("source_attempt_id")
            .or_insert(serde_json::json!("actual"));
    }
    let bytes = serde_json::to_vec(&CoreToolRuntimeFactPayload {
        schema_version: CoreToolRuntimeFactPayloadSchemaVersion::WinwincodeCoreToolFactV1,
        source_thread_id: "core-thread".into(),
        source_sequence: ExecutionSequence(17),
        fact_json: fact.to_string(),
    })
    .unwrap();
    ExecutionEventRecord {
        event_id: ExecutionEventId("xevt_00000000000000000000000001".into()),
        sequence: ExecutionSequence(2),
        occurred_at: Instant("2030-01-01T00:00:00.000Z".into()),
        category: ExecutionEventCategory::Activity,
        summary: "Core tool runtime fact".into(),
        payload: Some(EncodedPayload {
            content_type: "application/vnd.winwincode.core-tool-fact+json".into(),
            data_base64: STANDARD.encode(&bytes),
            payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))),
        }),
    }
}

#[test]
fn request_disposition_and_execution_remain_distinct_in_read_projection() {
    for (execution, disposition, expected_status, expected_outcome) in [
        (
            "completed",
            "accepted",
            RuntimeActivityStatus::Completed,
            RuntimeActivityOutcome::Observed,
        ),
        (
            "completed",
            "rejected",
            RuntimeActivityStatus::Declined,
            RuntimeActivityOutcome::PolicyDenied,
        ),
        (
            "uncertain",
            "pending",
            RuntimeActivityStatus::Unknown,
            RuntimeActivityOutcome::Observed,
        ),
    ] {
        let source = event(
            &serde_json::json!({"kind":"request","fact":{"schema_version":1,"resolution":"observed","request":{"logical_id":"call","tool_name":"functions.shell_command"},"attempt":{"execution":execution,"disposition":disposition,"delivery":if disposition == "accepted" { "offered" } else { "pending" }}}}),
        );
        let projection = decode(&source, "runtime:original".into()).unwrap().unwrap();
        assert_eq!(
            (
                projection.activity_type,
                projection.status,
                projection.outcome
            ),
            (RuntimeActivityType::Tool, expected_status, expected_outcome)
        );
        assert_eq!(projection.call_id, "core:core-thread:request:7");
        assert_eq!(projection.source_ref, "runtime:original");
    }
}

#[test]
fn historical_cell_and_wait_records_do_not_assert_a_live_execution_owner() {
    for (fact, expected) in [
        (
            serde_json::json!({"kind":"cell","fact":{"schema_version":1,"sequence":3,"cell_id":"cell","lifecycle":"live"}}),
            RuntimeActivityStatus::Unknown,
        ),
        (
            serde_json::json!({"kind":"wait","fact":{"schema_version":1,"waiter_request_sequence":9,"target_cell_sequence":3,"state":"settled"}}),
            RuntimeActivityStatus::Completed,
        ),
    ] {
        assert_eq!(
            decode(&event(&fact), "source".into())
                .unwrap()
                .unwrap()
                .status,
            expected
        );
    }
}

#[test]
fn corrupted_or_unknown_core_facts_cannot_be_projected_as_completed_activity() {
    let mut source = event(&serde_json::json!({"kind":"cell","fact":{"schema_version":2}}));
    assert!(decode(&source, "source".into()).is_err());
    source.payload.as_mut().unwrap().data_base64 = STANDARD.encode(b"changed");
    assert!(decode(&source, "source".into()).is_err());
}

#[test]
fn completion_wait_and_diagnosis_preserve_the_same_typed_edges() {
    let edge = serde_json::json!({"tree_id":"tree","request_sequence":8,"logical_id":"wait-8",
        "source":{"kind":"cell","thread_id":"thread-a","owner_id":"owner-a","cell_id":"cell-a","scope_id":"scope-a"},
        "targets":[{"kind":"thread","thread_id":"thread-b","owner_id":"owner-b"}],"deadline_unix_ms":1_893_456_000_000_i64});
    let wait = serde_json::json!({"kind":"agent_wait","fact":{"schema_version":1,"thread_id":"thread-a","edge":edge,"state":"waiting"}});
    let waiting = decode(&event(&wait), "original-wait".into())
        .unwrap()
        .unwrap();
    assert_eq!(waiting.status, RuntimeActivityStatus::Unknown);
    let original = waiting.core_tool.unwrap().agent_wait.unwrap().edge;
    assert_eq!(original.source.owner_id, "owner-a");
    assert_eq!(original.source.cell_id.as_deref(), Some("cell-a"));
    assert_eq!(original.targets[0].thread_id, "thread-b");
    assert_eq!(original.targets[0].owner_id, "owner-b");
    assert_eq!(original.deadline_unix_ms, 1_893_456_000_000);
    let diagnostic = serde_json::json!({"kind":"diagnostic","fact":{"schema_version":1,"delivery":"offered",
        "diagnostic":{"schema_version":1,"diagnostic_id":"stable-id","evidence_version":8,"kind":"wait_cycle","question":"Which participant can advance the task?","evidence":[],"wait_graph":[edge]}}});
    let diagnosis = decode(&event(&diagnostic), "original-diagnosis".into())
        .unwrap()
        .unwrap();
    assert_eq!(diagnosis.status, RuntimeActivityStatus::Unknown);
    assert_eq!(
        diagnosis
            .core_tool
            .unwrap()
            .diagnosis
            .unwrap()
            .wait_graph
            .unwrap(),
        vec![original]
    );
    let mut settled = wait;
    settled["fact"]["state"] = serde_json::json!("settled");
    assert_eq!(
        decode(&event(&settled), "settled".into())
            .unwrap()
            .unwrap()
            .status,
        RuntimeActivityStatus::Completed
    );
}

#[test]
fn downstream_evidence_remains_separate_from_execution_completion() {
    for state in ["unavailable", "unconfirmed", "running", "exited"] {
        let fact = serde_json::json!({"kind":"reconciliation","fact":{"schema_version":1,"request_sequence":7,"evidence":{"state":state}}});
        let projection = decode(&event(&fact), "original".into()).unwrap().unwrap();
        assert_eq!(projection.status, RuntimeActivityStatus::Unknown);
        assert_eq!(projection.outcome, RuntimeActivityOutcome::Observed);
        assert_eq!(projection.call_id, "core:core-thread:request:7");
    }
}

#[test]
fn diagnosis_delivery_and_model_response_remain_observations() {
    let diagnosis = serde_json::json!({"schema_version":1,"diagnostic_id":"stable-id","evidence_version":8,"question":"Which action can advance the task?"});
    for delivery in ["queued", "offered"] {
        let source = event(
            &serde_json::json!({"kind":"diagnostic","fact":{"schema_version":1,"diagnostic":diagnosis,"delivery":delivery}}),
        );
        let projection = decode(&source, "source".into()).unwrap().unwrap();
        assert_eq!(projection.call_id, "core:core-thread:diagnostic:stable-id");
        assert_eq!(projection.status, RuntimeActivityStatus::Unknown);
        assert_eq!(projection.outcome, RuntimeActivityOutcome::Observed);
    }
    let source = event(
        &serde_json::json!({"kind":"diagnostic_response","fact":{"schema_version":1,"diagnostic_id":"stable-id","evidence_version":8}}),
    );
    let projection = decode(&source, "response".into()).unwrap().unwrap();
    assert_eq!(projection.status, RuntimeActivityStatus::Unknown);
    assert_eq!(projection.outcome, RuntimeActivityOutcome::Observed);
}

#[test]
fn sharing_and_waiter_cancellation_keep_their_original_logical_source() {
    for delivery in ["pending", "offered"] {
        let fact = serde_json::json!({"kind":"sharing","fact":{"schema_version":1,"request_sequence":9,"source_request_sequence":7,"kind":"merged","delivery":delivery}});
        let projection = decode(&event(&fact), "receipt".into()).unwrap().unwrap();
        assert_eq!(projection.call_id, "core:core-thread:request:9");
        assert_eq!(projection.source_ref, "receipt");
        assert_eq!(
            projection.status,
            if delivery == "offered" {
                RuntimeActivityStatus::Completed
            } else {
                RuntimeActivityStatus::Unknown
            }
        );
    }
    let fact = serde_json::json!({"kind":"waiter_cancellation","fact":{"schema_version":1,"request_sequence":9,"source_request_sequence":7}});
    let projection = decode(&event(&fact), "cancel".into()).unwrap().unwrap();
    assert_eq!(projection.call_id, "core:core-thread:request:9");
    assert_eq!(projection.source_ref, "cancel");
}

#[test]
fn ordered_core_facts_preserve_parent_cell_and_independent_shared_cancellation() {
    let mut projector = CoreToolProjector::default();
    let cell = event(
        &serde_json::json!({"kind":"cell","fact":{"schema_version":1,"sequence":3,"cell_id":"cell-3","parent_request_sequence":1,"lifecycle":"live"}}),
    );
    projector.decode(&cell, "cell-source".into()).unwrap();
    for sequence in [7, 8] {
        let request = event(
            &serde_json::json!({"kind":"request","fact":{"schema_version":1,"request_sequence":sequence,"request":{"logical_id":format!("child-{sequence}"),"tool_name":"functions.public_smoke","parent_call_id":"exec-parent","cell_id":"cell-3"},"attempt":null}}),
        );
        let projected = projector
            .decode(&request, "request-source".into())
            .unwrap()
            .unwrap();
        let meta = projected.core_tool.unwrap();
        assert_eq!(meta.call.unwrap().parent_request_sequence.unwrap().0, 1);
    }
    let shared = event(
        &serde_json::json!({"kind":"sharing","fact":{"schema_version":1,"request_sequence":8,"source_request_sequence":7,"source_attempt_id":"actual-7","kind":"merged","disposition":"pending","delivery":"pending","cancelled":false}}),
    );
    let pending = projector
        .decode(&shared, "shared-source".into())
        .unwrap()
        .unwrap();
    assert_eq!(pending.call_id, "core:core-thread:request:8");
    assert_eq!(pending.command.as_deref(), Some("functions.public_smoke"));
    assert!(
        pending
            .core_tool
            .as_ref()
            .unwrap()
            .call
            .as_ref()
            .unwrap()
            .attempt_id
            .is_none()
    );
    let cancellation = event(
        &serde_json::json!({"kind":"waiter_cancellation","fact":{"schema_version":1,"request_sequence":8,"source_request_sequence":7}}),
    );
    let cancelled = projector
        .decode(&cancellation, "cancel-source".into())
        .unwrap()
        .unwrap();
    assert_eq!(cancelled.call_id, pending.call_id);
    assert_eq!(cancelled.status, RuntimeActivityStatus::Cancelled);
    let meta = cancelled.core_tool.unwrap();
    assert_eq!(meta.sharing.unwrap().source_request_sequence.0, 7);
    assert_eq!(
        meta.call.unwrap().parent_call_id.as_deref(),
        Some("exec-parent")
    );
}

#[test]
fn long_runs_retain_recently_updated_relations_and_original_call_ids() {
    let mut projector = CoreToolProjector::default();
    let request = |sequence| serde_json::json!({"kind":"request","fact":{"schema_version":1,"request_sequence":sequence,"request":{"logical_id":format!("logical-{sequence}"),"tool_name":"functions.public_smoke","parent_call_id":null,"cell_id":null},"attempt":null}});
    for sequence in 1..=300 {
        projector
            .decode(&event(&request(sequence)), "request-source".into())
            .unwrap();
        if sequence % 50 == 0 {
            projector
                .decode(&event(&request(1)), "updated-source".into())
                .unwrap();
        }
    }
    for sequence in [1, 2] {
        let shared = event(
            &serde_json::json!({"kind":"sharing","fact":{"schema_version":1,"request_sequence":sequence,"source_request_sequence":500,"source_attempt_id":"actual-500","kind":"reuse","disposition":"accepted","delivery":"offered","cancelled":false}}),
        );
        let projected = projector
            .decode(&shared, "shared-source".into())
            .unwrap()
            .unwrap();
        assert_eq!(
            projected.call_id,
            format!("core:core-thread:request:{sequence}")
        );
        let meta = projected.core_tool.unwrap();
        assert_eq!(meta.sharing.unwrap().source_request_sequence.0, 500);
        if sequence == 1 {
            assert_eq!(meta.call.unwrap().tool_name, "functions.public_smoke");
        } else {
            assert!(
                meta.call.is_none(),
                "expired metadata cannot invent an original request"
            );
        }
    }
}
