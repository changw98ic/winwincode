// SPDX-License-Identifier: Apache-2.0

// The first attempt uses the scheduler's attempt fencing token. All dispatch,
// retry, receipt and append operations below run through production storage.
#[allow(
    clippy::too_many_lines,
    reason = "the production ingress fixture keeps the durable Job, Registry claim, and accepted dispatch visibly exact"
)]
fn install_runtime_successor_dispatch(fixture: &mut Fixture, seed: u64) -> ProductDispatch {
    let register = worker_register();
    fixture
        .accept(
            &ExecutionPortMessage::WorkerRegisterMessage(register.clone()),
            register.sent_at.clone(),
        )
        .expect("Worker registration");
    let product_session_id = ProductSessionId(canonical_id("psn", seed));
    let job = ExecutionJob {
        attachments: None,
        model_selection: None,
        attempt: 1,
        execution_profile: "codex".to_owned(),
        goal: "Advance ProductSession chat".to_owned(),
        job_id: ExecutionJobId(canonical_id("job", seed)),
        limits: ExecutionLimits {
            deadline_at: Some(Instant("2027-01-15T09:00:00.000Z".to_owned())),
            max_artifact_bytes: 10_000_000,
            max_runtime_seconds: Some(3_600),
        },
        payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        scope: ExecutionScope::ProductSessionExecutionScope(ProductSessionExecutionScope {
            kind: ProductSessionExecutionScopeKind::ProductSession,
            product_session_id: product_session_id.clone(),
        }),
        work_input: None,
        workspace: ExecutionWorkspace {
            checkout_revision: "fixture-checkout".to_owned(),
            repository_id: fixture.scope.repository_id.clone(),
            write_mode: ExecutionWorkspaceWriteMode::Candidate,
        },
    };
    fixture
        .control_plane
        .commit(
            &CommandEnvelope {
                actor: Actor::UserActor(UserActor {
                    id: UserId(canonical_id("usr", seed)),
                    kind: winwincode_domain::UserActorKind::User,
                }),
                command: CommandName::SessionCancel,
                expected_revision: Revision(0),
                payload: serde_json::json!({"productSessionId": product_session_id}),
                request_id: RequestId(canonical_id("req", seed)),
                schema_version: SchemaVersion::WinwincodeV1,
                scope: Scope::RepositoryScope(fixture.scope.clone()),
            },
            StateChange::new(
                product_session_catalog_stream_id(&fixture.scope),
                b"product-session-state".to_vec(),
                vec![NewOutboxEvent::internal(
                    format!("execution-job:{}", job.job_id.0),
                    "execution.job.dispatch",
                    serde_json::to_vec(&job).expect("job JSON"),
                )],
            ),
        )
        .expect("durable ProductSession job");
    let claim = ExecutionLeaseClaim {
        expires_at: Instant("2027-01-15T08:05:00.000Z".to_owned()),
        fencing_token: FencingToken("1".to_owned()),
        issued_at: Instant("2027-01-15T08:00:00.200Z".to_owned()),
        job_id: job.job_id.clone(),
        lease_id: LeaseId(canonical_id("lse", seed)),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 1)),
        payload_digest: job.payload_digest.clone(),
        request_id: RequestId(canonical_id("req", seed + 1)),
        worker_id: register.worker_id,
        worker_instance_id: register.worker_instance_id,
        attempt: 1,
    };
    seed_queue_lease(
        &mut fixture.storage,
        &fixture.scope,
        &job,
        &claim,
        seed,
        None,
    );
    fixture
        .storage
        .execution_registry()
        .expect("registry")
        .claim_execution_job(&claim)
        .expect("lease claim");
    let worker_session_id = WorkerSessionId(canonical_id("wsn", seed));
    let lease = ExecutionLeaseStamp {
        attempt: 1,
        expires_at: claim.expires_at,
        fencing_token: claim.fencing_token,
        issued_at: claim.issued_at,
        job_id: claim.job_id,
        lease_id: claim.lease_id,
        worker_id: claim.worker_id,
        worker_instance_id: claim.worker_instance_id,
    };
    let result = JobDispatchResultMessage {
        error: None,
        job_id: job.job_id.clone(),
        kind: JobDispatchResultMessageKind::JobDispatchResult,
        lease: lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 2)),
        payload_digest: job.payload_digest.clone(),
        request_id: claim.request_id,
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:01.000Z".to_owned()),
        status: JobDispatchResultMessageStatus::Accepted,
        worker_session_id: Some(worker_session_id.clone()),
    };
    fixture
        .accept(
            &ExecutionPortMessage::JobDispatchResultMessage(result),
            Instant("2027-01-15T08:00:01.100Z".to_owned()),
        )
        .expect("accepted dispatch");
    ProductDispatch {
        job,
        lease,
        product_session_id,
        worker_session_id,
    }
}

fn successor_runtime_message(
    dispatch: &ProductDispatch,
    seed: u64,
    sequence: i64,
) -> RuntimeEventMessage {
    let thread_digest = format!(
        "{:x}",
        Sha256::digest(dispatch.worker_session_id.0.as_bytes())
    );
    let thread = CodexThreadId(format!("cdx_{}", thread_digest[..26].to_ascii_uppercase()));
    let time = if dispatch.job.attempt == 1 {
        Instant("2027-01-15T08:00:03.000Z".into())
    } else {
        Instant("2027-01-15T08:00:07.000Z".into())
    };
    RuntimeEventMessage {
        codex_thread_id: thread.clone(),
        event: ExecutionEventRecord {
            category: ExecutionEventCategory::Lifecycle,
            event_id: ExecutionEventId(canonical_id("xevt", seed)),
            occurred_at: time.clone(),
            payload: None,
            sequence: ExecutionSequence(sequence),
            summary: format!("attempt {} runtime started", dispatch.job.attempt),
        },
        kind: RuntimeEventMessageKind::RuntimeEvent,
        lease: dispatch.lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: time,
        session_identity: SessionIdentity {
            codex_thread_id: thread,
            product_session_id: dispatch.product_session_id.clone(),
            worker_session_id: dispatch.worker_session_id.clone(),
            work_run_id: None,
        },
        worker_session_id: dispatch.worker_session_id.clone(),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the successor fixture uses real failure, scheduler retry and accepted dispatch"
)]
fn prepare_runtime_successor(
    fixture: &mut Fixture,
    old: &ProductDispatch,
    seed: u64,
) -> (ProductDispatch, JobDispatchResultMessage) {
    prepare_workrun_admission(fixture, &old.job, None, seed);
    let revision = fixture
        .storage
        .load_execution_job_record(&old.job.job_id)
        .unwrap()
        .unwrap()
        .revision;
    fixture
        .storage
        .execution_queue()
        .unwrap()
        .transition(&ExecutionJobTransitionRequest {
            scope: ExecutionQueueScope {
                organization_id: fixture.scope.organization_id.clone(),
                workspace_id: fixture.scope.workspace_id.clone(),
                project_id: fixture.scope.project_id.clone(),
                repository_id: fixture.scope.repository_id.clone(),
                product_session_id: old.product_session_id.clone(),
                delivery_id: None,
            },
            job_id: old.job.job_id.clone(),
            request_id: RequestId(canonical_id("req", seed + 30)),
            expected_revision: revision,
            from: ExecutionJobState::Running,
            to: ExecutionJobState::Failed,
            occurred_at: Instant("2027-01-15T08:00:04.000Z".into()),
        })
        .unwrap();
    fixture
        .storage
        .execution_registry()
        .unwrap()
        .finish_execution_lease(&ExecutionLeaseTerminalRequest {
            job_id: old.job.job_id.clone(),
            lease_id: old.lease.lease_id.clone(),
            worker_id: old.lease.worker_id.clone(),
            worker_instance_id: old.lease.worker_instance_id.clone(),
            attempt: 1,
            fencing_token: old.lease.fencing_token.clone(),
            outcome: ExecutionLeaseTerminalOutcome::Failed,
            terminal_at: Instant("2027-01-15T08:00:04.000Z".into()),
            request_id: RequestId(canonical_id("req", seed + 31)),
        })
        .unwrap();
    let mut register = worker_register();
    register.worker_id = old.lease.worker_id.clone();
    register.worker_instance_id = WorkerInstanceId(canonical_id("wki", seed + 1));
    register.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 32));
    register.request_id = RequestId(canonical_id("req", seed + 32));
    register.sent_at = Instant("2027-01-15T08:00:04.100Z".into());
    register.started_at = register.sent_at.clone();
    fixture
        .accept(
            &ExecutionPortMessage::WorkerRegisterMessage(register.clone()),
            register.sent_at.clone(),
        )
        .unwrap();
    let ExecutionPortMessage::WorkerHeartbeatMessage(mut heartbeat) =
        execution_message("worker.heartbeat")
    else {
        panic!("heartbeat")
    };
    heartbeat.worker_id = register.worker_id.clone();
    heartbeat.worker_instance_id = register.worker_instance_id.clone();
    heartbeat.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 33));
    heartbeat.sent_at = Instant("2027-01-15T08:00:04.200Z".into());
    heartbeat.observed_at = heartbeat.sent_at.clone();
    heartbeat.capacity = WorkerCapacity {
        running_jobs: 0,
        available_slots: 4,
    };
    heartbeat.active_leases.clear();
    fixture
        .accept(
            &ExecutionPortMessage::WorkerHeartbeatMessage(heartbeat.clone()),
            heartbeat.sent_at,
        )
        .unwrap();
    let retry = RepositoryExecutionScheduler::new(&mut fixture.storage)
        .retry_failed(&RepositorySchedulerRetryRequest {
            scope: RepositorySchedulerScope {
                organization_id: fixture.scope.organization_id.clone(),
                workspace_id: fixture.scope.workspace_id.clone(),
                project_id: fixture.scope.project_id.clone(),
                repository_id: fixture.scope.repository_id.clone(),
            },
            job_id: old.job.job_id.clone(),
            request_id: RequestId(canonical_id("req", seed + 34)),
            scheduler_generation: "runtime-successor-fixture".into(),
            worker_id: register.worker_id,
            worker_instance_id: register.worker_instance_id,
            retryable_failure: true,
            failed_at_tick: 1,
            now_tick: 10,
            policy: SchedulerRetryPolicy {
                max_attempts: 3,
                initial_backoff_ticks: 1,
                max_backoff_ticks: 2,
            },
            issued_at: Instant("2027-01-15T08:00:05.000Z".into()),
            expires_at: Instant("2027-01-15T08:10:00.000Z".into()),
        })
        .unwrap()
        .unwrap();
    assert_eq!(retry.job.attempt, 2);
    let lease = ExecutionLeaseStamp {
        attempt: 2,
        expires_at: retry.lease.expires_at,
        fencing_token: retry.lease.fencing_token,
        issued_at: retry.lease.issued_at,
        job_id: retry.lease.job_id,
        lease_id: retry.lease.lease_id,
        worker_id: retry.lease.worker_id,
        worker_instance_id: retry.lease.worker_instance_id,
    };
    let worker_session_id = WorkerSessionId(canonical_id("wsn", seed + 1));
    let result = JobDispatchResultMessage {
        error: None,
        job_id: retry.job.job_id.clone(),
        kind: JobDispatchResultMessageKind::JobDispatchResult,
        lease: lease.clone(),
        message_id: ExecutionMessageId(canonical_id("xmsg", seed + 35)),
        payload_digest: retry.job.payload_digest.clone(),
        request_id: retry.request_id,
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2027-01-15T08:00:06.000Z".into()),
        status: JobDispatchResultMessageStatus::Accepted,
        worker_session_id: Some(worker_session_id.clone()),
    };
    (
        ProductDispatch {
            job: retry.job,
            lease,
            worker_session_id,
            product_session_id: old.product_session_id.clone(),
        },
        result,
    )
}

fn runtime_successor_ack(
    fixture: &mut Fixture,
    message: &RuntimeEventMessage,
) -> winwincode_execution_port::generated::RuntimeAckMessage {
    runtime_successor_ack_at(fixture, message, message.sent_at.clone())
}

fn runtime_successor_ack_at(
    fixture: &mut Fixture,
    message: &RuntimeEventMessage,
    server_time: Instant,
) -> winwincode_execution_port::generated::RuntimeAckMessage {
    let result = fixture
        .accept(
            &ExecutionPortMessage::RuntimeEventMessage(message.clone()),
            server_time,
        )
        .unwrap();
    let [ExecutionPortMessage::RuntimeAckMessage(ack)] = result.as_slice() else {
        panic!("runtime ACK")
    };
    ack.clone()
}

type RuntimeSuccessorWriteSnapshot = (Vec<(String, i64, Vec<u8>)>, Vec<i64>);

fn runtime_successor_write_snapshot(fixture: &Fixture) -> RuntimeSuccessorWriteSnapshot {
    let connection = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
    let rows = connection
        .prepare("SELECT stream_id, revision, payload FROM product_state ORDER BY stream_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let counts = [
        "SELECT COUNT(*) FROM command_receipts",
        "SELECT COUNT(*) FROM outbox",
        "SELECT COUNT(*) FROM audit_outbox",
    ]
    .into_iter()
    .map(|sql| connection.query_row(sql, [], |row| row.get(0)).unwrap())
    .collect();
    (rows, counts)
}

fn runtime_successor_stream(dispatch: &ProductDispatch) -> String {
    crate::runtime_event_transaction::product_session_runtime_stream_id_for_projection(
        &dispatch.product_session_id,
    )
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the real successor checks gap, old immutable facts, receipt replay, renewal and restart"
)]
fn runtime_successor_uses_current_seal_and_preserves_attempt_local_history_after_restart() {
    for size in [1_u64, 64] {
        let seed = 8_500_000 + size * 100;
        let mut fixture = Fixture::open("runtime-successor-current", seed);
        let old = install_runtime_successor_dispatch(&mut fixture, seed);
        let original = successor_runtime_message(&old, seed + 1_000, 1);
        for sequence in 1..=size {
            let last = successor_runtime_message(
                &old,
                seed + 999 + sequence,
                i64::try_from(sequence).unwrap(),
            );
            assert_eq!(
                runtime_successor_ack(&mut fixture, &last).status,
                LeaseWriteStatus::Accepted
            );
        }
        let stream = runtime_successor_stream(&old);
        let old_header = load_ledger(&fixture.storage, &stream).unwrap().unwrap();
        let original_row_key = format!("runtime-ledger-record:{}:{:020}", old.job.job_id.0, 1);
        let original_marker_key = format!(
            "runtime-ledger-identity:{}:{}",
            old.job.job_id.0, original.event.event_id.0
        );
        let original_row = fixture
            .storage
            .load_state(&original_row_key)
            .unwrap()
            .unwrap();
        let original_marker = fixture
            .storage
            .load_state(&original_marker_key)
            .unwrap()
            .unwrap();
        let (successor, result) = prepare_runtime_successor(&mut fixture, &old, seed);
        let mut first = successor_runtime_message(&successor, seed + 2_000, 1);
        let before = runtime_successor_write_snapshot(&fixture);
        let unaccepted = fixture.accept(
            &ExecutionPortMessage::RuntimeEventMessage(first.clone()),
            first.sent_at.clone(),
        );
        assert!(unaccepted.is_err() || unaccepted.as_ref().unwrap().iter().all(|message| matches!(message, ExecutionPortMessage::RuntimeAckMessage(ack) if ack.status == LeaseWriteStatus::RejectedConflict)));
        assert_eq!(runtime_successor_write_snapshot(&fixture), before);
        fixture
            .accept(
                &ExecutionPortMessage::JobDispatchResultMessage(result.clone()),
                result.sent_at,
            )
            .unwrap();
        let mut gap = first.clone();
        gap.event.sequence = ExecutionSequence(2);
        let before = runtime_successor_write_snapshot(&fixture);
        let ack = runtime_successor_ack(&mut fixture, &gap);
        assert_eq!(ack.status, LeaseWriteStatus::Gap);
        assert_eq!(ack.ack_sequence.0, 0);
        assert_eq!(ack.replay_from_sequence, Some(ExecutionSequence(1)));
        assert_eq!(runtime_successor_write_snapshot(&fixture), before);
        crate::storage_mechanism_regression::reset();
        let ack = runtime_successor_ack(&mut fixture, &first);
        assert_eq!(ack.status, LeaseWriteStatus::Accepted, "{:?}", ack.error);
        assert_eq!(ack.ack_sequence.0, 1);
        let metrics = crate::storage_mechanism_regression::finish();
        assert_eq!(metrics.old_event_validations, 0);
        assert_eq!(
            fixture
                .storage
                .load_state(&original_row_key)
                .unwrap()
                .unwrap(),
            original_row
        );
        assert_eq!(
            fixture
                .storage
                .load_state(&original_marker_key)
                .unwrap()
                .unwrap(),
            original_marker
        );
        let mut fixture = fixture.restart();
        let mut current = load_ledger(&fixture.storage, &stream).unwrap().unwrap();
        assert_eq!(current.attempt, 2);
        assert_eq!(current.highest_sequence, 1);
        assert_eq!(current.sequence_offset, old_header.revision());
        assert_eq!(current.indexed_sequence_offset, size);
        assert_eq!(current.worker_session_id, successor.worker_session_id);
        assert_eq!(current.codex_thread_id, first.codex_thread_id);
        crate::storage_mechanism_regression::reset();
        hydrate_runtime_ledger(&fixture.storage, &mut current).unwrap();
        assert_eq!(
            crate::storage_mechanism_regression::finish().old_event_validations,
            1
        );
        assert_eq!(current.events.len(), 1);
        assert_eq!(current.events[0].event, first.event);
        let mut history = old_header;
        hydrate_runtime_ledger(&fixture.storage, &mut history).unwrap();
        assert_eq!(history.events.len() as u64, size);
        assert_eq!(history.events[0].event, original.event);
        let before = runtime_successor_write_snapshot(&fixture);
        crate::storage_mechanism_regression::reset();
        let replay = runtime_successor_ack_at(
            &mut fixture,
            &original,
            Instant("2027-01-15T08:16:00.000Z".into()),
        );
        assert_eq!(replay.status, LeaseWriteStatus::Duplicate);
        assert_eq!(replay.ack_sequence.0, 1);
        assert_eq!(replay.lease, original.lease);
        assert_eq!(replay.session_identity, original.session_identity);
        assert_eq!(
            crate::storage_mechanism_regression::finish().ledger_reads,
            0
        );
        assert_eq!(runtime_successor_write_snapshot(&fixture), before);
        let renewal = winwincode_storage::ExecutionLeaseRenewal {
            expires_at: Instant("2027-01-15T08:15:00.000Z".into()),
            prior_expires_at: successor.lease.expires_at.clone(),
            sent_at: Instant("2027-01-15T08:09:00.000Z".into()),
            job_id: successor.job.job_id.clone(),
            lease_id: successor.lease.lease_id.clone(),
            worker_id: successor.lease.worker_id.clone(),
            worker_instance_id: successor.lease.worker_instance_id.clone(),
            attempt: 2,
            fencing_token: successor.lease.fencing_token.clone(),
            request_id: RequestId(canonical_id("req", seed + 36)),
            message_id: ExecutionMessageId(canonical_id("xmsg", seed + 36)),
        };
        assert_eq!(
            fixture
                .storage
                .execution_registry()
                .unwrap()
                .renew_execution_lease(&renewal)
                .unwrap()
                .status,
            winwincode_storage::LeaseWriteStatus::Accepted
        );
        first.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 2_001));
        first.event.event_id = ExecutionEventId(canonical_id("xevt", seed + 2_001));
        first.event.sequence = ExecutionSequence(2);
        first.sent_at = Instant("2027-01-15T08:11:00.000Z".into());
        first.event.occurred_at = first.sent_at.clone();
        // The original accepted lease period remains authority after renewal.
        assert_eq!(
            runtime_successor_ack(&mut fixture, &first).status,
            LeaseWriteStatus::Accepted
        );
        let mut again = load_ledger(&fixture.storage, &stream).unwrap().unwrap();
        hydrate_runtime_ledger(&fixture.storage, &mut again).unwrap();
        assert_eq!(again.highest_sequence, 2);
        assert_eq!(again.indexed_sequence_offset, size);
        assert_eq!(again.events[1].event.sequence.0, 2);
        fixture.close();
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "rejected fields share exact zero-write assertions"
)]
fn runtime_successor_rejects_new_stale_foreign_and_fabricated_facts_without_writes() {
    let seed = 8_600_000;
    let mut fixture = Fixture::open("runtime-successor-zero-write", seed);
    let old = install_runtime_successor_dispatch(&mut fixture, seed);
    let original = successor_runtime_message(&old, seed + 1_000, 1);
    assert_eq!(
        runtime_successor_ack(&mut fixture, &original).status,
        LeaseWriteStatus::Accepted
    );
    let (successor, result) = prepare_runtime_successor(&mut fixture, &old, seed);
    fixture
        .accept(
            &ExecutionPortMessage::JobDispatchResultMessage(result.clone()),
            result.sent_at,
        )
        .unwrap();
    let first = successor_runtime_message(&successor, seed + 2_000, 1);
    assert_eq!(
        runtime_successor_ack(&mut fixture, &first).status,
        LeaseWriteStatus::Accepted
    );
    for variant in [
        "old-owner",
        "attempt",
        "fence",
        "stale-fence",
        "lease",
        "instance",
        "session",
        "thread",
        "product",
        "old-event",
        "changed-receipt",
        "gap",
    ] {
        let mut message = successor_runtime_message(&successor, seed + 2_001, 2);
        match variant {
            "old-owner" => {
                message = original.clone();
                message.message_id = ExecutionMessageId(canonical_id("xmsg", seed + 3_000));
                message.event.event_id = ExecutionEventId(canonical_id("xevt", seed + 3_000));
                message.event.sequence = ExecutionSequence(2);
            }
            "attempt" => message.lease.attempt = 999,
            "fence" => message.lease.fencing_token = FencingToken("999".into()),
            "stale-fence" => message.lease.fencing_token = FencingToken("1".into()),
            "lease" => message.lease.lease_id = LeaseId(canonical_id("lse", seed + 99)),
            "instance" => {
                message.lease.worker_instance_id = WorkerInstanceId(canonical_id("wki", seed + 99));
            }
            "session" => {
                message.worker_session_id = WorkerSessionId(canonical_id("wsn", seed + 99));
                message.session_identity.worker_session_id = message.worker_session_id.clone();
            }
            "thread" => {
                message.codex_thread_id = CodexThreadId(canonical_id("cdx", seed + 99));
                message.session_identity.codex_thread_id = message.codex_thread_id.clone();
            }
            "product" => {
                message.session_identity.product_session_id =
                    ProductSessionId(canonical_id("psn", seed + 99));
            }
            "old-event" => message.event.event_id = original.event.event_id.clone(),
            "changed-receipt" => {
                message = original.clone();
                message.event.summary = "changed historical body".into();
            }
            "gap" => message.event.sequence = ExecutionSequence(3),
            _ => unreachable!(),
        }
        let before = runtime_successor_write_snapshot(&fixture);
        let response = fixture
            .accept(
                &ExecutionPortMessage::RuntimeEventMessage(message.clone()),
                Instant("2027-01-15T08:00:07.100Z".into()),
            )
            .expect("current accepted Job returns a typed runtime disposition");
        let [ExecutionPortMessage::RuntimeAckMessage(ack)] = response.as_slice() else {
            panic!("runtime reject ACK")
        };
        let expected = match variant {
            "instance" => LeaseWriteStatus::RejectedWorkerInstance,
            "stale-fence" => LeaseWriteStatus::RejectedStaleFencingToken,
            "gap" => LeaseWriteStatus::Gap,
            _ => LeaseWriteStatus::RejectedConflict,
        };
        assert_eq!(ack.status, expected, "{variant}: {ack:?}");
        if variant == "gap" {
            assert_eq!(ack.ack_sequence.0, 1);
            assert_eq!(ack.replay_from_sequence, Some(ExecutionSequence(2)));
        }
        assert_eq!(
            runtime_successor_write_snapshot(&fixture),
            before,
            "{variant}"
        );
    }
    fixture.close();
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "legacy import failure and restart retain original receipt facts"
)]
fn runtime_successor_imports_legacy_history_atomically_and_keeps_original_receipt() {
    let seed = 8_700_000;
    let mut fixture = Fixture::open("runtime-successor-v1", seed);
    let old = install_runtime_successor_dispatch(&mut fixture, seed);
    let original = successor_runtime_message(&old, seed + 1_000, 1);
    assert_eq!(
        runtime_successor_ack(&mut fixture, &original).status,
        LeaseWriteStatus::Accepted
    );
    let stream = runtime_successor_stream(&old);
    let mut legacy = load_ledger(&fixture.storage, &stream).unwrap().unwrap();
    hydrate_runtime_ledger(&fixture.storage, &mut legacy).unwrap();
    legacy.schema_version = 1;
    legacy.event_chain_digest.clear();
    let connection = rusqlite::Connection::open(fixture.storage.database_path()).unwrap();
    let transaction = connection.unchecked_transaction().unwrap();
    transaction
        .execute(
            "UPDATE product_state SET payload=?2 WHERE stream_id=?1",
            rusqlite::params![&stream, serde_json::to_vec(&legacy).unwrap()],
        )
        .unwrap();
    for prefix in [
        format!("runtime-ledger-record:{}:", old.job.job_id.0),
        format!("runtime-ledger-identity:{}:", old.job.job_id.0),
    ] {
        transaction
            .execute(
                "DELETE FROM product_state WHERE substr(stream_id,1,length(?1))=?1",
                [prefix],
            )
            .unwrap();
    }
    transaction.commit().unwrap();
    let (successor, result) = prepare_runtime_successor(&mut fixture, &old, seed);
    fixture
        .accept(
            &ExecutionPortMessage::JobDispatchResultMessage(result.clone()),
            result.sent_at,
        )
        .unwrap();
    let first = successor_runtime_message(&successor, seed + 2_000, 1);
    let key = format!("runtime-ledger-record:{}:{:020}", old.job.job_id.0, 2);
    connection.execute_batch(&format!("CREATE TRIGGER fail_runtime_successor BEFORE INSERT ON product_state WHEN NEW.stream_id='{key}' BEGIN SELECT RAISE(ABORT,'successor commit fault'); END;")).unwrap();
    let before = runtime_successor_write_snapshot(&fixture);
    assert!(
        fixture
            .accept(
                &ExecutionPortMessage::RuntimeEventMessage(first.clone()),
                first.sent_at.clone()
            )
            .is_err()
    );
    assert_eq!(runtime_successor_write_snapshot(&fixture), before);
    connection
        .execute_batch("DROP TRIGGER fail_runtime_successor")
        .unwrap();
    assert_eq!(
        runtime_successor_ack(&mut fixture, &first).status,
        LeaseWriteStatus::Accepted
    );
    let old_key = format!("runtime-ledger-record:{}:{:020}", old.job.job_id.0, 1);
    let old_row = fixture.storage.load_state(&old_key).unwrap().unwrap();
    let indexed: Value = serde_json::from_slice(&old_row.payload).unwrap();
    assert_eq!(
        indexed["entry"]["event"],
        serde_json::to_value(&original.event).unwrap()
    );
    assert!(indexed.get("indexedSequenceOffset").is_none());
    drop(connection);
    let mut fixture = fixture.restart();
    let mut current = load_ledger(&fixture.storage, &stream).unwrap().unwrap();
    hydrate_runtime_ledger(&fixture.storage, &mut current).unwrap();
    assert_eq!(current.attempt, 2);
    assert_eq!(current.events[0].event, first.event);
    let ack = runtime_successor_ack_at(
        &mut fixture,
        &original,
        Instant("2027-01-15T08:16:00.000Z".into()),
    );
    assert_eq!(ack.status, LeaseWriteStatus::Duplicate);
    assert_eq!(ack.ack_sequence.0, 1);
    assert_eq!(
        fixture.storage.load_state(&old_key).unwrap().unwrap(),
        old_row
    );
    fixture.close();
}
