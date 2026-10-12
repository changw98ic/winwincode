// SPDX-License-Identifier: Apache-2.0
use super::*;
use winwincode_worker::control_driver::WorkerControlSource;
use winwincode_worker::workspace::WorkspaceCloseReason;

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

fn validation_start_marker_count(root: &Path) -> usize {
    let mut directories = vec![root.to_path_buf()];
    let mut count = 0;
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                directories.push(entry.path());
            } else if kind.is_file() && entry.file_name() == "validation-started" {
                count += 1;
            }
        }
    }
    count
}
async fn validation_case_control(seconds: f64, cancel: bool) -> serde_json::Value {
    validation_case_control_with_cleanup_fault(seconds, cancel, false).await
}

async fn validation_case_control_with_cleanup_fault(
    seconds: f64,
    cancel: bool,
    cleanup_fault: bool,
) -> serde_json::Value {
    validation_case_control_with_store_fault(
        seconds,
        cancel,
        cleanup_fault,
        ValidationStoreFaults::default(),
    )
    .await
}

#[derive(Clone, Copy, Default)]
struct ValidationStoreFaults {
    diagnostic: bool,
    clear: bool,
    pending: bool,
}

async fn validation_case_control_with_store_fault(
    seconds: f64,
    cancel: bool,
    cleanup_fault: bool,
    store_faults: ValidationStoreFaults,
) -> serde_json::Value {
    let ValidationStoreFaults {
        diagnostic: diagnostic_fault,
        clear: clear_fault,
        pending: pending_fault,
    } = store_faults;
    let (workspaces_root, sources) = test_workspace_paths();
    let repository = sources.join(id("rpo", 'A'));
    std::fs::create_dir_all(repository.join(".winwincode")).unwrap();
    let config = PASSING_VALIDATION_CONFIG
        .replace("diagnosticParserVersion = \"typescript_v1\"", "")
        .replacen(
            "raise SystemExit(0)",
            &format!("import os, time; from pathlib import Path; Path(os.environ['WWC_OUTPUT_DIR']).joinpath('validation-started').write_text(str(os.getpid())); time.sleep({seconds}); raise SystemExit(0)"),
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
    let workspaces = JobWorkspaceRuntime::open(workspaces_root.clone(), sources.clone())
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
    if cleanup_fault {
        worker
            .workspace_runtime_for_test()
            .force_next_validation_cleanup_failure_for_test();
    }
    if pending_fault {
        worker
            .workspace_runtime_for_test()
            .fail_validation_cleanup_pending_for_test()
            .unwrap();
    }
    if diagnostic_fault {
        worker
            .workspace_runtime_for_test()
            .fail_validation_cleanup_diagnostic_for_test()
            .unwrap();
    }
    if clear_fault {
        worker
            .workspace_runtime_for_test()
            .fail_validation_cleanup_clear_for_test()
            .unwrap();
    }
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
    let accepted_revision_before = worker
        .workspace_runtime_for_test()
        .accepted_revision(&active.job.job_id)
        .unwrap();
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
    let message = if cancel {
        ExecutionPortMessage::JobCancelMessage(cancel_for(&active, 'N'))
    } else {
        ExecutionPortMessage::LeaseRenewMessage(
            winwincode_execution_port::generated::LeaseRenewMessage {
                kind: winwincode_execution_port::generated::LeaseRenewMessageKind::LeaseRenew,
                lease: extended,
                message_id: ExecutionMessageId(id("xmsg", 'N')),
                prior_expires_at: active.lease.expires_at.clone(),
                request_id: RequestId(id("req", 'N')),
                schema_version: SchemaVersion::WinwincodeV1,
                sent_at: Instant("2027-01-15T08:00:02.100Z".into()),
            },
        )
    };
    let queued_controls = Arc::clone(&controls.queued);
    let enqueue = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        queued_controls
            .lock()
            .unwrap()
            .push_back((ExecutionMessageId(id("xmsg", 'N')), message));
        origin.elapsed().as_millis()
    });
    let mut max_drive_millis = 0;
    let fixture_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
    let result = loop {
        assert!(
            tokio::time::Instant::now() < fixture_deadline,
            "validation/control fixture deadline"
        );
        let drive_started = std::time::Instant::now();
        let result = worker
            .drive_with_controls(&controls, || Ok(elapsed_clock(origin)))
            .await;
        max_drive_millis = max_drive_millis.max(drive_started.elapsed().as_millis());
        if result.is_err() || result.as_ref().is_ok_and(Option::is_some) {
            break result;
        }
        if !worker.has_pending_workspace_validation() && !controls.trace.borrow().is_empty() {
            break result;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    };
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
    let mut repeated_cancel_rejected = false;
    let mut terminal_close_rejected = false;
    let mut restart_rejected = false;
    let mut durable_cleanup_failure = false;
    let mut durable_validation_still_pending = false;
    let mut durable_cleanup_cause = None;
    if cleanup_fault || diagnostic_fault || clear_fault {
        let repeat = worker
            .accept_control(
                &ExecutionPortMessage::JobCancelMessage(cancel_for(&active, 'N')),
                elapsed_clock(origin),
            )
            .await;
        repeated_cancel_rejected = repeat
            .as_ref()
            .is_err_and(|error| error.to_string().contains("VALIDATION_CLEANUP_UNCONFIRMED"));
        let cancelling = worker.active_jobs()[0].clone();
        let terminal = worker
            .workspace_runtime_for_test()
            .prepare_terminal_job_close(
                &cancelling,
                WorkspaceCloseReason::Cancelled,
                &elapsed_clock(origin),
            );
        terminal_close_rejected = terminal
            .as_ref()
            .is_err_and(|error| error.cause_code() == "VALIDATION_CLEANUP_UNCONFIRMED");
        let store = winwincode_worker::change_batch_store::ChangeBatchStore::open(
            workspaces_root.parent().unwrap(),
        )
        .unwrap();
        let diagnostic = store
            .validation_cleanup_record(&active.job.job_id)
            .unwrap()
            .unwrap();
        durable_cleanup_failure = diagnostic.identity.job_id == active.job.job_id
            && diagnostic.identity.session_identity == active.session_identity
            && matches!(
                diagnostic.cause_code.as_str(),
                "VALIDATION_CLEANUP_UNCONFIRMED" | "VALIDATION_CLEANUP_PENDING"
            );
        durable_cleanup_cause = Some(diagnostic.cause_code.clone());
        let binding = store
            .workspace_binding(&diagnostic.workspace_id)
            .unwrap()
            .unwrap();
        durable_validation_still_pending = binding.state
            == winwincode_worker::change_batch_store::BatchState::ValidationPending
            && binding.active_batch_id.as_ref() == Some(&diagnostic.identity.batch_id);
        let mut restarted =
            JobWorkspaceRuntime::open(workspaces_root.clone(), sources.clone()).unwrap();
        restart_rejected = restarted
            .open_for_job_recovering(&active, None, &elapsed_clock(origin))
            .is_err_and(|error| error.cause_code() == "VALIDATION_CLEANUP_UNCONFIRMED");
        worker.flush_durable_outbox().await.unwrap();
    }
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
    let accepted_cancel_ack = messages.borrow().iter().any(|message| {
        matches!(message,
        ExecutionPortMessage::JobCancelAckMessage(ack)
            if ack.status == JobCancelAckMessageStatus::Accepted
                && ack.request_id.0 == id("req", 'N')
                && ack.lease == active.lease
                && ack.worker_session_id == active.worker_session_id
                && ack.session_identity == active.session_identity)
    });
    let positive_cancel_ack_count = messages.borrow().iter().filter(|message| {
        matches!(message,
        ExecutionPortMessage::JobCancelAckMessage(ack)
            if matches!(ack.status, JobCancelAckMessageStatus::Accepted | JobCancelAckMessageStatus::AlreadyCancelling)
                && ack.lease == active.lease
                && ack.worker_session_id == active.worker_session_id
                && ack.session_identity == active.session_identity)
    }).count();
    let interrupt_count = pump
        .calls()
        .iter()
        .filter(|call| *call == &format!("interrupt:{}", active.codex_thread_id.0))
        .count();
    let workspace_retained = worker
        .workspace_runtime_for_test()
        .contains(&active.job.job_id);
    let active_lifecycle = worker
        .active_jobs()
        .first()
        .map(|job| format!("{:?}", job.lifecycle));
    let accepted_revision_after = worker
        .workspace_runtime_for_test()
        .accepted_revision(&active.job.job_id)
        .unwrap();
    let mut value = serde_json::json!({"mechanism":"M11","fixtureLeaseMillis":1200,"validationSleepSecondsSingleChangedCommand":seconds,"actualDriver":"WorkerMain::drive_with_controls","fakeAdapterBoundary":"FakeCodex supplies initial proposal and a pending follow-up; Worker, renewal, workspace validation and process isolation are actual","actualOutcomeCodes":outcome_codes,"actualValidation":"JobWorkspaceRuntime::execute_change_batch/run_validation_command/VerificationIsolation::run","queuedRenewalAtMillis":queued_ms,"driveReturnedAtMillis":drive_finished_ms,"maxSingleDriveMillis":max_drive_millis,"cancelRequested":cancel,"cleanupConfirmationFault":cleanup_fault,"cleanupPendingWriteFault":pending_fault,"ownedValidationTaskCount":worker.workspace_runtime_for_test().pending_validation_task_count_for_test(),"validationStartMarkerCount":validation_start_marker_count(&workspaces_root),"acceptedRevisionBefore":accepted_revision_before.0,"acceptedRevisionAfter":accepted_revision_after.0,"cleanupDiagnosticWriteFault":diagnostic_fault,"cleanupClearWriteFault":clear_fault,"durableCleanupCause":durable_cleanup_cause,"repeatedCancelRejected":repeated_cancel_rejected,"terminalCloseRejected":terminal_close_rejected,"restartRejected":restart_rejected,"durableCleanupFailure":durable_cleanup_failure,"durableValidationStillPending":durable_validation_still_pending,"workspaceRetained":workspace_retained,"activeLifecycle":active_lifecycle,"acceptedExactCancelAck":accepted_cancel_ack,"positiveCancelAckCount":positive_cancel_ack_count,"exactCoreInterruptCount":interrupt_count,"remainingValidation":worker.has_pending_workspace_validation(),"controlReceipts":controls.trace.borrow().clone(),"driverError":format!("{result:?}"),"heartbeatMessagesDuringDrive":during_heartbeats,"humanReviewRequiredOutcome":human_review_outcome,"leaseExpiredOutcome":lease_expired,"currentExpiry":current_expiry,"heartbeatLimit":"Actual Worker driver sends the negotiated heartbeat while owned validation runs. Short synthetic lease is not a replay of the original 900-second incident."});
    if !cleanup_fault && !diagnostic_fault && !clear_fault && !pending_fault {
        let recovering_active = worker.active_jobs()[0].clone();
        drop(worker);
        let store = winwincode_worker::change_batch_store::ChangeBatchStore::open(
            workspaces_root.parent().unwrap(),
        )
        .unwrap();
        value["normalCleanupMarkerCleared"] = store
            .validation_cleanup_record(&recovering_active.job.job_id)
            .unwrap()
            .is_none()
            .into();
        let mut restarted = JobWorkspaceRuntime::open(workspaces_root, sources).unwrap();
        value["normalReopenAccepted"] = restarted
            .open_for_job_recovering(&recovering_active, None, &elapsed_clock(origin))
            .is_ok()
            .into();
    }
    println!("MECHANISM_RECEIPT {value}");
    value
}
#[tokio::test]
async fn mechanism_m11_actual_long_validation_preserves_renewal_and_heartbeat() {
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
        slow["leaseExpiredOutcome"], false,
        "owned validation must allow renewal before expiry"
    );
    assert!(slow["driveReturnedAtMillis"].as_u64().unwrap() > 1200);
    assert_eq!(slow["currentExpiry"], "2027-01-15T08:00:10.000Z");
    assert!(slow["heartbeatMessagesDuringDrive"].as_u64().unwrap() > 0);
    assert!(slow["validationStartMarkerCount"].as_u64().unwrap() > 0);
    assert_eq!(slow["normalCleanupMarkerCleared"], true);
    assert_eq!(slow["normalReopenAccepted"], true);
}
#[tokio::test]
async fn mechanism_m11_driver_renewal_service_bound_red() {
    let slow = Box::pin(validation_case(2.0)).await;
    assert!(
        slow["controlReceipts"][0][1].as_u64().unwrap() < 300,
        "renewal must be serviced during long validation; actual receipt={slow}"
    );
}

async fn validation_case(seconds: f64) -> serde_json::Value {
    validation_case_control(seconds, false).await
}

#[tokio::test]
async fn mechanism_m11_actual_long_validation_services_remote_cancel() {
    let slow = validation_case_control(2.0, true).await;
    assert!(
        slow["controlReceipts"][0][1].as_u64().unwrap() < 300,
        "remote cancellation must be durably consumed before validation completes: {slow}"
    );
    assert_eq!(slow["remainingValidation"], false);
    assert_eq!(slow["acceptedExactCancelAck"], true);
    assert_eq!(slow["exactCoreInterruptCount"], 1);
    assert_eq!(slow["normalCleanupMarkerCleared"], true);
    assert_eq!(slow["normalReopenAccepted"], true);
    assert_eq!(
        slow["controlReceipts"][0][0],
        format!("confirmed:{}", id("xmsg", 'N'))
    );
    assert!(
        slow["driveReturnedAtMillis"].as_u64().unwrap() < 1200,
        "cancellation must stop its owned validation promptly: {slow}"
    );
}

#[tokio::test]
async fn mechanism_m11_cleanup_confirmation_failure_preserves_the_cancelled_workspace() {
    let failed = validation_case_control_with_cleanup_fault(2.0, true, true).await;
    assert_eq!(failed["cleanupConfirmationFault"], true);
    assert_eq!(failed["acceptedExactCancelAck"], false);
    assert_eq!(failed["positiveCancelAckCount"], 0);
    assert_eq!(failed["exactCoreInterruptCount"], 1);
    assert_eq!(failed["workspaceRetained"], true);
    assert_eq!(failed["activeLifecycle"], "Cancelling");
    assert_eq!(failed["remainingValidation"], true);
    assert_eq!(failed["repeatedCancelRejected"], true);
    assert_eq!(failed["terminalCloseRejected"], true);
    assert_eq!(failed["restartRejected"], true);
    assert_eq!(failed["durableCleanupFailure"], true);
    assert_eq!(failed["durableValidationStillPending"], true);
    assert!(
        failed["driverError"]
            .as_str()
            .unwrap()
            .contains("VALIDATION_CLEANUP_UNCONFIRMED"),
        "{failed}"
    );
    assert!(
        failed["actualOutcomeCodes"].as_array().unwrap().is_empty(),
        "cleanup failure must not fabricate a terminal result: {failed}"
    );
}

#[tokio::test]
async fn mechanism_m11_cleanup_diagnostic_write_failure_keeps_the_pre_spawn_barrier() {
    let failed = validation_case_control_with_store_fault(
        2.0,
        true,
        true,
        ValidationStoreFaults {
            diagnostic: true,
            clear: false,
            pending: false,
        },
    )
    .await;
    assert_eq!(failed["durableCleanupCause"], "VALIDATION_CLEANUP_PENDING");
    assert_eq!(failed["restartRejected"], true);
    assert_eq!(failed["repeatedCancelRejected"], true);
    assert_eq!(failed["terminalCloseRejected"], true);
    assert_eq!(failed["positiveCancelAckCount"], 0);
    assert_eq!(failed["workspaceRetained"], true);
    assert_eq!(failed["exactCoreInterruptCount"], 1);
    assert!(
        failed["driverError"]
            .as_str()
            .unwrap()
            .contains("VALIDATION_CLEANUP_RECORD_UNAVAILABLE"),
        "{failed}"
    );
    assert!(failed["actualOutcomeCodes"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn mechanism_m11_cleanup_clear_write_failure_keeps_the_confirmed_workspace_blocked() {
    let failed = validation_case_control_with_store_fault(
        2.0,
        true,
        false,
        ValidationStoreFaults {
            diagnostic: false,
            clear: true,
            pending: false,
        },
    )
    .await;
    assert_eq!(failed["durableCleanupCause"], "VALIDATION_CLEANUP_PENDING");
    assert_eq!(failed["restartRejected"], true);
    assert_eq!(failed["repeatedCancelRejected"], true);
    assert_eq!(failed["terminalCloseRejected"], true);
    assert_eq!(failed["positiveCancelAckCount"], 0);
    assert_eq!(failed["workspaceRetained"], true);
    assert_eq!(failed["exactCoreInterruptCount"], 1);
    assert!(
        failed["driverError"]
            .as_str()
            .unwrap()
            .contains("VALIDATION_CLEANUP_CLEAR_UNAVAILABLE"),
        "{failed}"
    );
    assert!(failed["actualOutcomeCodes"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn mechanism_m11_pending_marker_write_failure_prevents_validation_spawn() {
    let failed = validation_case_control_with_store_fault(
        2.0,
        false,
        false,
        ValidationStoreFaults {
            diagnostic: false,
            clear: false,
            pending: true,
        },
    )
    .await;
    assert!(
        failed["driverError"]
            .as_str()
            .unwrap()
            .contains("VALIDATION_CLEANUP_PENDING_UNAVAILABLE"),
        "{failed}"
    );
    assert_eq!(failed["ownedValidationTaskCount"], 0);
    assert_eq!(failed["validationStartMarkerCount"], 0);
    assert_eq!(failed["workspaceRetained"], true);
    assert_eq!(failed["activeLifecycle"], "Running");
    assert_eq!(failed["positiveCancelAckCount"], 0);
    assert_eq!(failed["exactCoreInterruptCount"], 0);
    assert_eq!(
        failed["acceptedRevisionAfter"],
        failed["acceptedRevisionBefore"]
    );
    assert!(failed["actualOutcomeCodes"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn mechanism_m11_natural_completion_clear_failure_keeps_the_checkpoint_blocked() {
    let failed = validation_case_control_with_store_fault(
        2.0,
        false,
        false,
        ValidationStoreFaults {
            diagnostic: false,
            clear: true,
            pending: false,
        },
    )
    .await;
    assert_eq!(failed["durableCleanupCause"], "VALIDATION_CLEANUP_PENDING");
    assert_eq!(failed["restartRejected"], true);
    assert_eq!(failed["terminalCloseRejected"], true);
    assert_eq!(failed["positiveCancelAckCount"], 0);
    assert_eq!(failed["workspaceRetained"], true);
    assert_eq!(
        failed["acceptedRevisionAfter"],
        failed["acceptedRevisionBefore"]
    );
    assert!(failed["validationStartMarkerCount"].as_u64().unwrap() > 0);
    assert!(
        failed["driverError"]
            .as_str()
            .unwrap()
            .contains("VALIDATION_CLEANUP_CLEAR_UNAVAILABLE"),
        "{failed}"
    );
    assert!(failed["actualOutcomeCodes"].as_array().unwrap().is_empty());
}
