// SPDX-License-Identifier: Apache-2.0

//! Exercise diagnostics through production event consumption and durable Worker
//! outcomes. Faults enter before classification; no test constructs a terminal.

use super::*;
use winwincode_codex::ProductionEventPollFault;

#[test]
fn z7xn_closed_event_stream_retains_safe_cause_across_restart() {
    run_on_large_stack(assert_retained_failure(
        "closed",
        ProductionEventPollFault::Closed,
        ExecutionOutcomeStatus::InfrastructureError,
        ExecutionPortErrorCode::InfrastructureError,
        true,
        "embedded Codex infrastructure failed [stage=event_poll; cause=EVENT_STREAM_CLOSED]",
        2,
    ));
}

#[test]
fn z7xn_malformed_core_event_retains_safe_stage_across_restart() {
    run_on_large_stack(assert_retained_failure(
        "malformed",
        ProductionEventPollFault::MalformedEvent,
        ExecutionOutcomeStatus::InfrastructureError,
        ExecutionPortErrorCode::InfrastructureError,
        true,
        "embedded Codex infrastructure failed [stage=event_decode; cause=EVENT_DECODE_FAILED]",
        2,
    ));
}

#[test]
fn z7xn_real_kernel_closed_failure_survives_unavailable_fallback_and_restart() {
    run_on_large_stack(assert_shutdown_then_recover(false));
}

#[test]
fn z7xn_closed_kernel_submission_preserves_diagnostic_until_real_final_cut_recovery() {
    run_on_large_stack(assert_shutdown_then_recover(true));
}

#[test]
fn z7xn_closed_kernel_session_admission_rejects_with_safe_typed_classification() {
    run_on_large_stack(async {
        let root = TestDirectory::new("z7xn-admission-kernel-closed");
        let dispatch = dispatch(&root);
        let port = RecordedPort::default();
        let adapter = winwincode_codex::ProductionCodexAdapter::open(
            adapter_config(&root).with_test_closed_kernel_before_session_start(),
        )
        .unwrap();
        let mut worker = winwincode_worker::WorkerMain::new(
            worker_config(),
            port.clone(),
            adapter,
            root.workspace_runtime(),
        );
        register(&mut worker, &port).await;
        worker
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
                at("2030-01-01T00:00:00.000Z"),
            )
            .await
            .unwrap();
        worker
            .poll_codex(at("2030-01-01T00:00:02.000Z"))
            .await
            .unwrap();
        let messages = port.messages();
        assert_no_model_request(&messages);
        assert!(
            !messages
                .iter()
                .any(|message| matches!(message, ExecutionPortMessage::JobOutcomeMessage(_)))
        );
        let rejection = messages
            .iter()
            .find_map(|message| match message {
                ExecutionPortMessage::JobDispatchResultMessage(result) => Some(result),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            rejection.status,
            JobDispatchResultMessageStatus::RejectedCapability
        );
        assert_eq!(rejection.lease, dispatch.lease);
        let error = rejection.error.as_ref().unwrap();
        assert_eq!(error.code, ExecutionPortErrorCode::InfrastructureError);
        assert!(error.retryable);
        assert_eq!(
            error.message,
            "embedded Codex Core is unavailable [stage=session_create; cause=KERNEL_CLOSED]"
        );
        let serialized = serde_json::to_vec(rejection).unwrap();
        let decoded: winwincode_execution_port::generated::JobDispatchResultMessage =
            serde_json::from_slice(&serialized).unwrap();
        assert_eq!(decoded, *rejection);
        assert_secret_safe(&messages, &serde_json::Value::Null);
        assert!(worker.active_jobs().is_empty());
        worker
            .shutdown(at("2030-01-01T00:00:03.000Z"))
            .await
            .unwrap();
        drop(worker);
        let reopened =
            winwincode_codex::ProductionCodexAdapter::open(adapter_config(&root)).unwrap();
        drop(reopened);
        // Dispatch rejection is a response-free transport fact. A successful
        // send removes it from pending(), but its canonical bytes stay durable.
        let connection =
            rusqlite::Connection::open(root.worker().join("worker-codex.sqlite3")).unwrap();
        let (digest, bytes): (String, Vec<u8>) = connection
            .query_row(
                "SELECT frame_digest, frame_json FROM execution_outbox WHERE delivery_id = ?1",
                rusqlite::params![rejection.message_id.0],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        drop(connection);
        assert_eq!(digest, format!("sha256:{:x}", Sha256::digest(&bytes)));
        assert_eq!(
            serde_json::from_slice::<ExecutionPortMessage>(&bytes).unwrap(),
            ExecutionPortMessage::JobDispatchResultMessage(decoded)
        );
    });
}

async fn assert_shutdown_then_recover(submission: bool) {
    let label = if submission {
        "submit-kernel-closed"
    } else {
        "poll-kernel-closed"
    };
    let stage = if submission {
        "turn_submit"
    } else {
        "event_poll"
    };
    let root = TestDirectory::new(&format!("z7xn-{label}"));
    let dispatch = dispatch(&root);
    let port = RecordedPort::default();
    let config = if submission {
        adapter_config(&root).with_test_submission_fault(
            winwincode_codex::ProductionSubmissionFault::KernelClosedBeforeKernel,
        )
    } else {
        adapter_config(&root).with_test_event_poll_fault(ProductionEventPollFault::KernelClosed)
    };
    let adapter = winwincode_codex::ProductionCodexAdapter::open(config).unwrap();
    let mut worker = winwincode_worker::WorkerMain::new(
        worker_config(),
        port.clone(),
        adapter,
        root.workspace_runtime(),
    );
    register(&mut worker, &port).await;
    let initial = worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
            at("2030-01-01T00:00:00.000Z"),
        )
        .await;
    let mut failed = initial.is_err();
    for _ in 0..40 {
        if failed {
            break;
        }
        failed = worker
            .poll_codex(at("2030-01-01T00:00:02.000Z"))
            .await
            .is_err();
        if !failed {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    assert!(
        failed,
        "a globally closed Kernel cannot provide a final receipt cut"
    );
    let messages = port.messages();
    assert_no_model_request(&messages);
    assert!(
        !messages
            .iter()
            .any(|message| matches!(message, ExecutionPortMessage::JobOutcomeMessage(_))),
        "failed final receipt closure must not invent a terminal frame"
    );
    let stored = stored_run_json(&root);
    assert_eq!(
        stored["failureDiagnostic"],
        serde_json::json!({"stage":stage,"cause":"KERNEL_CLOSED"})
    );
    assert!(stored["coreToolFinalCursor"].is_null());
    assert_secret_safe(&messages, &stored);
    // Drop the globally closed owner. The new owner reads the same Core DB;
    // no test writes a cursor, terminal, or tool fact into it.
    drop(worker);
    let recovered_port = RecordedPort::default();
    let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config(&root)).unwrap();
    let mut recovered = winwincode_worker::WorkerMain::new(
        worker_config(),
        recovered_port.clone(),
        adapter,
        root.workspace_runtime(),
    );
    register(&mut recovered, &recovered_port).await;
    recovered
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
            at("2030-01-01T00:00:00.000Z"),
        )
        .await
        .unwrap();
    run_until_outcome_without_gateway(&mut recovered, &recovered_port).await;
    let messages = recovered_port.messages();
    assert_no_model_request(&messages);
    let facts = unique_terminal_facts(&messages);
    let outcome = facts
        .iter()
        .find_map(|message| match message {
            ExecutionPortMessage::JobOutcomeMessage(value) => Some(value),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        outcome.outcome.status,
        ExecutionOutcomeStatus::InfrastructureError
    );
    let error = outcome.outcome.error.as_ref().unwrap();
    assert_eq!(error.code, ExecutionPortErrorCode::InfrastructureError);
    assert!(error.retryable);
    assert_eq!(
        error.message,
        format!("embedded Codex infrastructure failed [stage={stage}; cause=KERNEL_CLOSED]")
    );
    assert_eq!(outcome.lease, dispatch.lease);
    assert!(outcome.outcome.finished_at.0 < dispatch.lease.expires_at.0);
    assert_canonical_export(&root, outcome);
    let stored = stored_run_json(&root);
    assert_eq!(
        stored["failureDiagnostic"],
        serde_json::json!({"stage":stage,"cause":"KERNEL_CLOSED"})
    );
    assert!(!stored["coreToolFinalCursor"].is_null());
    assert_eq!(stored["coreToolFinalCursor"], stored["coreToolCursor"]);
    assert!(stored["coreToolPending"].is_null());
    assert_eq!(stored["phase"], "outcome_retained");
    assert_secret_safe(&messages, &stored);
    assert_control_plane_retains_failure(&root, &dispatch, &messages);
    recovered
        .shutdown(at("2030-01-01T00:00:03.000Z"))
        .await
        .unwrap();
    drop(recovered);
    let replay_port = RecordedPort::default();
    let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config(&root)).unwrap();
    let mut replay = winwincode_worker::WorkerMain::new(
        worker_config(),
        replay_port.clone(),
        adapter,
        root.workspace_runtime(),
    );
    register(&mut replay, &replay_port).await;
    replay
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch),
            at("2030-01-01T00:00:00.000Z"),
        )
        .await
        .unwrap();
    run_until_outcome_without_gateway(&mut replay, &replay_port).await;
    assert_no_model_request(&replay_port.messages());
    assert_eq!(unique_terminal_facts(&replay_port.messages()), facts);
    replay
        .shutdown(at("2030-01-01T00:00:03.000Z"))
        .await
        .unwrap();
}

#[test]
fn z7xn_typed_core_provider_error_is_failed_and_secret_safe_after_restart() {
    run_on_large_stack(assert_retained_failure(
        "core-provider",
        ProductionEventPollFault::CoreError(
            codex_protocol::protocol::CodexErrorInfo::ResponseStreamDisconnected {
                http_status_code: Some(503),
            },
        ),
        ExecutionOutcomeStatus::Failed,
        ExecutionPortErrorCode::ExecutionFailed,
        false,
        "embedded Codex execution failed [stage=core_event; cause=CORE_RESPONSE_STREAM_DISCONNECTED]",
        2,
    ));
}

#[test]
fn z7xn_explicit_cancel_does_not_become_a_pending_event_failure() {
    run_on_large_stack(async {
        let root = TestDirectory::new("z7xn-active-cancel");
        let dispatch = dispatch(&root);
        let port = RecordedPort::default();
        // The cancellation terminal must take precedence over a future stream
        // failure; its retained frame must never acquire that failure's cause.
        let adapter = winwincode_codex::ProductionCodexAdapter::open(
            adapter_config(&root).with_test_event_poll_fault(ProductionEventPollFault::Closed),
        )
        .unwrap();
        let mut worker = winwincode_worker::WorkerMain::new(
            worker_config(),
            port.clone(),
            adapter,
            root.workspace_runtime(),
        );
        register(&mut worker, &port).await;
        worker
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
                at("2030-01-01T00:00:00.000Z"),
            )
            .await
            .unwrap();
        let active = worker.active_jobs()[0].clone();
        let cancelled_at = at("2030-01-01T00:00:01.000Z");
        worker
            .accept_control(
                &ExecutionPortMessage::JobCancelMessage(JobCancelMessage {
                    kind: JobCancelMessageKind::JobCancel,
                    lease: active.lease.clone(),
                    message_id: ExecutionMessageId(id("xmsg", 606)),
                    reason: JobCancelMessageReason::UserRequested,
                    requested_at: cancelled_at.clone(),
                    request_id: RequestId(id("req", 606)),
                    schema_version: SchemaVersion::WinwincodeV1,
                    sent_at: cancelled_at.clone(),
                    session_identity: active.session_identity.clone(),
                    worker_session_id: active.worker_session_id.clone(),
                }),
                cancelled_at,
            )
            .await
            .unwrap();
        run_until_outcome_without_gateway(&mut worker, &port).await;
        let messages = port.messages();
        assert_no_model_request(&messages);
        let facts = unique_terminal_facts(&messages);
        let outcome = facts
            .iter()
            .find_map(|message| match message {
                ExecutionPortMessage::JobOutcomeMessage(outcome) => Some(outcome),
                _ => None,
            })
            .unwrap();
        assert_eq!(outcome.outcome.status, ExecutionOutcomeStatus::Cancelled);
        assert_eq!(outcome.outcome.summary, "embedded Codex turn cancelled");
        let error = outcome.outcome.error.as_ref().unwrap();
        assert_eq!(error.code, ExecutionPortErrorCode::Cancelled);
        assert!(!error.retryable);
        assert_eq!(error.message, "embedded Codex turn cancelled");
        assert!(outcome.outcome.finished_at.0 < dispatch.lease.expires_at.0);
        assert_eq!(outcome.outcome.last_event_sequence.0, 2);
        assert_eq!(facts.len(), 3);
        assert_canonical_export(&root, outcome);
        let stored = stored_run_json(&root);
        assert!(stored["failureDiagnostic"].is_null());
        assert_secret_safe(&messages, &stored);
        assert_control_plane_retains_failure(&root, &dispatch, &messages);
        worker
            .shutdown(at("2030-01-01T00:00:03.000Z"))
            .await
            .unwrap();
        drop(worker);
        let replay_port = RecordedPort::default();
        let adapter =
            winwincode_codex::ProductionCodexAdapter::open(adapter_config(&root)).unwrap();
        let mut replay = winwincode_worker::WorkerMain::new(
            worker_config(),
            replay_port.clone(),
            adapter,
            root.workspace_runtime(),
        );
        register(&mut replay, &replay_port).await;
        replay
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch),
                at("2030-01-01T00:00:00.000Z"),
            )
            .await
            .unwrap();
        run_until_outcome_without_gateway(&mut replay, &replay_port).await;
        assert_eq!(unique_terminal_facts(&replay_port.messages()), facts);
        assert_no_model_request(&replay_port.messages());
        replay
            .shutdown(at("2030-01-01T00:00:03.000Z"))
            .await
            .unwrap();
    });
}

async fn assert_retained_failure(
    label: &'static str,
    fault: ProductionEventPollFault,
    status: ExecutionOutcomeStatus,
    code: ExecutionPortErrorCode,
    retryable: bool,
    expected_message: &'static str,
    expected_last_sequence: i64,
) {
    let root = TestDirectory::new(&format!("z7xn-{label}"));
    let dispatch = dispatch(&root);
    let port = RecordedPort::default();
    let adapter = winwincode_codex::ProductionCodexAdapter::open(
        adapter_config(&root).with_test_event_poll_fault(fault),
    )
    .expect("open production failure fixture");
    let mut worker = winwincode_worker::WorkerMain::new(
        worker_config(),
        port.clone(),
        adapter,
        root.workspace_runtime(),
    );
    register(&mut worker, &port).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
            at("2030-01-01T00:00:00.000Z"),
        )
        .await
        .expect("accept real production failure dispatch");
    run_until_outcome_without_gateway(&mut worker, &port).await;
    let messages = port.messages();
    assert_no_model_request(&messages);
    let facts = unique_terminal_facts(&messages);
    let outcome = facts
        .iter()
        .find_map(|message| match message {
            ExecutionPortMessage::JobOutcomeMessage(outcome) => Some(outcome),
            _ => None,
        })
        .expect("production Worker retained terminal outcome");
    assert_eq!(outcome.outcome.status, status);
    let error = outcome
        .outcome
        .error
        .as_ref()
        .expect("typed terminal error");
    assert_eq!(error.code, code);
    assert_eq!(error.retryable, retryable);
    assert_eq!(error.message, expected_message);
    // The event failure occurs at two seconds with five minutes of authority.
    // It must not become a lease-expiry or cancellation diagnosis.
    assert_eq!(outcome.lease, dispatch.lease);
    assert!(outcome.outcome.finished_at.0 < dispatch.lease.expires_at.0);
    assert_eq!(outcome.outcome.finished_at, at("2030-01-01T00:00:02.000Z"));
    assert_eq!(
        outcome.outcome.last_event_sequence.0,
        expected_last_sequence
    );
    assert_eq!(
        facts.len(),
        usize::try_from(expected_last_sequence).unwrap() + 1
    );
    assert_eq!(stored_run_json(&root)["phase"], "outcome_retained");
    let encoded = serde_json::to_vec(outcome).expect("serialize terminal diagnostic");
    let decoded: winwincode_execution_port::generated::JobOutcomeMessage =
        serde_json::from_slice(&encoded).expect("deserialize terminal diagnostic");
    assert_eq!(decoded, *outcome);
    assert_canonical_export(&root, outcome);
    assert_secret_safe(&messages, &stored_run_json(&root));
    assert_control_plane_retains_failure(&root, &dispatch, &messages);

    worker
        .shutdown(at("2030-01-01T00:00:03.000Z"))
        .await
        .expect("quiesce first production Worker");
    drop(worker);
    let replay_port = RecordedPort::default();
    let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config(&root))
        .expect("reopen durable production adapter without injected fault");
    let mut replay = winwincode_worker::WorkerMain::new(
        worker_config(),
        replay_port.clone(),
        adapter,
        root.workspace_runtime(),
    );
    register(&mut replay, &replay_port).await;
    replay
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch),
            at("2030-01-01T00:00:00.000Z"),
        )
        .await
        .expect("replay dispatch after process restart");
    run_until_outcome_without_gateway(&mut replay, &replay_port).await;
    let replayed = replay_port.messages();
    assert_no_model_request(&replayed);
    assert_eq!(unique_terminal_facts(&replayed), facts);
    assert_secret_safe(&replayed, &stored_run_json(&root));
    replay
        .shutdown(at("2030-01-01T00:00:03.000Z"))
        .await
        .expect("quiesce replay Worker");
    drop(replay);
    assert_legacy_snapshot_replays_canonical_terminal(&root, &facts).await;
}

async fn assert_legacy_snapshot_replays_canonical_terminal(
    root: &TestDirectory,
    expected: &[ExecutionPortMessage],
) {
    // Simulate the exact pre-upgrade record shape. Only the newly optional
    // classification field is removed; receipts and cursors are untouched.
    let connection =
        rusqlite::Connection::open(root.worker().join("worker-codex.sqlite3")).unwrap();
    let (key, bytes): (String, Vec<u8>) = connection
        .query_row("SELECT run_key, record_json FROM codex_run", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    let mut legacy: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        legacy
            .as_object_mut()
            .unwrap()
            .remove("failureDiagnostic")
            .is_some()
    );
    connection
        .execute(
            "UPDATE codex_run SET record_json = ?1 WHERE run_key = ?2",
            rusqlite::params![serde_json::to_vec(&legacy).unwrap(), key],
        )
        .unwrap();
    drop(connection);
    let port = RecordedPort::default();
    let adapter = winwincode_codex::ProductionCodexAdapter::open(adapter_config(root)).unwrap();
    let mut worker = winwincode_worker::WorkerMain::new(
        worker_config(),
        port.clone(),
        adapter,
        root.workspace_runtime(),
    );
    register(&mut worker, &port).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch(root)),
            at("2030-01-01T00:00:00.000Z"),
        )
        .await
        .unwrap();
    run_until_outcome_without_gateway(&mut worker, &port).await;
    assert_no_model_request(&port.messages());
    assert_eq!(unique_terminal_facts(&port.messages()), expected);
    worker
        .shutdown(at("2030-01-01T00:00:03.000Z"))
        .await
        .unwrap();
}

fn assert_canonical_export(
    root: &TestDirectory,
    outcome: &winwincode_execution_port::generated::JobOutcomeMessage,
) {
    // This is the canonical Host frame read by collectCoreFacts in the real
    // batch runner. It is separate from the Delivery's status-only projection.
    let connection = rusqlite::Connection::open(root.worker().join("worker-codex.sqlite3"))
        .expect("open canonical outcome export");
    let (digest, bytes): (String, Vec<u8>) = connection
        .query_row(
            "SELECT frame_digest, frame_json FROM execution_terminal_outcome",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("load production canonical terminal frame");
    drop(connection);
    assert_eq!(digest, format!("sha256:{:x}", Sha256::digest(&bytes)));
    let stored: ExecutionPortMessage =
        serde_json::from_slice(&bytes).expect("decode original canonical terminal");
    assert_eq!(
        stored,
        ExecutionPortMessage::JobOutcomeMessage(outcome.clone())
    );
}

fn assert_secret_safe(messages: &[ExecutionPortMessage], stored: &serde_json::Value) {
    let messages = serde_json::to_string(messages).unwrap();
    let stored = serde_json::to_string(stored).unwrap();
    for secret in [
        "z7xn-private-sentinel",
        "do-not-publish",
        "/private/z7xn-tool-input",
    ] {
        assert!(
            !messages.contains(secret),
            "outbound frames leaked a private error"
        );
        assert!(!stored.contains(secret), "StoredRun leaked a private error");
    }
}

fn assert_control_plane_retains_failure(
    root: &TestDirectory,
    dispatch: &JobDispatchMessage,
    messages: &[ExecutionPortMessage],
) {
    use winwincode_execution_port::generated::{
        JobOutcomeAckMessageStatus, WorkerRegistrationResultMessageStatus,
    };
    let outcome = messages
        .iter()
        .find_map(|message| match message {
            ExecutionPortMessage::JobOutcomeMessage(value) => Some(value),
            _ => None,
        })
        .unwrap();
    seed_delivery_job(root, &dispatch.job);
    let mut storage = SqliteStorage::open(root.data()).expect("open terminal ingress storage");
    commit_execution_queue(&mut storage, &dispatch.job);
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(root.data()),
        Box::new(DiscardingPublisher),
    )
    .expect("open real Control Plane for production Worker frames");
    let registration = messages
        .iter()
        .find(|message| matches!(message, ExecutionPortMessage::WorkerRegisterMessage(_)))
        .unwrap();
    let responses = DurableExecutionPortIngress::new(
        &mut control_plane,
        &mut storage,
        &repository_scope(),
        at("2030-01-01T00:00:00.000Z"),
    )
    .unwrap()
    .handle(registration)
    .unwrap();
    assert!(responses.iter().any(|message| matches!(message,
        ExecutionPortMessage::WorkerRegistrationResultMessage(result)
        if result.status == WorkerRegistrationResultMessageStatus::Accepted)));
    prepare_terminal_authority(&mut storage, dispatch, outcome);
    let mut accepted = false;
    for message in messages.iter().filter(|message| {
        matches!(
            message,
            ExecutionPortMessage::JobDispatchResultMessage(_)
                | ExecutionPortMessage::SessionBindingMessage(_)
                | ExecutionPortMessage::RuntimeEventMessage(_)
                | ExecutionPortMessage::JobOutcomeMessage(_)
        )
    }) {
        let responses = DurableExecutionPortIngress::new(
            &mut control_plane,
            &mut storage,
            &repository_scope(),
            at("2030-01-01T00:00:02.000Z"),
        )
        .unwrap()
        .handle(message)
        .expect("consume actual production Worker frame");
        if matches!(message, ExecutionPortMessage::JobOutcomeMessage(_)) {
            assert!(responses.iter().any(|message| matches!(message,
                ExecutionPortMessage::JobOutcomeAckMessage(ack)
                if ack.status == JobOutcomeAckMessageStatus::Accepted
                    || ack.status == JobOutcomeAckMessageStatus::Duplicate)));
            accepted = true;
        }
    }
    assert!(accepted);
    assert_eq!(
        storage
            .execution_queue()
            .unwrap()
            .load_job(&execution_scope(&dispatch.job), &dispatch.job.job_id)
            .unwrap()
            .unwrap()
            .state,
        ExecutionJobState::Failed
    );
    let database = storage.database_path().to_path_buf();
    control_plane.shutdown().unwrap();
    drop(storage);
    let connection = rusqlite::Connection::open(&database).unwrap();
    let payload: Vec<u8> = connection
        .query_row(
            "SELECT payload FROM outbox WHERE topic = 'delivery.work_run.terminal'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(connection);
    let projection: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(projection["jobId"], dispatch.job.job_id.0);
    assert_eq!(
        projection["outcome"]["status"],
        serde_json::to_value(&outcome.outcome.status).unwrap()
    );
    assert!(projection["outcome"].get("error").is_none());
    assert_secret_safe(messages, &projection);

    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(root.data()),
        Box::new(DiscardingPublisher),
    )
    .unwrap();
    let mut storage = SqliteStorage::open(root.data()).unwrap();
    // Replaying the exact message after expiry must use its durable receipt.
    let responses = DurableExecutionPortIngress::new(
        &mut control_plane,
        &mut storage,
        &repository_scope(),
        at("2030-01-01T00:06:00.000Z"),
    )
    .unwrap()
    .handle(&ExecutionPortMessage::JobOutcomeMessage(outcome.clone()))
    .unwrap();
    assert!(responses.iter().any(|message| matches!(message,
        ExecutionPortMessage::JobOutcomeAckMessage(ack)
        if ack.status == JobOutcomeAckMessageStatus::Duplicate)));
    control_plane.shutdown().unwrap();
}

fn prepare_terminal_authority(
    storage: &mut SqliteStorage,
    dispatch: &JobDispatchMessage,
    outcome: &winwincode_execution_port::generated::JobOutcomeMessage,
) {
    let lease = &dispatch.lease;
    let heartbeat = WorkerHeartbeatRequest {
        active_leases: Vec::new(),
        available_slots: 1,
        running_slots: 0,
        max_slots: 1,
        heartbeat_sequence: ExecutionSequence(1),
        message_id: ExecutionMessageId(id("xmsg", 601)),
        observed_at: at("2030-01-01T00:00:00.000Z"),
        sent_at: at("2030-01-01T00:00:00.000Z"),
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
    };
    assert_eq!(
        storage
            .execution_registry()
            .unwrap()
            .record_heartbeat(&heartbeat)
            .unwrap()
            .status,
        StorageLeaseWriteStatus::Accepted
    );
    let scope = execution_scope(&dispatch.job);
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 1,
        max_queued: 1,
        token_budget: Some(10_000),
        cost_budget_microunits: Some(10_000),
        max_runtime_millis: Some(300_000),
    };
    let mut admission = storage.execution_admission().unwrap();
    for boundary in admission_boundaries(&scope) {
        admission
            .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
            .unwrap();
    }
    admission
        .reserve(&ExecutionReservationRequest {
            scope: scope.clone(),
            user_id: UserId(id("usr", 1)),
            worker_pool_id: WorkerPoolId(id("wpl", 1)),
            job_id: dispatch.job.job_id.clone(),
            request_id: RequestId(id("req", 602)),
            repository_access: ExecutionRepositoryAccess::ReadOnly,
            reserved_tokens: Some(100),
            reserved_cost_microunits: Some(100),
            runtime_limit_millis: Some(300_000),
            submitted_at: at("2030-01-01T00:00:00.000Z"),
        })
        .unwrap();
    admission
        .start(&ExecutionReservationStart {
            scope,
            worker_pool_id: WorkerPoolId(id("wpl", 1)),
            job_id: dispatch.job.job_id.clone(),
            request_id: RequestId(id("req", 603)),
            expected_revision: 1,
            started_at: at("2030-01-01T00:00:00.000Z"),
        })
        .unwrap();
    assert_eq!(
        storage
            .execution_registry()
            .unwrap()
            .claim_execution_job(&ExecutionLeaseClaim {
                attempt: u64::try_from(lease.attempt).unwrap(),
                expires_at: lease.expires_at.clone(),
                fencing_token: lease.fencing_token.clone(),
                issued_at: lease.issued_at.clone(),
                job_id: lease.job_id.clone(),
                lease_id: lease.lease_id.clone(),
                message_id: ExecutionMessageId(id("xmsg", 604)),
                payload_digest: dispatch.job.payload_digest.clone(),
                request_id: RequestId(id("req", 604)),
                worker_id: lease.worker_id.clone(),
                worker_instance_id: lease.worker_instance_id.clone(),
            })
            .unwrap()
            .status,
        StorageLeaseWriteStatus::Accepted
    );
    let authority = WorkerSlotAuthority {
        attempt: u64::try_from(lease.attempt).unwrap(),
        codex_thread_id: outcome.session_identity.codex_thread_id.clone(),
        fencing_token: lease.fencing_token.clone(),
        job_id: lease.job_id.clone(),
        lease_id: lease.lease_id.clone(),
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
        worker_session_id: outcome.worker_session_id.clone(),
    };
    let mut slots = storage.worker_session_slots().unwrap();
    slots
        .configure_resources(
            &lease.worker_id,
            &lease.worker_instance_id,
            WorkerSlotResourceLimits {
                max_memory_bytes: 1_000,
                max_disk_bytes: 1_000,
                max_processes: 4,
            },
        )
        .unwrap();
    slots
        .open(&WorkerSlotOpenRequest {
            authority,
            resources: WorkerSlotResources {
                memory_bytes: 10,
                disk_bytes: 10,
                process_slots: 1,
            },
            request_id: RequestId(id("req", 605)),
            opened_at: lease.issued_at.clone(),
        })
        .unwrap();
}

fn assert_no_model_request(messages: &[ExecutionPortMessage]) {
    assert!(!messages.iter().any(|message| matches!(
        message,
        ExecutionPortMessage::ModelOpenMessage(_) | ExecutionPortMessage::ModelAckMessage(_)
    )));
}
