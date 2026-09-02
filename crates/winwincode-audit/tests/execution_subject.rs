// SPDX-License-Identifier: Apache-2.0

use serde_json::json;
use winwincode_audit::{
    AuditBindingPhase, AuditBindingSource, AuditExecutionIdentity, AuditExecutionSubjectKind,
    AuditSubject, AuditSubjectKind,
};
use winwincode_domain::{
    CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionAckSequence, ExecutionJobId,
    ExecutionMessageId, FencingToken, LeaseId, ProductSessionId, StageRunId, WorkerId,
    WorkerInstanceId, WorkerSessionId,
};

fn id(prefix: &str, tail: char) -> String {
    format!("{prefix}_{}", tail.to_string().repeat(26))
}

fn execution_identity() -> AuditExecutionIdentity {
    AuditExecutionIdentity::try_new(
        ProductSessionId(id("psn", '1')),
        WorkerSessionId(id("wsn", '2')),
        CodexThreadId(id("cdx", '3')),
        StageRunId(id("run", '4')),
        ExecutionJobId(id("job", '5')),
        DeliveryId(id("dlv", '6')),
        Some(DeliveryTaskId(id("dtk", '7'))),
        WorkerId(id("wrk", '8')),
        WorkerInstanceId(id("wki", '9')),
        LeaseId(id("lse", 'A')),
        2,
        FencingToken("3".to_owned()),
        ExecutionAckSequence(11),
    )
    .expect("canonical execution identity")
}

fn accepted_binding_identity() -> AuditExecutionIdentity {
    AuditExecutionIdentity::try_new_binding(
        ProductSessionId(id("psn", '1')),
        WorkerSessionId(id("wsn", '2')),
        CodexThreadId(id("cdx", '3')),
        StageRunId(id("run", '4')),
        ExecutionJobId(id("job", '5')),
        DeliveryId(id("dlv", '6')),
        Some(DeliveryTaskId(id("dtk", '7'))),
        WorkerId(id("wrk", '8')),
        WorkerInstanceId(id("wki", '9')),
        LeaseId(id("lse", 'A')),
        2,
        FencingToken("3".to_owned()),
        AuditBindingSource::try_new(
            ExecutionMessageId(id("xmsg", 'B')),
            AuditBindingPhase::CodexThread,
        )
        .expect("canonical binding source"),
    )
    .expect("canonical accepted binding identity")
}

#[test]
fn execution_subject_variants_are_closed_and_locate_every_identity() {
    let identity = execution_identity();
    let cases = [
        (
            AuditSubject::accepted_binding(accepted_binding_identity()),
            AuditSubjectKind::Execution(AuditExecutionSubjectKind::AcceptedBinding),
            "accepted_binding",
        ),
        (
            AuditSubject::runtime(identity.clone()),
            AuditSubjectKind::Execution(AuditExecutionSubjectKind::Runtime),
            "runtime",
        ),
        (
            AuditSubject::terminal(identity),
            AuditSubjectKind::Execution(AuditExecutionSubjectKind::Terminal),
            "terminal",
        ),
    ];

    for (subject, expected_kind, expected_variant) in cases {
        assert_eq!(subject.kind(), expected_kind);
        let execution = subject
            .execution()
            .expect("execution subject branch must contain a complete identity");
        assert_eq!(subject.execution_kind(), expected_kind.execution_kind());
        assert_eq!(execution.product_session_id().0, id("psn", '1'));
        assert_eq!(execution.worker_session_id().0, id("wsn", '2'));
        assert_eq!(execution.codex_thread_id().0, id("cdx", '3'));
        assert_eq!(execution.stage_run_id().0, id("run", '4'));
        assert_eq!(execution.execution_job_id().0, id("job", '5'));
        assert_eq!(execution.delivery_id().0, id("dlv", '6'));
        assert_eq!(
            execution.delivery_task_id().map(|id| &id.0),
            Some(&id("dtk", '7'))
        );
        assert_eq!(execution.worker_id().0, id("wrk", '8'));
        assert_eq!(execution.worker_instance_id().0, id("wki", '9'));
        assert_eq!(execution.lease_id().0, id("lse", 'A'));
        assert_eq!(execution.attempt(), 2);
        assert_eq!(execution.fencing_token().0, "3");
        if expected_variant == "accepted_binding" {
            assert!(execution.source_sequence().is_none());
            assert!(execution.binding_source().is_some());
        } else {
            assert_eq!(execution.source_sequence().expect("ack sequence").0, 11);
            assert!(execution.binding_source().is_none());
        }

        let encoded = serde_json::to_value(&subject).expect("encode execution subject");
        assert_eq!(encoded["kind"], expected_variant);
        assert_eq!(encoded["product_session_id"], id("psn", '1'));
        assert_eq!(encoded["worker_session_id"], id("wsn", '2'));
        assert_eq!(encoded["codex_thread_id"], id("cdx", '3'));
        assert_eq!(encoded["stage_run_id"], id("run", '4'));
        assert_eq!(encoded["execution_job_id"], id("job", '5'));
        assert_eq!(encoded["delivery_id"], id("dlv", '6'));
        assert_eq!(encoded["delivery_task_id"], id("dtk", '7'));
        assert_eq!(encoded["worker_id"], id("wrk", '8'));
        assert_eq!(encoded["worker_instance_id"], id("wki", '9'));
        assert_eq!(encoded["lease_id"], id("lse", 'A'));
        assert_eq!(encoded["attempt"], 2);
        assert_eq!(encoded["fencing_token"], "3");
        if expected_variant == "accepted_binding" {
            assert!(encoded.get("source_sequence").is_none());
            assert!(encoded["binding_source"].is_object());
        } else {
            assert_eq!(encoded["source_sequence"], 11);
            assert!(encoded.get("binding_source").is_none());
        }
    }
}

#[test]
fn execution_subject_rejects_incomplete_or_cross_branch_shapes() {
    let subject = AuditSubject::accepted_binding(accepted_binding_identity());
    let mut encoded = serde_json::to_value(&subject).expect("encode complete subject");
    encoded
        .as_object_mut()
        .expect("subject object")
        .remove("worker_session_id");
    assert!(serde_json::from_value::<AuditSubject>(encoded).is_err());

    let mut malformed = serde_json::to_value(AuditSubject::runtime(execution_identity()))
        .expect("encode complete runtime subject");
    malformed["kind"] = json!("publication");
    assert!(serde_json::from_value::<AuditSubject>(malformed).is_err());

    assert!(
        AuditExecutionIdentity::try_new(
            ProductSessionId(id("psn", '1')),
            WorkerSessionId(id("wsn", '2')),
            CodexThreadId(id("cdx", '3')),
            StageRunId(id("run", '4')),
            ExecutionJobId(id("job", '5')),
            DeliveryId(id("dlv", '6')),
            None,
            WorkerId(id("wrk", '8')),
            WorkerInstanceId(id("wki", '9')),
            LeaseId(id("lse", 'A')),
            0,
            FencingToken("3".to_owned()),
            ExecutionAckSequence(11),
        )
        .is_err()
    );
}

#[test]
fn publication_subject_uses_its_own_branch_and_keeps_legacy_wire_shape() {
    let subject = AuditSubject::new()
        .with_delivery(DeliveryId(id("dlv", 'B')))
        .with_publication(winwincode_domain::PublicationId(id("pub", 'C')));
    assert_eq!(subject.kind(), AuditSubjectKind::Publication);
    let encoded = serde_json::to_value(&subject).expect("encode publication subject");
    assert_eq!(encoded["delivery"], id("dlv", 'B'));
    assert_eq!(encoded["publication"], id("pub", 'C'));
    assert!(encoded.get("product_session_id").is_none());
    assert!(encoded.get("worker_session_id").is_none());
}

#[test]
fn execution_identity_deserialization_rejects_semantically_invalid_fields() {
    let valid = serde_json::to_value(execution_identity()).expect("encode identity");
    for (field, invalid) in [
        ("product_session_id", json!(id("dlv", '1'))),
        ("attempt", json!(0)),
        ("fencing_token", json!("01")),
        ("source_sequence", json!(0)),
    ] {
        let mut candidate = valid.clone();
        candidate[field] = invalid;
        serde_json::from_value::<AuditExecutionIdentity>(candidate)
            .expect_err("semantic identity validation must run during deserialization");
    }

    let mut missing_source = valid.clone();
    missing_source
        .as_object_mut()
        .expect("identity object")
        .remove("source_sequence");
    assert!(serde_json::from_value::<AuditExecutionIdentity>(missing_source).is_err());

    let mut both_sources = valid;
    both_sources["binding_source"] = json!({
        "message_id": id("xmsg", 'B'),
        "phase": "codex_thread"
    });
    assert!(serde_json::from_value::<AuditExecutionIdentity>(both_sources).is_err());
}

#[test]
fn execution_subject_source_shape_is_bound_to_its_variant() {
    let runtime =
        serde_json::to_value(AuditSubject::runtime(execution_identity())).expect("runtime subject");
    let mut accepted_with_sequence = runtime.clone();
    accepted_with_sequence["kind"] = json!("accepted_binding");
    assert!(serde_json::from_value::<AuditSubject>(accepted_with_sequence).is_err());

    let accepted =
        serde_json::to_value(AuditSubject::accepted_binding(accepted_binding_identity()))
            .expect("accepted binding subject");
    let mut terminal_with_binding = accepted;
    terminal_with_binding["kind"] = json!("terminal");
    assert!(serde_json::from_value::<AuditSubject>(terminal_with_binding).is_err());
}

#[test]
fn accepted_binding_uses_a_typed_message_source_instead_of_a_fake_zero_sequence() {
    let subject = AuditSubject::accepted_binding(accepted_binding_identity());
    let encoded = serde_json::to_value(subject).expect("encode accepted binding subject");
    assert!(encoded["binding_source"].is_object());
    assert!(encoded.get("source_sequence").is_none());
}

#[test]
fn execution_subject_partial_builder_fails_closed() {
    let result = std::panic::catch_unwind(|| {
        AuditSubject::runtime(execution_identity()).with_delivery(DeliveryId(id("dlv", 'Z')))
    });
    assert!(result.is_err());
}
