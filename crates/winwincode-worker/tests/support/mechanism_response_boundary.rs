// SPDX-License-Identifier: Apache-2.0

use super::*;

fn isolated_case(exact: &str, kind: &str, crash: bool) {
    if std::env::var_os("WWC_MECHANISM_RESPONSE_CHILD").is_some() {
        return;
    }
    let root = Fixture::new();
    let barrier = root.0.join("mechanism-barrier");
    fs::create_dir_all(&barrier).unwrap();
    fs::write(barrier.join(format!("{kind}.dispatch.arm")), b"armed").unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            exact,
            "--include-ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(
            "WWC_MECHANISM_RESPONSE_CHILD",
            if crash { "crash" } else { "normal" },
        )
        .env("WWC_APPROVAL_RESTART_DIRECTORY", &root.0)
        .env("WWC_MECHANISM_INTERACTION_BARRIER", &barrier)
        .output()
        .unwrap();
    eprintln!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    eprintln!(
        "MECHANISM_RA_C01_PROCESS {}",
        serde_json::json!({"kind":kind,"phase":if crash {"crash"} else {"normal"},"actualExitCode":output.status.code()})
    );
    assert_eq!(
        output.status.code(),
        Some(if crash { 73 } else { 0 }),
        "child must reach the exact ordinary-response barrier"
    );
    if crash {
        fs::write(
            barrier.join(format!("{kind}.dispatch.release")),
            b"release after original process exited",
        )
        .unwrap();
        let recovery = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                exact,
                "--include-ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("WWC_MECHANISM_RESPONSE_CHILD", "recover")
            .env("WWC_APPROVAL_RESTART_DIRECTORY", &root.0)
            .env("WWC_MECHANISM_INTERACTION_BARRIER", &barrier)
            .output()
            .unwrap();
        eprintln!(
            "{}{}",
            String::from_utf8_lossy(&recovery.stdout),
            String::from_utf8_lossy(&recovery.stderr)
        );
        eprintln!(
            "MECHANISM_RA_C01_PROCESS {}",
            serde_json::json!({"kind":kind,"phase":"recover","actualExitCode":recovery.status.code()})
        );
        assert!(
            recovery.status.success(),
            "ordinary resolved {kind} response must reach the restored original waiter after crash"
        );
    }
}

async fn paused_response(root: &Fixture, port: &RecordedPort, kind: &str, id: &str) {
    let barrier = root.0.join("mechanism-barrier");
    for _ in 0..2_000 {
        if barrier.join(format!("{kind}.dispatch.paused")).exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    assert!(
        barrier.join(format!("{kind}.dispatch.paused")).exists(),
        "actual Core submission dispatch hit the test-only fault barrier"
    );
    assert!(
        !barrier.join(format!("{kind}.waiter.consumed")).exists(),
        "the actual oneshot waiter has not consumed the ordinary response"
    );
    let table = if kind == "input" {
        "input_operation"
    } else {
        "approval_operation"
    };
    let column = if kind == "input" {
        "input_request_id"
    } else {
        "approval_id"
    };
    let (state, digest): (String, Option<String>) = core_database(root)
        .query_row(
            &format!("SELECT state,resolution_digest FROM {table} WHERE {column}=?1"),
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let proofs: i64 = core_database(root)
        .query_row(
            "SELECT COUNT(*) FROM execution_response_receipt WHERE family=?1",
            [if kind == "input" {
                "input_request"
            } else {
                "approval_request"
            }],
            |row| row.get(0),
        )
        .unwrap();
    let queued = fs::read_to_string(barrier.join(format!("{kind}.dispatch.paused"))).unwrap();
    eprintln!(
        "MECHANISM_RA_C01_BOUNDARY {}",
        serde_json::json!({"kind":kind,"state":state,"resolutionDigestPresent":digest.is_some(),"receiptCount":proofs,"queueAcceptedRealOp":queued,"actualWaiterConsumed":false,"modelOpenCount":port.messages().iter().filter(|message|matches!(message,wire::ExecutionPortMessage::ModelOpenMessage(_))).count()})
    );
    assert_eq!(state, "resolved");
    assert!(digest.is_some());
    assert_eq!(proofs, 1);
}

fn deny_at(approval: &wire::ApprovalRequestMessage) -> wire::ExecutionPortMessage {
    let mut response = late_approval(approval);
    if let wire::ExecutionPortMessage::ApprovalDecisionMessage(value) = &mut response {
        value.decided_at = at("00:01:00");
        value.sent_at = at("00:01:00");
        value.decision = wire::ApprovalDecisionMessageDecision::Denied;
    }
    response
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedOrdinaryResponse {
    dispatch: wire::JobDispatchMessage,
    response: wire::ExecutionPortMessage,
    input: Option<wire::InputRequestMessage>,
    approval: Option<wire::ApprovalRequestMessage>,
    marker: Option<PathBuf>,
}

#[allow(
    clippy::too_many_lines,
    reason = "保持崩溃恢复实际控制路径的单fixture及原断言"
)]
async fn recover_response(root: &Fixture, kind: &str) {
    let mut saved: SavedOrdinaryResponse =
        serde_json::from_slice(&fs::read(root.0.join("saved-ordinary-response.json")).unwrap())
            .unwrap();
    saved.dispatch.message_id = message_id(91_000);
    saved.dispatch.request_id = domain::RequestId(fixture_id("req", 91_000));
    saved.dispatch.sent_at = at("00:02:00");
    let config = worker_config(&saved.dispatch);
    let port = RecordedPort::default();
    let mut worker = winwincode_worker::WorkerMain::new(
        config.clone(),
        port.clone(),
        root.adapter_with_owner(&config, true),
        root.workspace_runtime(),
    );
    register_at(&mut worker, &port, &config, at("00:02:00")).await;
    worker
        .accept_control(
            &wire::ExecutionPortMessage::JobDispatchMessage(saved.dispatch.clone()),
            at("00:02:00"),
        )
        .await
        .unwrap();
    worker.flush_durable_outbox().await.unwrap();
    assert_dispatch_accepted(&port);
    let replay = worker.accept_control(&saved.response, at("00:02:00")).await;
    eprintln!(
        "MECHANISM_RA_C01_REPLAY {}",
        serde_json::json!({"kind":kind,"sameResponseAccepted":replay.is_ok(),"error":format!("{replay:?}"),"actualWaiterConsumed":root.0.join(format!("mechanism-barrier/{kind}.waiter.consumed")).exists()})
    );
    replay.expect(
        "the exact durable response replay is accepted while the original deadline remains valid",
    );
    let open = wait_open_at(&mut worker, &port, 0, at("00:02:00")).await;
    if kind == "input" {
        request_input_at(&mut worker, &open, "input-expiry-call", 1, at("00:02:01")).await;
    } else {
        request_escalated_shell_at(
            &mut worker,
            &open,
            saved.marker.as_ref().unwrap(),
            1,
            at("00:02:01"),
        )
        .await;
    }
    let mut continuation = None;
    for _ in 0..300 {
        Box::pin(worker.poll_codex(at("00:02:01"))).await.unwrap();
        continuation = port
            .messages()
            .into_iter()
            .filter_map(|message| match message {
                wire::ExecutionPortMessage::ModelOpenMessage(open) => Some(open),
                _ => None,
            })
            .nth(1);
        if continuation.is_some()
            || port
                .messages()
                .iter()
                .any(|message| matches!(message, wire::ExecutionPortMessage::JobOutcomeMessage(_)))
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let outcomes = port
        .messages()
        .into_iter()
        .filter_map(|message| match message {
            wire::ExecutionPortMessage::JobOutcomeMessage(outcome) => Some(outcome),
            _ => None,
        })
        .collect::<Vec<_>>();
    let public_kinds = port
        .messages()
        .into_iter()
        .map(|message| serde_json::to_value(message).unwrap()["kind"].clone())
        .collect::<Vec<_>>();
    let recovery_tool_output = continuation.as_ref().and_then(|open| {
        let body: serde_json::Value =
            serde_json::from_slice(&STANDARD.decode(&open.request.data_base64).unwrap()).unwrap();
        tool_output(
            &body,
            if kind == "input" {
                "input-expiry-call"
            } else {
                "approval-expiry-call"
            },
        )
    });
    let event_path = root
        .0
        .join(format!("mechanism-barrier/{kind}.request.event.json"));
    let saved_time = fs::metadata(root.0.join("saved-ordinary-response.json"))
        .unwrap()
        .modified()
        .unwrap();
    let event_newer_than_saved = fs::metadata(&event_path)
        .and_then(|metadata| metadata.modified())
        .is_ok_and(|event_time| event_time > saved_time);
    let replacement_event = event_newer_than_saved.then(|| {
        serde_json::from_slice::<serde_json::Value>(&fs::read(&event_path).unwrap()).unwrap()
    });
    eprintln!(
        "MECHANISM_RA_C01_RECOVERY {}",
        serde_json::json!({"kind":kind,"continuation":continuation.is_some(),"outcomes":outcomes,"actualWaiterConsumed":root.0.join(format!("mechanism-barrier/{kind}.waiter.consumed")).exists(),"recoveryToolOutput":recovery_tool_output,"requestEventNewerThanSavedResponse":event_newer_than_saved,"replacementActualRequestEvent":replacement_event,"publicKinds":public_kinds})
    );
    let continuation = continuation.expect(
        "the original ordinary response survives the resolved-before-consumed process crash",
    );
    let body: serde_json::Value =
        serde_json::from_slice(&STANDARD.decode(&continuation.request.data_base64).unwrap())
            .unwrap();
    let output = tool_output(
        &body,
        if kind == "input" {
            "input-expiry-call"
        } else {
            "approval-expiry-call"
        },
    )
    .unwrap();
    if kind == "input" {
        assert!(output.to_string().contains("continue"));
    } else {
        assert!(is_rejected_tool_result(&body));
        assert!(!saved.marker.as_ref().unwrap().exists());
    }
    assert!(
        root.0
            .join(format!("mechanism-barrier/{kind}.waiter.consumed"))
            .exists()
    );
    Box::pin(worker.shutdown(at("00:02:02"))).await.unwrap();
}

#[allow(
    clippy::too_many_lines,
    reason = "保持真实响应屏障与原waiter消费边界的单fixture路径及原断言"
)]
fn ordinary_boundary(exact: &str, kind: &str, crash: bool) {
    if std::env::var_os("WWC_MECHANISM_RESPONSE_CHILD").is_none() {
        isolated_case(exact, kind, crash);
        return;
    }
    let kind = kind.to_owned();
    run_native(async move {
        let root = Fixture::new();
        if std::env::var("WWC_MECHANISM_RESPONSE_CHILD").unwrap() == "recover" {
            recover_response(&root, &kind).await;
            return;
        }
        let (mut worker, port, saved) = if kind == "input" {
            let (worker, port, dispatch, input) = start_pending_input(&root).await;
            let response = input_response_at(&input, at("00:01:00"));
            (
                worker,
                port,
                SavedOrdinaryResponse {
                    dispatch,
                    response,
                    input: Some(input),
                    approval: None,
                    marker: None,
                },
            )
        } else {
            let pending = start_pending_shell(&root).await;
            let response = deny_at(&pending.approval);
            (
                pending.worker,
                pending.port,
                SavedOrdinaryResponse {
                    dispatch: pending.dispatch,
                    response,
                    input: None,
                    approval: Some(pending.approval),
                    marker: Some(pending.marker),
                },
            )
        };
        fs::write(
            root.0.join("saved-ordinary-response.json"),
            serde_json::to_vec(&saved).unwrap(),
        )
        .unwrap();
        worker
            .accept_control(&saved.response, at("00:01:00"))
            .await
            .unwrap();
        let id = if let Some(input) = &saved.input {
            &input.input_request_id.0
        } else {
            &saved.approval.as_ref().unwrap().approval_id.0
        };
        paused_response(&root, &port, &kind, id).await;
        if crash {
            std::process::exit(73);
        }
        fs::write(
            root.0
                .join(format!("mechanism-barrier/{kind}.dispatch.release")),
            b"release",
        )
        .unwrap();
        let continuation = wait_open_at(&mut worker, &port, 1, at("00:01:00")).await;
        let body: serde_json::Value =
            serde_json::from_slice(&STANDARD.decode(&continuation.request.data_base64).unwrap())
                .unwrap();
        let output = tool_output(
            &body,
            if kind == "input" {
                "input-expiry-call"
            } else {
                "approval-expiry-call"
            },
        )
        .expect("original actual Core waiter returned");
        if kind == "input" {
            assert!(output.to_string().contains("continue"));
        } else {
            assert!(is_rejected_tool_result(&body));
            assert!(!saved.marker.as_ref().unwrap().exists());
        }
        assert!(
            root.0
                .join(format!("mechanism-barrier/{kind}.waiter.consumed"))
                .exists()
        );
        worker
            .accept_control(&saved.response, at("00:01:01"))
            .await
            .unwrap();
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
        eprintln!(
            "MECHANISM_RA_C01_CONSUMED {}",
            serde_json::json!({"kind":kind,"actualWaiterConsumed":true,"exactResponseReplayAccepted":true,"originalToolOutput":output})
        );
        Box::pin(worker.shutdown(at("00:01:02"))).await.unwrap();
    });
}

#[test]
#[ignore = "mechanism audit: long subprocess control for resolved input before waiter consumption"]
fn ra_c01_input_resolved_precedes_real_waiter_consumption() {
    ordinary_boundary(
        "mechanism_response_boundary::ra_c01_input_resolved_precedes_real_waiter_consumption",
        "input",
        false,
    );
}
#[test]
#[ignore = "mechanism audit: long subprocess control for resolved approval before waiter consumption"]
fn ra_c01_approval_resolved_precedes_real_waiter_consumption() {
    ordinary_boundary(
        "mechanism_response_boundary::ra_c01_approval_resolved_precedes_real_waiter_consumption",
        "approval",
        false,
    );
}
#[test]
#[ignore = "mechanism audit: conditional red; replacement input waiter was not reconstructed"]
fn ra_c01_input_resolved_before_consumption_survives_crash() {
    ordinary_boundary(
        "mechanism_response_boundary::ra_c01_input_resolved_before_consumption_survives_crash",
        "input",
        true,
    );
}
#[test]
#[ignore = "mechanism audit: conditional red; replacement approval waiter was not reconstructed"]
fn ra_c01_approval_resolved_before_consumption_survives_crash() {
    ordinary_boundary(
        "mechanism_response_boundary::ra_c01_approval_resolved_before_consumption_survives_crash",
        "approval",
        true,
    );
}

#[test]
#[ignore = "mechanism audit: conditional red; early approval dropped before replacement waiter"]
#[allow(
    clippy::too_many_lines,
    reason = "保持早到审批响应与崩溃恢复真实waiter边界的单fixture路径及原断言"
)]
fn ra_c01_pending_approval_answer_before_restored_waiter_is_retained() {
    const EXACT: &str = "mechanism_response_boundary::ra_c01_pending_approval_answer_before_restored_waiter_is_retained";
    if std::env::var_os("WWC_MECHANISM_PENDING_APPROVAL_PHASE").is_none() {
        let root = Fixture::new();
        let barrier = root.0.join("mechanism-barrier");
        fs::create_dir_all(&barrier).unwrap();
        let prepare = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                EXACT,
                "--include-ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("WWC_MECHANISM_PENDING_APPROVAL_PHASE", "prepare")
            .env("WWC_APPROVAL_RESTART_DIRECTORY", &root.0)
            .env("WWC_MECHANISM_INTERACTION_BARRIER", &barrier)
            .output()
            .unwrap();
        eprintln!(
            "{}{}",
            String::from_utf8_lossy(&prepare.stdout),
            String::from_utf8_lossy(&prepare.stderr)
        );
        eprintln!(
            "MECHANISM_RA_C01_PROCESS {}",
            serde_json::json!({"kind":"approval","scenario":"pending_before_answer","phase":"prepare","actualExitCode":prepare.status.code()})
        );
        assert_eq!(prepare.status.code(), Some(73));
        let recovery = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                EXACT,
                "--include-ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("WWC_MECHANISM_PENDING_APPROVAL_PHASE", "recover")
            .env("WWC_APPROVAL_RESTART_DIRECTORY", &root.0)
            .env("WWC_MECHANISM_INTERACTION_BARRIER", &barrier)
            .output()
            .unwrap();
        eprintln!(
            "{}{}",
            String::from_utf8_lossy(&recovery.stdout),
            String::from_utf8_lossy(&recovery.stderr)
        );
        eprintln!(
            "MECHANISM_RA_C01_PROCESS {}",
            serde_json::json!({"kind":"approval","scenario":"pending_before_answer","phase":"recover","actualExitCode":recovery.status.code()})
        );
        assert!(
            recovery.status.success(),
            "early valid approval must survive until the restored exact Core waiter"
        );
        return;
    }
    run_native(async {
        let root = Fixture::new();
        if std::env::var("WWC_MECHANISM_PENDING_APPROVAL_PHASE").unwrap() == "prepare" {
            let pending = start_pending_shell(&root).await;
            let saved = SavedOrdinaryResponse {
                dispatch: pending.dispatch,
                response: deny_at(&pending.approval),
                input: None,
                approval: Some(pending.approval),
                marker: Some(pending.marker),
            };
            fs::write(
                root.0.join("saved-ordinary-response.json"),
                serde_json::to_vec(&saved).unwrap(),
            )
            .unwrap();
            std::process::exit(73);
        }
        let mut saved: SavedOrdinaryResponse =
            serde_json::from_slice(&fs::read(root.0.join("saved-ordinary-response.json")).unwrap())
                .unwrap();
        saved.dispatch.message_id = message_id(92_000);
        saved.dispatch.request_id = domain::RequestId(fixture_id("req", 92_000));
        saved.dispatch.sent_at = at("00:02:00");
        let config = worker_config(&saved.dispatch);
        let port = RecordedPort::default();
        let mut worker = winwincode_worker::WorkerMain::new(
            config.clone(),
            port.clone(),
            root.adapter_with_owner(&config, true),
            root.workspace_runtime(),
        );
        register_at(&mut worker, &port, &config, at("00:02:00")).await;
        worker
            .accept_control(
                &wire::ExecutionPortMessage::JobDispatchMessage(saved.dispatch.clone()),
                at("00:02:00"),
            )
            .await
            .unwrap();
        worker.flush_durable_outbox().await.unwrap();
        assert_dispatch_accepted(&port);
        worker
            .accept_control(&saved.response, at("00:02:00"))
            .await
            .unwrap();
        let dropped = root.0.join("mechanism-barrier/approval.notify.dropped");
        for _ in 0..2_000 {
            if dropped.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(
            dropped.exists(),
            "real Core notify_approval processed the restored valid response before an exact waiter existed"
        );
        let state: String = core_database(&root)
            .query_row(
                "SELECT state FROM approval_operation WHERE approval_id=?1",
                [&saved.approval.as_ref().unwrap().approval_id.0],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "resolved");
        // Remove the first process's request witness so only the replacement Core request qualifies.
        fs::remove_file(root.0.join("mechanism-barrier/approval.request.event.json")).unwrap();
        let open = wait_open_at(&mut worker, &port, 0, at("00:02:00")).await;
        request_escalated_shell_at(
            &mut worker,
            &open,
            saved.marker.as_ref().unwrap(),
            1,
            at("00:02:01"),
        )
        .await;
        let mut continuation = None;
        for _ in 0..300 {
            Box::pin(worker.poll_codex(at("00:02:01"))).await.unwrap();
            continuation = port
                .messages()
                .into_iter()
                .filter_map(|message| match message {
                    wire::ExecutionPortMessage::ModelOpenMessage(open) => Some(open),
                    _ => None,
                })
                .nth(1);
            if continuation.is_some()
                || port.messages().iter().any(|message| {
                    matches!(message, wire::ExecutionPortMessage::JobOutcomeMessage(_))
                })
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let restored_event = fs::read(root.0.join("mechanism-barrier/approval.request.event.json"))
            .ok()
            .map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).unwrap());
        let replay = worker.accept_control(&saved.response, at("00:02:01")).await;
        eprintln!(
            "MECHANISM_RA_C01_EARLY_APPROVAL {}",
            serde_json::json!({"responseAcceptedBeforeWaiter":true,"coreNotifyDropped":true,"adapterState":state,"restoredActualRequestEvent":restored_event,"exactReplayAccepted":replay.is_ok(),"continuation":continuation.is_some(),"actualWaiterConsumed":root.0.join("mechanism-barrier/approval.waiter.consumed").exists()})
        );
        replay.unwrap();
        assert!(
            restored_event.is_some(),
            "actual replacement Core emitted the exact approval request"
        );
        assert!(
            continuation.is_some(),
            "early valid ordinary approval must settle its later restored waiter"
        );
        assert!(!saved.marker.as_ref().unwrap().exists());
        Box::pin(worker.shutdown(at("00:02:02"))).await.unwrap();
    });
}
