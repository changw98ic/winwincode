// SPDX-License-Identifier: Apache-2.0
//! Isolated behavior regression. No production fixes and no external model calls.
use super::*;
use winwincode_worker::control_driver::WorkerControlSource;

#[derive(Clone, Default)]
struct TimedControls {
    queued: Arc<Mutex<VecDeque<(ExecutionMessageId, ExecutionPortMessage)>>>,
    trace: Rc<RefCell<Vec<(String, u128)>>>,
    origin: Rc<RefCell<Option<std::time::Instant>>>,
}
impl WorkerControlSource for TimedControls {
    type Error = std::io::Error;
    fn next_control(
        &self,
    ) -> Result<Option<(ExecutionMessageId, ExecutionPortMessage)>, Self::Error> {
        Ok(self.queued.lock().unwrap().pop_front())
    }
    fn confirm(&self, id: ExecutionMessageId) -> Result<(), Self::Error> {
        self.trace.borrow_mut().push((
            format!("confirmed:{}", id.0),
            self.origin.borrow().unwrap().elapsed().as_millis(),
        ));
        Ok(())
    }
    fn retry(&self, id: &ExecutionMessageId) -> Result<(), Self::Error> {
        self.trace.borrow_mut().push((
            format!("retried:{}", id.0),
            self.origin.borrow().unwrap().elapsed().as_millis(),
        ));
        Ok(())
    }
}
fn elapsed_clock(origin: std::time::Instant) -> Instant {
    let base =
        time::OffsetDateTime::parse(NOW, &time::format_description::well_known::Rfc3339).unwrap();
    Instant(
        (base + time::Duration::try_from(origin.elapsed()).unwrap())
            .format(
                &time::format_description::parse(
                    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z",
                )
                .unwrap(),
            )
            .unwrap(),
    )
}
async fn validation_case(seconds: f64) -> serde_json::Value {
    let (workspaces_root, sources) = test_workspace_paths();
    let repository = sources.join(id("rpo", 'A'));
    std::fs::create_dir_all(repository.join(".winwincode")).unwrap();
    let config = PASSING_VALIDATION_CONFIG
        .replace("diagnosticParserVersion = \"typescript_v1\"", "")
        .replacen(
            "raise SystemExit(0)",
            &format!("import time; time.sleep({seconds}); raise SystemExit(0)"),
            1,
        );
    std::fs::write(repository.join(".winwincode/validation.toml"), config).unwrap();
    run_git(&repository, &["add", ".winwincode/validation.toml"]);
    run_git(
        &repository,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "isolated slow validation",
        ],
    );
    let artifacts = DurableValidationArtifactStore::open(
        workspaces_root
            .parent()
            .unwrap()
            .join("validation-artifacts"),
    )
    .unwrap();
    let workspaces = JobWorkspaceRuntime::open(workspaces_root, sources)
        .unwrap()
        .with_validation_artifact_port(artifacts)
        .with_change_batch_executor(AppliedBatchExecutor {
            calls: Arc::new(Mutex::new(Vec::new())),
        });
    let port = RecordingPort::default();
    let messages = port.messages.clone();
    let codex = FakeCodex::with_threads([thread('A')]);
    let pump = codex.clone();
    pump.state.lock().unwrap().mechanism_keep_followup_pending = true;
    let mut worker = WorkerMain::new(worker_config(1), port, codex, workspaces);
    register(&mut worker).await;
    let mut dispatched = writer_dispatch('A');
    dispatched.job.workspace.write_mode = ExecutionWorkspaceWriteMode::ReadOnly;
    dispatched.lease.expires_at = Instant("2027-01-15T08:00:03.200Z".into());
    worker
        .accept_control(&ExecutionPortMessage::JobDispatchMessage(dispatched), now())
        .await
        .unwrap();
    worker.flush_durable_outbox().await.unwrap();
    let active = worker.active_jobs()[0].clone();
    let identity = delegated_identity(&active, pump.workspace_revision(&active.codex_thread_id));
    pump.queue_poll(
        &active.codex_thread_id,
        Ok(CodexPoll::ChangeBatchProposed(Box::new(
            ChangeBatchProposalEvent {
                identity,
                occurred_at: now(),
                proposal: ChangeBatchProposal {
                    acceptance_criteria_ids: vec!["crt_00000000000000000000000001".into()],
                    disposition: ChangeBatchProposalDisposition::ContinueValue,
                    patch: DELEGATED_PATCH.into(),
                    schema_version: 1,
                    validation_profile: ValidationProfileName::Changed,
                },
            },
        ))),
    );
    let origin = std::time::Instant::now();
    let controls = TimedControls::default();
    *controls.origin.borrow_mut() = Some(origin);
    worker.heartbeat(elapsed_clock(origin)).await.unwrap();
    let initial_heartbeats = messages
        .borrow()
        .iter()
        .filter(|m| matches!(m, ExecutionPortMessage::WorkerHeartbeatMessage(_)))
        .count();
    let mut extended = active.lease.clone();
    extended.expires_at = Instant("2027-01-15T08:00:10.000Z".into());
    let message = ExecutionPortMessage::LeaseRenewMessage(
        winwincode_execution_port::generated::LeaseRenewMessage {
            kind: winwincode_execution_port::generated::LeaseRenewMessageKind::LeaseRenew,
            lease: extended,
            message_id: ExecutionMessageId(id("xmsg", 'N')),
            prior_expires_at: active.lease.expires_at.clone(),
            request_id: RequestId(id("req", 'N')),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: Instant("2027-01-15T08:00:02.100Z".into()),
        },
    );
    let queued_controls = Arc::clone(&controls.queued);
    let enqueue = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        queued_controls
            .lock()
            .unwrap()
            .push_back((ExecutionMessageId(id("xmsg", 'N')), message));
        origin.elapsed().as_millis()
    });
    let result = worker
        .drive_with_controls(&controls, || Ok(elapsed_clock(origin)))
        .await;
    let queued_ms = enqueue.join().unwrap();
    assert!(
        queued_ms < 1200,
        "fixture must enqueue renewal before original expiry"
    );
    let drive_finished_ms = origin.elapsed().as_millis();
    if !controls.queued.lock().unwrap().is_empty() {
        worker
            .drain_controls(&controls, || Ok(elapsed_clock(origin)))
            .await
            .unwrap();
    }
    worker.flush_durable_outbox().await.unwrap();
    let during_heartbeats = messages
        .borrow()
        .iter()
        .filter(|m| matches!(m, ExecutionPortMessage::WorkerHeartbeatMessage(_)))
        .count()
        - initial_heartbeats;
    let lease_expired = observed_outcomes(&messages).iter().any(|m| {
        m.outcome.error.as_ref().is_some_and(|e| {
            e.code == winwincode_execution_port::generated::ExecutionPortErrorCode::LeaseExpired
        })
    });
    let human_review_outcome = observed_outcomes(&messages).iter().any(|m| {
        m.outcome
            .error
            .as_ref()
            .is_some_and(|e| e.message.contains("human review"))
    });
    let outcome_codes = observed_outcomes(&messages)
        .iter()
        .map(|m| {
            format!(
                "{:?}:{}",
                m.outcome.error.as_ref().map(|e| &e.code),
                m.outcome.summary
            )
        })
        .collect::<Vec<_>>();
    let current_expiry = worker
        .active_jobs()
        .first()
        .map(|a| a.lease.expires_at.0.clone());
    let value = serde_json::json!({"mechanism":"M11","fixtureLeaseMillis":1200,"validationSleepSecondsSingleChangedCommand":seconds,"actualDriver":"WorkerMain::drive_with_controls","fakeAdapterBoundary":"FakeCodex supplies initial proposal and a pending follow-up; Worker, renewal, workspace validation and process isolation are actual","actualOutcomeCodes":outcome_codes,"actualValidation":"JobWorkspaceRuntime::execute_change_batch/run_validation_command/VerificationIsolation::run","queuedRenewalAtMillis":queued_ms,"driveReturnedAtMillis":drive_finished_ms,"controlReceipts":controls.trace.borrow().clone(),"driverError":format!("{result:?}"),"heartbeatMessagesDuringDrive":during_heartbeats,"humanReviewRequiredOutcome":human_review_outcome,"leaseExpiredOutcome":lease_expired,"currentExpiry":current_expiry,"heartbeatLimit":"Explicit actual heartbeat before drive; production outer-loop non-service during this exclusive drive is separately source-derived. Short synthetic lease is not replay of original 900-second incident."});
    println!("MECHANISM_RECEIPT {value}");
    value
}
#[tokio::test]
async fn mechanism_m11_actual_long_validation_delays_renewal_past_expiry() {
    let baseline = Box::pin(validation_case(0.0)).await;
    let slow = Box::pin(validation_case(2.0)).await;
    assert_eq!(
        baseline["leaseExpiredOutcome"], false,
        "fast baseline must not expire"
    );
    assert_eq!(
        baseline["currentExpiry"], "2027-01-15T08:00:10.000Z",
        "fast baseline must apply renewal and keep Job active"
    );
    assert_eq!(
        slow["humanReviewRequiredOutcome"], false,
        "expiry must be the exact competing terminal"
    );
    assert_eq!(
        slow["leaseExpiredOutcome"], true,
        "slow actual validation must reproduce late renewal rejection"
    );
    assert!(slow["driveReturnedAtMillis"].as_u64().unwrap() > 1200);
}
#[tokio::test]
#[ignore = "mechanism audit: selected 300ms renewal service budget is an explicit cost baseline"]
async fn mechanism_m11_driver_renewal_service_bound_red() {
    let slow = Box::pin(validation_case(2.0)).await;
    assert!(
        slow["controlReceipts"][0][1].as_u64().unwrap() < 300,
        "renewal must be serviced during long validation; actual receipt={slow}"
    );
}
