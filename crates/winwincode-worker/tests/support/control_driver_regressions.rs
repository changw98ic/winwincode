// SPDX-License-Identifier: Apache-2.0

use super::*;
use winwincode_worker::control_driver::WorkerControlSource;

#[derive(Clone, Default)]
struct Controls {
    queued: Rc<RefCell<VecDeque<(ExecutionMessageId, ExecutionPortMessage)>>>,
    trace: Rc<RefCell<Vec<String>>>,
    clock: Rc<RefCell<String>>,
}

impl WorkerControlSource for Controls {
    type Error = std::io::Error;

    fn next_control(
        &self,
    ) -> Result<Option<(ExecutionMessageId, ExecutionPortMessage)>, Self::Error> {
        Ok(self.queued.borrow_mut().pop_front())
    }

    fn confirm(&self, id: ExecutionMessageId) -> Result<(), Self::Error> {
        self.trace.borrow_mut().push(format!("confirmed:{}", id.0));
        Ok(())
    }

    fn retry(&self, id: &ExecutionMessageId) -> Result<(), Self::Error> {
        self.trace.borrow_mut().push(format!("retried:{}", id.0));
        Ok(())
    }
}

#[derive(Clone)]
struct DriverPort {
    recorded: RecordingPort,
    controls: Controls,
    after_response: Rc<RefCell<Option<(ExecutionPortMessage, String)>>>,
}

impl WorkerExecutionPort for DriverPort {
    type Error = RecordingPortError;

    fn failure_kind(error: &Self::Error) -> winwincode_codex::ExecutionPortFailureKind {
        RecordingPort::failure_kind(error)
    }

    fn has_pending_controls(&self) -> bool {
        !self.controls.queued.borrow().is_empty()
    }

    async fn send(&mut self, message: ExecutionPortMessage) -> Result<(), Self::Error> {
        self.recorded.send(message.clone()).await?;
        // Exercise an actual suspended response boundary. The virtual clock
        // advances independently of the timestamp passed into the driver.
        tokio::task::yield_now().await;
        let value = serde_json::to_value(&message).unwrap();
        self.controls
            .trace
            .borrow_mut()
            .push(format!("sent:{}", value["messageId"].as_str().unwrap()));
        if let Some((control, received_at)) = self.after_response.borrow_mut().take() {
            *self.controls.clock.borrow_mut() = received_at;
            let id = serde_json::to_value(&control).unwrap()["messageId"]
                .as_str()
                .unwrap()
                .to_owned();
            self.controls
                .queued
                .borrow_mut()
                .push_back((ExecutionMessageId(id), control));
        }
        Ok(())
    }
}

type DriverWorker = WorkerMain<DriverPort, FakeCodex>;

async fn started_worker() -> (DriverWorker, DriverPort, FakeCodex) {
    let port = DriverPort {
        recorded: RecordingPort::default(),
        controls: Controls {
            clock: Rc::new(RefCell::new(NOW.into())),
            ..Controls::default()
        },
        after_response: Rc::new(RefCell::new(None)),
    };
    let codex = FakeCodex::with_threads([thread('A'), thread('B')]);
    let mut worker = WorkerMain::new(
        worker_config(2),
        port.clone(),
        codex.clone(),
        test_workspaces(),
    );
    worker.start(now()).await.unwrap();
    let register = port
        .recorded
        .messages
        .borrow()
        .iter()
        .find_map(|message| {
            if let ExecutionPortMessage::WorkerRegisterMessage(register) = message {
                Some(register.clone())
            } else {
                None
            }
        })
        .unwrap();
    worker
        .accept_control(
            &ExecutionPortMessage::WorkerRegistrationResultMessage(
                WorkerRegistrationResultMessage {
                    error: None,
                    heartbeat_interval_ms: 2_000,
                    kind: WorkerRegistrationResultMessageKind::WorkerRegistrationResult,
                    lease_recovery: WorkerRegistrationResultMessageLeaseRecovery::NoActiveLeases,
                    message_id: ExecutionMessageId(id("xmsg", 'R')),
                    request_id: register.request_id,
                    schema_version: SchemaVersion::WinwincodeV1,
                    sent_at: now(),
                    server_time: now(),
                    status: WorkerRegistrationResultMessageStatus::Accepted,
                    worker_id: register.worker_id,
                    worker_instance_id: register.worker_instance_id,
                },
            ),
            now(),
        )
        .await
        .unwrap();
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('A', product_scope('A'))),
            now(),
        )
        .await
        .unwrap();
    worker.flush_durable_outbox().await.unwrap();
    port.controls.trace.borrow_mut().clear();
    (worker, port, codex)
}

fn queue_evidence(codex: &FakeCodex, active: &winwincode_worker::ActiveJob) {
    for sequence in 1..=2 {
        let message = serde_json::from_value(serde_json::json!({
            "kind":"runtime.event", "schemaVersion":SchemaVersion::WinwincodeV1,
            "messageId":format!("xmsg_{:026}",1000+sequence), "sentAt":now(), "lease":active.lease,
            "workerSessionId":active.worker_session_id, "sessionIdentity":active.session_identity,
            "codexThreadId":active.codex_thread_id,
            "event":{"eventId":format!("evt_{sequence:026}"),"sequence":sequence,"category":"command","occurredAt":now(),"summary":"safe fixture evidence"}
        })).unwrap();
        codex.clone().retain_execution_delivery(&message).unwrap();
    }
}

fn renewal(active: &winwincode_worker::ActiveJob) -> ExecutionPortMessage {
    let mut lease = active.lease.clone();
    lease.expires_at = Instant("2027-01-15T08:10:00.000Z".into());
    ExecutionPortMessage::LeaseRenewMessage(
        winwincode_execution_port::generated::LeaseRenewMessage {
            kind: winwincode_execution_port::generated::LeaseRenewMessageKind::LeaseRenew,
            lease,
            message_id: ExecutionMessageId(id("xmsg", 'N')),
            prior_expires_at: active.lease.expires_at.clone(),
            request_id: RequestId(id("req", 'N')),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: Instant("2027-01-15T08:04:50.000Z".into()),
        },
    )
}

async fn drive(
    worker: &mut DriverWorker,
    port: &DriverPort,
) -> Option<winwincode_worker::WorkerError> {
    worker
        .drive_with_controls(&port.controls, || {
            Ok(Instant(port.controls.clock.borrow().clone()))
        })
        .await
        .unwrap()
}

fn assert_control_before_next_send(port: &DriverPort) {
    let trace = port.controls.trace.borrow();
    assert!(
        trace[0].starts_with("sent:"),
        "first response was consumed: {trace:?}"
    );
    assert!(
        trace[1].starts_with("confirmed:"),
        "control must precede another send: {trace:?}"
    );
    assert_eq!(
        trace
            .iter()
            .filter(|event| event.starts_with("sent:"))
            .count(),
        1,
        "one response must yield the whole outbound scan: {trace:?}"
    );
}

#[tokio::test]
async fn renewal_from_send_response_precedes_next_send_and_core_poll() {
    let (mut worker, port, codex) = started_worker().await;
    let active = worker.active_jobs()[0].clone();
    queue_evidence(&codex, &active);
    let calls_before = codex.calls();
    *port.after_response.borrow_mut() = Some((renewal(&active), "2027-01-15T08:04:55.000Z".into()));
    assert!(drive(&mut worker, &port).await.is_none());
    assert_control_before_next_send(&port);
    assert_eq!(
        codex.calls(),
        calls_before,
        "Core cannot run while a control is queued"
    );
    assert_eq!(
        worker.active_jobs()[0].lease.expires_at.0,
        "2027-01-15T08:10:00.000Z"
    );
    assert!(drive(&mut worker, &port).await.is_none());
    assert!(codex.calls().iter().any(|call| call.starts_with("poll:")));
}

#[tokio::test]
async fn cancellation_from_send_response_interrupts_before_core_poll() {
    let (mut worker, port, codex) = started_worker().await;
    let active = worker.active_jobs()[0].clone();
    queue_evidence(&codex, &active);
    *port.after_response.borrow_mut() = Some((
        ExecutionPortMessage::JobCancelMessage(cancel_for(&active, 'C')),
        NOW.into(),
    ));
    assert!(drive(&mut worker, &port).await.is_none());
    assert_control_before_next_send(&port);
    let calls = codex.calls();
    assert!(calls.iter().any(|call| call.starts_with("interrupt:")));
    assert!(!calls.iter().any(|call| call.starts_with("poll:")));
    codex.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::Cancelled(
            secret_safe_runtime_summary("cancelled fixture").unwrap(),
        )),
    );
    assert!(drive(&mut worker, &port).await.is_none());
    assert_eq!(observed_outcomes(&port.recorded.messages).len(), 1);
    assert!(worker.active_jobs().is_empty());
}

#[tokio::test]
async fn runtime_ack_from_send_response_is_consumed_before_next_send() {
    let (mut worker, port, codex) = started_worker().await;
    let active = worker.active_jobs()[0].clone();
    queue_evidence(&codex, &active);
    let ack = serde_json::from_value(serde_json::json!({
        "kind":"runtime.ack", "schemaVersion":SchemaVersion::WinwincodeV1, "messageId":id("xmsg",'K'),
        "sentAt":now(), "lease":active.lease, "workerSessionId":active.worker_session_id,
        "sessionIdentity":active.session_identity, "status":"accepted", "ackSequence":1
    })).unwrap();
    *port.after_response.borrow_mut() = Some((ack, NOW.into()));
    assert!(drive(&mut worker, &port).await.is_none());
    assert_control_before_next_send(&port);
    assert!(!codex.calls().iter().any(|call| call.starts_with("poll:")));
}

#[tokio::test]
async fn late_exact_renewal_settles_its_job_without_renewing_or_stopping_another() {
    let (mut worker, port, codex) = started_worker().await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(dispatch('B', product_scope('B'))),
            now(),
        )
        .await
        .unwrap();
    worker.flush_durable_outbox().await.unwrap();
    let active = worker
        .active_jobs()
        .into_iter()
        .find(|active| active.job.job_id.0 == id("job", 'A'))
        .unwrap()
        .clone();
    port.controls.trace.borrow_mut().clear();
    queue_evidence(&codex, &active);
    *port.after_response.borrow_mut() = Some((renewal(&active), "2027-01-15T08:05:01.000Z".into()));
    assert!(drive(&mut worker, &port).await.is_none());
    assert_control_before_next_send(&port);
    let remaining = worker.active_jobs();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].job.job_id.0, id("job", 'B'));
    assert_eq!(remaining[0].lease.expires_at, lease('B').expires_at);
    worker.flush_durable_outbox().await.unwrap();
    let outcomes = observed_outcomes(&port.recorded.messages);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].outcome.status, ExecutionOutcomeStatus::Failed);
    assert_eq!(
        outcomes[0].outcome.error.as_ref().unwrap().code,
        winwincode_execution_port::generated::ExecutionPortErrorCode::LeaseExpired
    );
}

#[tokio::test]
async fn candidate_git_failure_retains_stage_and_cause_without_private_path() {
    let port = RecordingPort::default();
    let messages = Rc::clone(&port.messages);
    let codex = FakeCodex::with_threads([thread('A')]);
    let mut worker = test_worker(worker_config(1), port, codex.clone());
    register(&mut worker).await;
    worker
        .accept_control(
            &ExecutionPortMessage::JobDispatchMessage(writer_dispatch('A')),
            now(),
        )
        .await
        .unwrap();
    let active = worker.active_jobs()[0].clone();
    let workspace = codex.workspace(&active.codex_thread_id);
    std::fs::write(workspace.join("candidate.txt"), "safe fixture change\n").unwrap();
    // A real Git index lock makes candidate staging fail, while leaving the
    // registered checkout valid for the normal terminal cleanup.
    let path = Command::new("git")
        .arg("-C")
        .arg(&workspace)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "index.lock",
        ])
        .output()
        .unwrap();
    assert!(path.status.success());
    let path = PathBuf::from(String::from_utf8(path.stdout).unwrap().trim());
    std::fs::write(&path, "SYNTHETIC_PRIVATE_LOCK_CONTENT\n").unwrap();
    codex.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::Completed(CodexTurnCompletion {
            summary: secret_safe_runtime_summary("candidate fixture completed").unwrap(),
            artifacts: Vec::new(),
            usage: Some(measured_completion_usage()),
        })),
    );
    worker.poll_codex_boxed().await.unwrap();
    let outcomes = observed_outcomes(&messages);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        outcomes[0].outcome.error.as_ref().unwrap().code,
        winwincode_execution_port::generated::ExecutionPortErrorCode::CandidatePreparationFailed
    );
    let safe = serde_json::to_string(&outcomes[0]).unwrap();
    assert!(safe.contains("stage=candidate_prepare"));
    assert!(safe.contains("cause_code=WORKSPACE_GIT"));
    assert!(!safe.contains("SYNTHETIC_PRIVATE_LOCK_CONTENT"));
    assert!(!safe.contains(path.to_str().unwrap()));
}

#[tokio::test]
async fn queued_control_cannot_starve_the_durable_outbox() {
    let (mut worker, port, codex) = started_worker().await;
    let active = worker.active_jobs()[0].clone();
    queue_evidence(&codex, &active);
    // A control is already pending before the flush starts.
    let control = renewal(&active);
    port.controls
        .queued
        .borrow_mut()
        .push_back((ExecutionMessageId(id("xmsg", 'N')), control));
    worker.flush_durable_outbox().await.unwrap();
    let sent = port
        .controls
        .trace
        .borrow()
        .iter()
        .filter(|event| event.starts_with("sent:"))
        .count();
    assert_eq!(
        sent, 1,
        "the flush must make progress by one frame, then yield to the control"
    );
}
