// SPDX-License-Identifier: Apache-2.0

use super::*;

async fn renew_at_ten_minutes(worker: &mut NativeWorker, original: &wire::ExecutionLeaseStamp) {
    let mut value = template("lease.renew");
    let mut lease = original.clone();
    lease.expires_at = at("00:30:00");
    value["messageId"] = serde_json::json!(message_id(80_000));
    value["sentAt"] = serde_json::json!(at("00:10:00"));
    value["priorExpiresAt"] = serde_json::json!(original.expires_at);
    value["lease"] = serde_json::to_value(&lease).unwrap();
    worker
        .accept_control(&serde_json::from_value(value).unwrap(), at("00:10:00"))
        .await
        .unwrap();
    assert_eq!(worker.active_jobs()[0].lease, lease);
}

fn input_receipts(root: &Fixture) -> i64 {
    core_database(root)
        .query_row(
            "SELECT COUNT(*) FROM execution_response_receipt WHERE family='input_request'",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

fn tool_output_count(value: &serde_json::Value, call: &str) -> usize {
    let here = usize::from(
        matches!(
            value["type"].as_str(),
            Some("function_call_output" | "custom_tool_call_output")
        ) && value["call_id"] == call,
    );
    here + match value {
        serde_json::Value::Object(fields) => fields
            .values()
            .map(|value| tool_output_count(value, call))
            .sum::<usize>(),
        serde_json::Value::Array(values) => values
            .iter()
            .map(|value| tool_output_count(value, call))
            .sum::<usize>(),
        _ => 0,
    }
}

async fn assert_original_input_consumed_once(
    root: &Fixture,
    worker: &mut NativeWorker,
    port: &RecordedPort,
    request: &wire::InputRequestMessage,
) {
    let continuation = wait_open_at(worker, port, 1, at("00:11:00")).await;
    let body: serde_json::Value =
        serde_json::from_slice(&STANDARD.decode(&continuation.request.data_base64).unwrap())
            .unwrap();
    let output = tool_output(&body, "input-expiry-call")
        .expect("the original real Core waiter produced its tool result");
    assert!(
        output.to_string().contains("continue"),
        "the Provided choice reached the original waiter: {output}"
    );
    assert_eq!(
        tool_output_count(&body, "input-expiry-call"),
        1,
        "one logical input produces exactly one Core tool result"
    );
    assert_eq!(
        input_operation_state(root, &request.input_request_id.0).0,
        "resolved"
    );
    assert_eq!(
        input_receipts(root),
        1,
        "the exact response has one durable receipt proof"
    );
    worker
        .accept_control(&input_response_at(request, at("00:11:00")), at("00:11:00"))
        .await
        .unwrap();
    for _ in 0..3 {
        Box::pin(worker.poll_codex(at("00:11:00"))).await.unwrap();
    }
    assert_eq!(input_receipts(root), 1);
    assert_eq!(
        port.messages()
            .iter()
            .filter(|message| matches!(message, wire::ExecutionPortMessage::ModelOpenMessage(_)))
            .count(),
        2,
        "exact response replay does not consume a second waiter or create another model exchange"
    );
    Box::pin(worker.shutdown(at("00:11:01"))).await.unwrap();
}

#[test]
fn m12_unrenewed_provided_response_consumes_original_waiter_once() {
    run_native(async {
        let root = Fixture::new();
        let (mut worker, port, _, request) = start_pending_input(&root).await;
        assert_eq!(request.expires_at, at("00:15:00"));
        worker
            .accept_control(&input_response_at(&request, at("00:11:00")), at("00:11:00"))
            .await
            .unwrap();
        assert_original_input_consumed_once(&root, &mut worker, &port, &request).await;
    });
}

#[test]
#[ignore = "mechanism audit: known red; renewed input must reach the original waiter"]
fn m12_legal_renewal_provided_at_eleven_minutes_reaches_original_waiter_once() {
    run_native(async {
        let root = Fixture::new();
        let (mut worker, port, _, request) = start_pending_input(&root).await;
        assert_eq!(request.expires_at, at("00:15:00"));
        renew_at_ten_minutes(&mut worker, &request.lease).await;
        let result = worker
            .accept_control(&input_response_at(&request, at("00:11:00")), at("00:11:00"))
            .await;
        eprintln!(
            "MECHANISM_M12 {}",
            serde_json::json!({"originalExpiry":request.expires_at,"renewalAt":at("00:10:00"),"currentExpiry":worker.active_jobs()[0].lease.expires_at,"providedAt":at("00:11:00"),"accepted":result.is_ok(),"operationState":input_operation_state(&root,&request.input_request_id.0).0,"receiptCount":input_receipts(&root),"error":format!("{result:?}")})
        );
        result.expect("same-attempt proven renewal must accept a Provided answer before the original deadline");
        assert_original_input_consumed_once(&root, &mut worker, &port, &request).await;
    });
}

#[test]
fn m12_changed_owner_fence_and_late_answers_do_not_settle_original_waiter() {
    run_native(async {
        for change in ["owner", "fence", "late"] {
            let root = Fixture::new();
            let (mut worker, port, _, request) = start_pending_input(&root).await;
            renew_at_ten_minutes(&mut worker, &request.lease).await;
            let clock = if change == "late" {
                at("00:15:01")
            } else {
                at("00:11:00")
            };
            let mut response = input_response_at(&request, clock.clone());
            if let wire::ExecutionPortMessage::InputResponseMessage(value) = &mut response {
                // Present the current expires_at so each negative independently reaches its identity/deadline check.
                value.lease = worker.active_jobs()[0].lease.clone();
                match change {
                    "owner" => {
                        value.lease.worker_instance_id.0 = "wki_00000000000000000000000999".into();
                    }
                    "fence" => value.lease.fencing_token.0 = "999".into(),
                    "late" => {}
                    _ => unreachable!(),
                }
            }
            worker
                .accept_control(&response, clock.clone())
                .await
                .expect_err(
                    "changed owner/fencing or a late answer cannot settle the original input",
                );
            assert_eq!(input_receipts(&root), 0);
            if change == "late" {
                let continuation = wait_open_at(&mut worker, &port, 1, clock.clone()).await;
                let body: serde_json::Value = serde_json::from_slice(
                    &STANDARD.decode(&continuation.request.data_base64).unwrap(),
                )
                .unwrap();
                let output = tool_output(&body, "input-expiry-call").unwrap();
                assert!(
                    output.to_string().contains("[]"),
                    "only timeout, not the late choice, reaches Core"
                );
            } else {
                for _ in 0..3 {
                    Box::pin(worker.poll_codex(clock.clone())).await.unwrap();
                }
                assert_eq!(
                    input_operation_state(&root, &request.input_request_id.0).0,
                    "pending"
                );
                assert_eq!(
                    port.messages()
                        .iter()
                        .filter(|message| matches!(
                            message,
                            wire::ExecutionPortMessage::ModelOpenMessage(_)
                        ))
                        .count(),
                    1
                );
            }
            Box::pin(worker.shutdown(clock)).await.unwrap();
        }
    });
}
