// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::collections::VecDeque;
use std::io::{BufReader, Read, Write};
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Duration;
use winwincode_client_port::worker_relay::{WorkerRelayRequest, WorkerRelayResponse};
use winwincode_codex::{CodexCoreAdapter, WorkerExecutionPort};
use winwincode_control_plane::{RepositoryExecutionScheduler, WorkRunCancellationRequest};
use winwincode_execution_port::transport::{
    ExecutionPortCore, RemoteExchangeDelivery, RemoteExchangeRequest, RemoteExchangeResponse,
    RemoteTransportAdapter,
};
use winwincode_storage::RepositorySchedulerScope;
use winwincode_worker::remote_transport::{RemoteWorkerPort, RemoteWorkerPortError};

struct RelayCore;

impl ExecutionPortCore for RelayCore {
    type Output = ();
    type Error = std::convert::Infallible;

    fn accept(&mut self, _message: &ExecutionPortMessage) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Default)]
struct RelayState {
    response: VecDeque<u8>,
    requests: Vec<ExecutionPortMessage>,
    cancellation: Option<JobCancelMessage>,
    retry_started: Option<std::time::Instant>,
    fail_artifacts: bool,
    artifact_failed: bool,
    pause_after_id: Option<String>,
}

#[derive(Clone)]
struct RelayInput(Arc<Mutex<RelayState>>);

impl Read for RelayInput {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let mut state = self.0.lock().unwrap();
        let count = bytes.len().min(state.response.len());
        for byte in &mut bytes[..count] {
            *byte = state.response.pop_front().unwrap();
        }
        Ok(count)
    }
}

struct RelayOutput {
    state: Arc<Mutex<RelayState>>,
    pending: Vec<u8>,
}

impl Write for RelayOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let WorkerRelayRequest::Exchange { body } = serde_json::from_slice(&self.pending).unwrap()
        else {
            panic!("execution exchange");
        };
        self.pending.clear();
        let request = RemoteExchangeRequest::decode(body.as_bytes()).unwrap();
        let message = RemoteTransportAdapter::<RelayCore>::decode(request.frame())
            .unwrap()
            .message()
            .clone();
        let mut state = self.state.lock().unwrap();
        let runtime_failed = matches!(message, ExecutionPortMessage::RuntimeEventMessage(_))
            && state.retry_started.is_none();
        let artifact_failed = matches!(message, ExecutionPortMessage::ArtifactOpenMessage(_))
            && state.fail_artifacts
            && !state.artifact_failed;
        let failed = runtime_failed || artifact_failed;
        state.requests.push(message.clone());
        let response = if failed {
            if runtime_failed {
                state.retry_started = Some(std::time::Instant::now());
            }
            if artifact_failed {
                state.artifact_failed = true;
            }
            WorkerRelayResponse {
                status: 503,
                body: String::new(),
                failure: Some(winwincode_network::NetworkFailure::http(
                    503,
                    Some(Duration::from_secs(2)),
                )),
            }
        } else {
            let yield_control = matches!(message, ExecutionPortMessage::WorkerHeartbeatMessage(_))
                || state.pause_after_id.as_deref().is_some_and(|id| {
                    winwincode_execution_port::transport::execution_message_id(&message)
                        .unwrap()
                        .0
                        == id
                });
            let deliveries = if yield_control {
                state
                    .cancellation
                    .take()
                    .map(|cancel| RemoteExchangeDelivery {
                        delivery_id: cancel.message_id.clone(),
                        frame: RemoteTransportAdapter::<RelayCore>::encode(
                            &TypedFrame::new(
                                FrameDirection::ControlPlaneToWorker,
                                ExecutionPortMessage::JobCancelMessage(cancel),
                            )
                            .unwrap(),
                        )
                        .unwrap(),
                    })
                    .into_iter()
                    .collect()
            } else {
                Vec::new()
            };
            WorkerRelayResponse {
                status: 200,
                body: String::from_utf8(
                    RemoteExchangeResponse::with_acceptance(deliveries, true)
                        .unwrap()
                        .encode()
                        .unwrap(),
                )
                .unwrap(),
                failure: None,
            }
        };
        state
            .response
            .extend(serde_json::to_vec(&response).unwrap());
        state.response.push_back(b'\n');
        Ok(())
    }
}

#[derive(Default)]
struct LocalModelLane {
    application: Option<StandaloneModelExecutionApplication>,
    cancellations: Vec<winwincode_control_plane::ModelStreamFlowCancellationReceipt>,
}

struct SwitchingPort {
    recorded: RecordedPort,
    remote: RemoteWorkerPort,
    enabled: Arc<AtomicBool>,
    models: Arc<Mutex<LocalModelLane>>,
}

impl WorkerExecutionPort for SwitchingPort {
    type Error = RemoteWorkerPortError;

    fn has_pending_controls(&self) -> bool {
        self.enabled.load(Ordering::SeqCst) && self.remote.has_pending_controls()
    }

    fn failure_kind(error: &Self::Error) -> winwincode_codex::ExecutionPortFailureKind {
        RemoteWorkerPort::failure_kind(error)
    }

    async fn send(&mut self, message: ExecutionPortMessage) -> Result<(), Self::Error> {
        if matches!(message, ExecutionPortMessage::ModelAckMessage(_)) {
            {
                let mut models = self.models.lock().unwrap();
                let receipt = models
                    .application
                    .as_mut()
                    .expect("the actual opened local model application")
                    .accept_local(&typed(message.clone()))
                    .expect("settle the original model exchange");
                match receipt {
                    ModelExecutionPortReceipt::Acknowledged(
                        winwincode_control_plane::ModelExecutionAckReceipt::Cancelled(receipt),
                    ) => {
                        models.cancellations.push(receipt);
                    }
                    ModelExecutionPortReceipt::Acknowledged(
                        winwincode_control_plane::ModelExecutionAckReceipt::Acknowledged(_),
                    ) => {}
                    ModelExecutionPortReceipt::Opened(_) => {
                        panic!("model acknowledgement must settle its existing exchange")
                    }
                }
            }
            return self
                .recorded
                .send(message)
                .await
                .map_err(|()| RemoteWorkerPortError::Transport);
        }
        if self.enabled.load(Ordering::SeqCst) {
            self.remote.send(message).await
        } else {
            self.recorded
                .send(message)
                .await
                .map_err(|()| RemoteWorkerPortError::Transport)
        }
    }
}

#[test]
fn cancellation_ack_crosses_a_retrying_durable_runtime_prefix() {
    run_on_large_stack(async {
        let root = TestDirectory::new("durable-cancel-ack");
        let dispatch = dispatch(&root);
        let recorded = RecordedPort::default();
        let state = Arc::new(Mutex::new(RelayState::default()));
        let enabled = Arc::new(AtomicBool::new(false));
        let models = Arc::new(Mutex::new(LocalModelLane::default()));
        let (remote, controls) = RemoteWorkerPort::with_io(
            worker_config().worker_id,
            worker_config().worker_instance_id,
            BufReader::new(RelayInput(Arc::clone(&state))),
            RelayOutput {
                state: Arc::clone(&state),
                pending: Vec::new(),
            },
        );
        let adapter =
            winwincode_codex::ProductionCodexAdapter::open(adapter_config(&root)).unwrap();
        let mut worker = winwincode_worker::WorkerMain::new(
            worker_config(),
            SwitchingPort {
                recorded: recorded.clone(),
                remote,
                enabled: Arc::clone(&enabled),
                models: Arc::clone(&models),
            },
            adapter,
            root.workspace_runtime(),
        );
        register(&mut worker, &recorded).await;
        worker
            .accept_control(
                &ExecutionPortMessage::JobDispatchMessage(dispatch.clone()),
                at("2030-01-01T00:00:00.000Z"),
            )
            .await
            .unwrap();
        let mut opened = None;
        for _ in 0..200 {
            Box::pin(worker.poll_codex(at("2030-01-01T00:00:01.000Z")))
                .await
                .unwrap();
            if let Some(open) = recorded
                .messages()
                .into_iter()
                .find_map(|message| match message {
                    ExecutionPortMessage::ModelOpenMessage(open) => Some(open),
                    _ => None,
                })
            {
                opened = Some(open);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let open = opened.expect("actual embedded Core model request");
        setup(&root, &open, &dispatch.job);
        let mut model_application = application(&root);
        let model_receipt = super::opened(
            model_application
                .accept_local(&typed(ExecutionPortMessage::ModelOpenMessage(open.clone())))
                .unwrap(),
        );
        assert_eq!(model_receipt.model_exchange_id, open.model_exchange_id);
        models.lock().unwrap().application = Some(model_application);
        let mut control_plane = ControlPlane::start_local(
            ControlPlaneConfig::local(root.data()),
            Box::new(DiscardingPublisher),
        )
        .unwrap();
        let mut storage = SqliteStorage::open(root.data()).unwrap();
        let scope = repository_scope();
        for message in recorded.messages() {
            if matches!(
                message,
                ExecutionPortMessage::JobDispatchResultMessage(_)
                    | ExecutionPortMessage::SessionBindingMessage(_)
            ) {
                DurableExecutionPortIngress::new(
                    &mut control_plane,
                    &mut storage,
                    &scope,
                    at("2030-01-01T00:00:01.000Z"),
                )
                .unwrap()
                .handle(&message)
                .unwrap();
            }
        }
        let active = worker.active_jobs()[0].clone();
        let scheduler_scope = RepositorySchedulerScope {
            organization_id: scope.organization_id.clone(),
            workspace_id: scope.workspace_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
        };
        let delivery = storage
            .load_state(&format!("delivery:{}", id("dlv", 1)))
            .unwrap()
            .unwrap();
        let cancel = RepositoryExecutionScheduler::new(&mut storage)
            .request_cancellation_for_work_run(&WorkRunCancellationRequest {
                scope: scheduler_scope.clone(),
                delivery_id: DeliveryId(id("dlv", 1)),
                work_run_id: active.session_identity.work_run_id.clone().unwrap(),
                request_id: RequestId(id("req", 990)),
                expected_delivery_revision: delivery.revision,
                requested_at: at("2030-01-01T00:00:02.000Z"),
                public_command_digest: digest('d'),
            })
            .unwrap()
            .unwrap();
        state.lock().unwrap().cancellation = Some(cancel.clone());
        enabled.store(true, Ordering::SeqCst);
        let mut transport_failed = false;
        for _ in 0..8 {
            if let Err(error) = Box::pin(worker.poll_codex(at("2030-01-01T00:00:02.000Z"))).await {
                assert_eq!(
                    error.code,
                    winwincode_worker::WorkerErrorCode::ExecutionPort
                );
                transport_failed = true;
                break;
            }
        }
        assert!(
            transport_failed,
            "an actual durable runtime frame receives 503 and Retry-After"
        );
        let original = state
            .lock()
            .unwrap()
            .requests
            .iter()
            .find(|message| matches!(message, ExecutionPortMessage::RuntimeEventMessage(_)))
            .unwrap()
            .clone();
        let database =
            rusqlite::Connection::open(root.worker().join("worker-codex.sqlite3")).unwrap();
        let original_id = serde_json::to_value(&original).unwrap()["messageId"]
            .as_str()
            .unwrap()
            .to_owned();
        let database_path = root.worker().join("worker-codex.sqlite3");
        let retained = || -> Vec<u8> {
            rusqlite::Connection::open(&database_path)
                .unwrap()
                .query_row(
                    "SELECT frame_json FROM execution_outbox WHERE delivery_id=?1",
                    [&original_id],
                    |row| row.get(0),
                )
                .unwrap()
        };
        let original_bytes = retained();
        let retry_state = worker
            .execution_port()
            .remote
            .retry_state_for_test(&original)
            .unwrap();
        let mut retained_adapter =
            winwincode_codex::ProductionCodexAdapter::open(adapter_config(&root)).unwrap();
        let dispatch_result = recorded
            .messages()
            .into_iter()
            .find_map(|message| match message {
                ExecutionPortMessage::JobDispatchResultMessage(result) => Some(result),
                _ => None,
            })
            .unwrap();
        for index in 0..70 {
            let mut result = dispatch_result.clone();
            result.message_id = ExecutionMessageId(id("xmsg", 10_000 + index));
            retained_adapter
                .retain_execution_delivery(&ExecutionPortMessage::JobDispatchResultMessage(result))
                .unwrap();
        }
        drop(retained_adapter);
        worker
            .heartbeat(at("2030-01-01T00:00:02.000Z"))
            .await
            .unwrap();
        let (control_id, message) = controls.next_control().unwrap().unwrap();
        assert_eq!(
            message,
            ExecutionPortMessage::JobCancelMessage(cancel.clone())
        );
        worker
            .accept_control(&message, at("2030-01-01T00:00:02.000Z"))
            .await
            .unwrap();
        controls.confirm(control_id).unwrap();
        assert_eq!(
            RepositoryExecutionScheduler::new(&mut storage)
                .pending_cancellations(&scheduler_scope)
                .unwrap(),
            vec![cancel.clone()]
        );
        assert_eq!(
            worker.active_jobs()[0].lifecycle,
            winwincode_worker::ActiveJobLifecycle::Cancelling
        );
        let run = stored_run_json(&root);
        assert!(!run["operationFencedAt"].is_null());
        assert!(!run["terminal"].is_null());
        for _ in 0..8 {
            if let Err(error) = worker.flush_durable_outbox().await {
                assert_eq!(
                    error.code,
                    winwincode_worker::WorkerErrorCode::ExecutionPort,
                    "only the original runtime category remains unavailable"
                );
            }
        }
        {
            let lane = models.lock().unwrap();
            assert_eq!(lane.cancellations.len(), 1);
            let receipt = &lane.cancellations[0];
            assert_eq!(
                receipt.gateway.model_exchange_id,
                model_receipt.model_exchange_id
            );
            assert_eq!(
                receipt.pool.model_exchange_id,
                model_receipt.model_exchange_id
            );
            assert_eq!(
                receipt.gateway.outcome,
                winwincode_control_plane::ProviderGatewayTerminalOutcome::Cancelled
            );
            assert_eq!(
                receipt.pool.outcome,
                winwincode_control_plane::ModelRequestTerminalOutcome::Cancelled
            );
        }
        let requests = state.lock().unwrap().requests.clone();
        let ack = requests.iter().find(|message| matches!(message, ExecutionPortMessage::JobCancelAckMessage(_)))
            .expect("the actual cancel handler's durable business ACK must cross the unavailable runtime prefix").clone();
        assert!(std::time::Instant::now() < retry_state.0);
        assert_eq!(
            worker
                .execution_port()
                .remote
                .retry_state_for_test(&original),
            Some(retry_state)
        );
        assert_eq!(
            requests
                .iter()
                .filter(|message| matches!(message, ExecutionPortMessage::RuntimeEventMessage(_)))
                .count(),
            1
        );
        assert_eq!(retained(), original_bytes);
        assert_eq!(original_bytes, serde_json::to_vec(&original).unwrap());
        DurableExecutionPortIngress::new(
            &mut control_plane,
            &mut storage,
            &scope,
            at("2030-01-01T00:00:02.000Z"),
        )
        .unwrap()
        .handle(&ack)
        .unwrap();
        assert!(
            RepositoryExecutionScheduler::new(&mut storage)
                .pending_cancellations(&scheduler_scope)
                .unwrap()
                .is_empty()
        );
        let receipt_count: i64 = rusqlite::Connection::open(storage.database_path()).unwrap().query_row(
            "SELECT COUNT(*) FROM command_receipts WHERE stream_id LIKE 'execution-cancellation-ack:%'", [], |row| row.get(0),
        ).unwrap();
        assert_eq!(receipt_count, 1);
        worker
            .accept_control(&message, at("2030-01-01T00:00:02.000Z"))
            .await
            .unwrap();
        for _ in 0..8 {
            let _ = worker.flush_durable_outbox().await;
        }
        for ack in state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|message| matches!(message, ExecutionPortMessage::JobCancelAckMessage(_)))
        {
            DurableExecutionPortIngress::new(
                &mut control_plane,
                &mut storage,
                &scope,
                at("2030-01-01T00:00:02.000Z"),
            )
            .unwrap()
            .handle(ack)
            .unwrap();
        }
        assert!(
            RepositoryExecutionScheduler::new(&mut storage)
                .pending_cancellations(&scheduler_scope)
                .unwrap()
                .is_empty()
        );
        tokio::time::sleep(
            retry_state
                .0
                .saturating_duration_since(std::time::Instant::now()),
        )
        .await;
        let before_recovery = state.lock().unwrap().requests.len();
        let _ = worker.flush_durable_outbox().await;
        assert!(
            state.lock().unwrap().requests[before_recovery..].contains(&original),
            "the next flush retries the original frame before scanning more backlog"
        );
        for _ in 0..8 {
            let _ = worker.flush_durable_outbox().await;
        }
        let runtime = state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter_map(|message| match message {
                ExecutionPortMessage::RuntimeEventMessage(event) => Some(event.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(runtime.len() >= 2);
        assert_eq!(runtime[0], runtime[1]);
        assert!(
            runtime[1..]
                .windows(2)
                .all(|pair| pair[0].event.sequence.0 < pair[1].event.sequence.0)
        );
        for event in &runtime[1..] {
            let responses = DurableExecutionPortIngress::new(
                &mut control_plane,
                &mut storage,
                &scope,
                at("2030-01-01T00:00:03.000Z"),
            )
            .unwrap()
            .handle(&ExecutionPortMessage::RuntimeEventMessage(event.clone()))
            .unwrap();
            for response in responses {
                worker
                    .accept_control(&response, at("2030-01-01T00:00:03.000Z"))
                    .await
                    .unwrap();
            }
        }
        let remaining: i64 = database
            .query_row(
                "SELECT COUNT(*) FROM execution_outbox WHERE delivery_id=?1",
                [&original_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0);
        worker
            .shutdown(at("2030-01-01T00:00:04.000Z"))
            .await
            .unwrap();
        control_plane.shutdown().unwrap();
    });
}

fn delivery_fixture(kind: &str, job: u64, sequence: u64) -> ExecutionPortMessage {
    let fixtures: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let mut value = fixtures["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["kind"] == kind)
        .unwrap()
        .clone();
    value["messageId"] = id("xmsg", sequence).into();
    value["sentAt"] = "2030-01-01T00:00:01.000Z".into();
    if value.get("jobId").is_some() {
        value["jobId"] = id("job", job).into();
    }
    if value.get("lease").is_some() {
        value["lease"]["jobId"] = id("job", job).into();
        value["lease"]["leaseId"] = id("lse", job).into();
        value["lease"]["workerInstanceId"] = worker_config().worker_instance_id.0.into();
        value["lease"]["issuedAt"] = "2030-01-01T00:00:00.000Z".into();
        value["lease"]["expiresAt"] = "2030-01-01T00:05:00.000Z".into();
    }
    if value.get("sessionIdentity").is_some() {
        value["workerSessionId"] = id("wsn", job).into();
        value["sessionIdentity"]["workerSessionId"] = id("wsn", job).into();
        value["sessionIdentity"]["codexThreadId"] = id("cdx", job).into();
        value["sessionIdentity"]["productSessionId"] = id("psn", job).into();
        value["sessionIdentity"]["workRunId"] = id("wrn", job).into();
    }
    if value.get("codexThreadId").is_some() {
        value["codexThreadId"] = id("cdx", job).into();
    }
    if kind == "job.outcome" {
        value["outcome"]["codexThreadId"] = id("cdx", job).into();
        value["outcome"]["finishedAt"] = "2030-01-01T00:00:01.000Z".into();
        value["outcome"]["artifacts"] = serde_json::json!([]);
        value["outcome"]["lastEventSequence"] = 0.into();
    }
    serde_json::from_value(value).unwrap()
}

#[test]
fn durable_retries_preserve_dependencies_and_allow_an_independent_job_outcome() {
    run_on_large_stack(async {
        let root = TestDirectory::new("durable-retry-dependencies");
        let recorded = RecordedPort::default();
        let state = Arc::new(Mutex::new(RelayState {
            fail_artifacts: true,
            ..RelayState::default()
        }));
        let enabled = Arc::new(AtomicBool::new(false));
        let (remote, controls) = RemoteWorkerPort::with_io(
            worker_config().worker_id,
            worker_config().worker_instance_id,
            BufReader::new(RelayInput(Arc::clone(&state))),
            RelayOutput {
                state: Arc::clone(&state),
                pending: Vec::new(),
            },
        );
        let mut adapter =
            winwincode_codex::ProductionCodexAdapter::open(adapter_config(&root)).unwrap();
        let first_runtime = delivery_fixture("runtime.event", 1, 1000);
        let mut next_runtime = delivery_fixture("runtime.event", 1, 1001);
        let ExecutionPortMessage::RuntimeEventMessage(next) = &mut next_runtime else {
            unreachable!()
        };
        next.event.event_id.0 = id("xevt", 1001);
        next.event.sequence.0 = 2;
        let mut runtime_outcome = delivery_fixture("job.outcome", 1, 1002);
        let ExecutionPortMessage::JobOutcomeMessage(outcome) = &mut runtime_outcome else {
            unreachable!()
        };
        outcome.outcome.last_event_sequence.0 = 2;
        let artifact_open = delivery_fixture("artifact.open", 2, 1003);
        let artifact_chunk = delivery_fixture("artifact.chunk", 2, 1004);
        let mut artifact_outcome = delivery_fixture("job.outcome", 2, 1005);
        let ExecutionPortMessage::JobOutcomeMessage(outcome) = &mut artifact_outcome else {
            unreachable!()
        };
        let ExecutionPortMessage::ArtifactOpenMessage(open) = &artifact_open else {
            unreachable!()
        };
        outcome.outcome.artifacts.push(ArtifactReference {
            artifact_id: open.artifact.artifact_id.clone(),
            digest: open.artifact.digest.clone(),
        });
        let independent_outcome = delivery_fixture("job.outcome", 3, 1006);
        for frame in [
            &first_runtime,
            &next_runtime,
            &artifact_open,
            &artifact_chunk,
            &independent_outcome,
        ] {
            adapter.retain_execution_delivery(frame).unwrap();
        }
        for sequence in 2000..2187 {
            adapter
                .retain_execution_delivery(&delivery_fixture("job.dispatch_result", 4, sequence))
                .unwrap();
        }
        adapter.retain_execution_delivery(&runtime_outcome).unwrap();
        adapter
            .retain_execution_delivery(&artifact_outcome)
            .unwrap();
        for sequence in 3000..3256 {
            adapter
                .retain_execution_delivery(&delivery_fixture("job.dispatch_result", 4, sequence))
                .unwrap();
        }
        let ExecutionPortMessage::JobCancelMessage(pause_control) =
            delivery_fixture("job.cancel", 4, 7777)
        else {
            unreachable!()
        };
        {
            let mut relay = state.lock().unwrap();
            relay.pause_after_id = Some(id("xmsg", 2186));
            relay.cancellation = Some(pause_control);
        }
        let mut worker = winwincode_worker::WorkerMain::new(
            worker_config(),
            SwitchingPort {
                recorded: recorded.clone(),
                remote,
                enabled: Arc::clone(&enabled),
                models: Arc::new(Mutex::new(LocalModelLane::default())),
            },
            adapter,
            root.workspace_runtime(),
        );
        register(&mut worker, &recorded).await;
        enabled.store(true, Ordering::SeqCst);
        let error = worker.flush_durable_outbox().await.unwrap_err();
        assert_eq!(
            error.code,
            winwincode_worker::WorkerErrorCode::ExecutionPort
        );
        let runtime_retry = worker
            .execution_port()
            .remote
            .retry_state_for_test(&first_runtime)
            .unwrap();
        for _ in 0..8 {
            if controls.has_pending_controls() {
                break;
            }
            let _ = worker.flush_durable_outbox().await;
        }
        let requests = state.lock().unwrap().requests.clone();
        assert!(
            requests.contains(&independent_outcome),
            "another Job's durable terminal has no failed runtime or Artifact dependency"
        );
        let artifact_retry = worker
            .execution_port()
            .remote
            .retry_state_for_test(&artifact_open)
            .unwrap();
        let (control_id, pause_control) = controls
            .next_control()
            .unwrap()
            .expect("stop precisely after the durable prefix and before the dependent Outcomes");
        worker
            .accept_control(&pause_control, at("2030-01-01T00:00:02.000Z"))
            .await
            .unwrap();
        controls.confirm(control_id).unwrap();
        for blocked in [
            &next_runtime,
            &runtime_outcome,
            &artifact_chunk,
            &artifact_outcome,
        ] {
            assert!(
                !requests.contains(blocked),
                "a real stream or Artifact dependency must remain ordered"
            );
        }
        assert_eq!(
            requests
                .iter()
                .filter(|message| **message == first_runtime)
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|message| **message == artifact_open)
                .count(),
            1
        );
        assert_eq!(
            worker
                .execution_port()
                .remote
                .retry_state_for_test(&first_runtime),
            Some(runtime_retry)
        );
        assert_eq!(
            worker
                .execution_port()
                .remote
                .retry_state_for_test(&artifact_open),
            Some(artifact_retry)
        );
        let retry_at = runtime_retry.0.max(artifact_retry.0);
        tokio::time::sleep(retry_at.saturating_duration_since(std::time::Instant::now())).await;
        let before_recovery = state.lock().unwrap().requests.len();
        worker.flush_durable_outbox().await.unwrap();
        let requests = state.lock().unwrap().requests.clone();
        assert!(requests[before_recovery..].contains(&first_runtime));
        assert!(requests[before_recovery..].contains(&artifact_open));
        for (dependency, outcome) in [
            (&next_runtime, &runtime_outcome),
            (&artifact_chunk, &artifact_outcome),
        ] {
            if let Some(position) = requests.iter().position(|request| request == outcome) {
                assert!(
                    requests[..position].contains(dependency),
                    "an Outcome sent during recovery must follow its runtime or Artifact dependency"
                );
            }
        }
        for _ in 0..6 {
            worker.flush_durable_outbox().await.unwrap();
        }
        let requests = state.lock().unwrap().requests.clone();
        let position = |message: &ExecutionPortMessage| {
            requests
                .iter()
                .rposition(|request| request == message)
                .unwrap()
        };
        assert!(position(&first_runtime) < position(&next_runtime));
        assert!(position(&next_runtime) < position(&runtime_outcome));
        assert!(position(&artifact_open) < position(&artifact_chunk));
        assert!(position(&artifact_chunk) < position(&artifact_outcome));
    });
}
