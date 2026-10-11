// SPDX-License-Identifier: Apache-2.0

use super::*;

fn saved_interaction(root: &Fixture, id: &domain::ExecutionMessageId) -> (String, Vec<u8>) {
    core_database(root)
        .query_row(
            "SELECT correlation_key, frame_json FROM execution_outbox WHERE delivery_id=?1",
            [&id.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

fn assert_applied_source_retired(
    root: &Fixture,
    id: &domain::ExecutionMessageId,
    family: &str,
    original_key: &str,
    original_bytes: &[u8],
) {
    let database = core_database(root);
    let (key, bytes, required, state): (String, Vec<u8>, i64, String) = database
        .query_row(
            "SELECT correlation_key, frame_json, acknowledgement_required, state
             FROM execution_outbox WHERE delivery_id=?1",
            [&id.0],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(key, original_key);
    assert_eq!(bytes, original_bytes);
    assert_eq!(
        required, 0,
        "accepted current-stamp response must retire the original transport"
    );
    assert_eq!(state, "sent_attempt");
    assert_eq!(database.query_row(
        "SELECT COUNT(*) FROM execution_response_receipt WHERE family=?1 AND correlation_key=?2",
        [family, original_key], |row| row.get::<_, i64>(0),
    ).unwrap(), 1, "the exact accepted response proof is bound to its original source request");
    assert_eq!(
        database
            .query_row(
                "SELECT COUNT(*) FROM execution_response_receipt WHERE family=?1",
                [family],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );
}

#[test]
fn same_attempt_current_renewed_input_stamp_retires_exact_original_source_once() {
    run_native(async {
        let root = Fixture::new();
        let (mut worker, port, _, request) = start_pending_input(&root).await;
        let (source_key, source_bytes) = saved_interaction(&root, &request.message_id);
        mechanism_interactions::renew_at_ten_minutes(&mut worker, &request.lease).await;
        let mut response = input_response_at(&request, at("00:11:00"));
        let wire::ExecutionPortMessage::InputResponseMessage(value) = &mut response else {
            panic!("input response fixture")
        };
        value.lease = worker.active_jobs()[0].lease.clone();
        assert_ne!(value.lease.expires_at, request.lease.expires_at);
        worker
            .accept_control(&response, at("00:11:00"))
            .await
            .unwrap();
        assert_eq!(
            input_operation_state(&root, &request.input_request_id.0).0,
            "resolved"
        );
        assert_applied_source_retired(
            &root,
            &request.message_id,
            "input_request",
            &source_key,
            &source_bytes,
        );
        let continuation = wait_open_at(&mut worker, &port, 1, at("00:11:00")).await;
        let body: serde_json::Value =
            serde_json::from_slice(&STANDARD.decode(&continuation.request.data_base64).unwrap())
                .unwrap();
        assert_eq!(
            mechanism_interactions::tool_output_count(&body, "input-expiry-call"),
            1
        );
        assert!(
            tool_output(&body, "input-expiry-call")
                .unwrap()
                .to_string()
                .contains("continue")
        );
        worker
            .accept_control(&response, at("00:11:00"))
            .await
            .unwrap();
        worker.flush_durable_outbox().await.unwrap();
        assert_applied_source_retired(
            &root,
            &request.message_id,
            "input_request",
            &source_key,
            &source_bytes,
        );
        assert_eq!(
            port.messages()
                .iter()
                .filter(|message| matches!(
                    message,
                    wire::ExecutionPortMessage::ModelOpenMessage(_)
                ))
                .count(),
            2
        );
        Box::pin(worker.shutdown(at("00:11:01"))).await.unwrap();
    });
}

#[test]
fn same_attempt_current_renewed_approval_stamp_retires_exact_original_source_once() {
    run_native(async {
        let root = Fixture::new();
        let mut pending = start_pending_shell(&root).await;
        let (source_key, source_bytes) = saved_interaction(&root, &pending.approval.message_id);
        let current = renew_before_approval_expiry(&mut pending.worker, &pending.approval).await;
        let mut response = late_approval(&pending.approval);
        let wire::ExecutionPortMessage::ApprovalDecisionMessage(value) = &mut response else {
            panic!("approval decision fixture")
        };
        value.lease = current;
        value.sent_at = at("00:14:00");
        value.decided_at = at("00:14:00");
        value.decision = wire::ApprovalDecisionMessageDecision::Denied;
        pending
            .worker
            .accept_control(&response, at("00:14:00"))
            .await
            .unwrap();
        assert_applied_source_retired(
            &root,
            &pending.approval.message_id,
            "approval_request",
            &source_key,
            &source_bytes,
        );
        let continuation =
            wait_open_at(&mut pending.worker, &pending.port, 1, at("00:14:00")).await;
        let body: serde_json::Value =
            serde_json::from_slice(&STANDARD.decode(&continuation.request.data_base64).unwrap())
                .unwrap();
        assert_eq!(
            mechanism_interactions::tool_output_count(&body, "approval-expiry-call"),
            1
        );
        assert!(is_rejected_tool_result(&body));
        assert!(!pending.marker.exists());
        pending
            .worker
            .accept_control(&response, at("00:14:00"))
            .await
            .unwrap();
        pending.worker.flush_durable_outbox().await.unwrap();
        assert_applied_source_retired(
            &root,
            &pending.approval.message_id,
            "approval_request",
            &source_key,
            &source_bytes,
        );
        assert!(!pending.marker.exists());
        Box::pin(pending.worker.shutdown(at("00:14:01")))
            .await
            .unwrap();
    });
}
