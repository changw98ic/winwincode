// SPDX-License-Identifier: Apache-2.0
use super::super::load_stored_run;
use super::super::tests::{delegated_record_and_binding, diagnostic_adapter_config};
use super::*;
use winwincode_domain::{FencingToken, LeaseId};
use winwincode_execution_port::replay::ReplayAcknowledgementStore;

#[tokio::test]
async fn retained_terminal_cannot_overtake_a_durable_core_fact() {
    let root =
        std::env::temp_dir().join(format!("wwc-terminal-core-fact-{}", uuid::Uuid::new_v4()));
    let (mut record, mut binding) = delegated_record_and_binding();
    record.workspace = root.join("workspace");
    std::fs::create_dir_all(&record.workspace).unwrap();
    binding.authority.lease.fencing_token = FencingToken("1".into());
    binding.authority.lease.lease_id = LeaseId("lse_00000000000000000000000001".into());
    binding.authority.lease.issued_at = Instant("2026-08-28T00:00:00.000Z".into());
    binding.authority.lease.expires_at = Instant("2026-08-28T01:00:00.000Z".into());
    let key = binding.run_key.clone();
    let session = record.kernel_session_id.clone();
    let thread = binding.canonical_thread_id.clone();
    let now = Instant("2026-08-28T00:00:02.000Z".into());
    record.terminal = Some(super::super::StoredTerminal::Cancelled {
        artifacts: Vec::new(),
    });
    let mut adapter = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
    adapter
        .install_active_run(&key, record, binding.clone(), false, false)
        .unwrap();
    adapter.bridge.authority().update_now(&now).unwrap();
    let message = adapter
        .prepare_tool_fact(
            &key,
            &session,
            KernelToolRuntimeEvent {
                source_sequence: 17,
                fact_json:
                    "{\"kind\":\"cell\",\"fact\":{\"schema_version\":1,\"lifecycle\":\"closed\"}}"
                        .into(),
            },
            &now,
        )
        .unwrap();
    adapter.runs.get_mut(&key).unwrap().record.core_tool_pending = Some(message.clone());
    adapter.persist_run(&key).unwrap();
    let polled = crate::CodexCoreAdapter::poll(&mut adapter, &thread, &now)
        .await
        .unwrap();
    let CodexPoll::RuntimeTrace(actual) = polled else {
        panic!("Core fact must precede terminal: {polled:?}")
    };
    assert_eq!(*actual, message);
    let record = load_stored_run(&adapter.store, &key).unwrap().unwrap();
    assert_eq!(record.core_tool_cursor, 17);
    assert!(record.core_tool_pending.is_none());
    assert!(record.terminal.is_some());
    assert!(matches!(
        crate::CodexCoreAdapter::poll(&mut adapter, &thread, &now)
            .await
            .unwrap(),
        CodexPoll::Cancelled { .. }
    ));
    let record = load_stored_run(&adapter.store, &key).unwrap().unwrap();
    assert_eq!(record.core_tool_final_cursor, Some(17));
    drop(adapter);
    let mut restarted = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
    restarted
        .install_active_run(&key, record, binding, false, true)
        .unwrap();
    restarted.bridge.authority().update_now(&now).unwrap();
    let CodexPoll::RuntimeTrace(replayed) =
        crate::CodexCoreAdapter::poll(&mut restarted, &thread, &now)
            .await
            .unwrap()
    else {
        panic!("unacknowledged final fact must replay after restart");
    };
    assert_eq!(*replayed, message);
    assert!(matches!(
        crate::CodexCoreAdapter::poll(&mut restarted, &thread, &now)
            .await
            .unwrap(),
        CodexPoll::Cancelled { .. }
    ));
    drop(restarted);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn pending_core_projection_repairs_replay_outbox_crash_and_keeps_source_cursor() {
    for (replay_retained, acknowledged) in [(false, false), (true, false), (true, true)] {
        let root =
            std::env::temp_dir().join(format!("wwc-core-fact-projection-{}", uuid::Uuid::new_v4()));
        let (mut record, mut binding) = delegated_record_and_binding();
        record.workspace = root.join("workspace");
        std::fs::create_dir_all(&record.workspace).unwrap();
        binding.authority.lease.fencing_token = FencingToken("1".into());
        binding.authority.lease.lease_id = LeaseId("lse_00000000000000000000000001".into());
        binding.authority.lease.issued_at = Instant("2026-08-28T00:00:00.000Z".into());
        binding.authority.lease.expires_at = Instant("2026-08-28T01:00:00.000Z".into());
        let key = binding.run_key.clone();
        let session = record.kernel_session_id.clone();
        let now = Instant("2026-08-28T00:00:02.000Z".into());
        let mut adapter = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
        adapter
            .install_active_run(&key, record, binding.clone(), false, false)
            .unwrap();
        adapter.bridge.authority().update_now(&now).unwrap();
        let mut message = adapter
            .prepare_tool_fact(
                &key,
                &session,
                KernelToolRuntimeEvent {
                    source_sequence: 17,
                    fact_json: "{\"kind\":\"request\",\"fact\":{\"schema_version\":1}}".into(),
                },
                &now,
            )
            .unwrap();
        adapter.runs.get_mut(&key).unwrap().record.core_tool_pending = Some(message.clone());
        adapter.persist_run(&key).unwrap();
        // Crash after retaining the Core projection in replay, before its outbox write.
        if replay_retained {
            RuntimeReplayResponder::default()
                .retain_runtime_event(&mut adapter.store, &adapter.bridge.authority(), &message)
                .unwrap();
            if acknowledged {
                let stream = RuntimeReplayIdentity {
                    lease: message.lease.clone(),
                    worker_session_id: message.worker_session_id.clone(),
                    session_identity: message.session_identity.clone(),
                    codex_thread_id: message.codex_thread_id.clone(),
                }
                .stream_key();
                adapter.store.record_acknowledgement(&stream, 0, 1).unwrap();
            }
        }
        drop(adapter);
        binding.authority.lease.expires_at = Instant("2026-08-28T02:00:00.000Z".into());
        if !replay_retained {
            message.lease = binding.authority.lease.clone();
        }
        let mut restarted = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
        let record = load_stored_run(&restarted.store, &key).unwrap().unwrap();
        restarted
            .install_active_run(&key, record, binding, false, true)
            .unwrap();
        restarted.bridge.authority().update_now(&now).unwrap();
        assert_eq!(
            restarted
                .runs
                .get(&key)
                .unwrap()
                .replay
                .iter()
                .any(|frame| frame == &message),
            !acknowledged
        );
        let record = load_stored_run(&restarted.store, &key).unwrap().unwrap();
        assert_eq!(record.core_tool_cursor, 17);
        assert!(record.core_tool_pending.is_none());
        let retained: Vec<_> = restarted
            .outbox
            .pending()
            .unwrap()
            .into_iter()
            .filter_map(|delivery| match delivery.message {
                ExecutionPortMessage::RuntimeEventMessage(frame) => Some(frame),
                _ => None,
            })
            .collect();
        assert_eq!(retained, if acknowledged { vec![] } else { vec![message] });
        drop(restarted);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one terminal lifecycle verifies ordering, close, restart and independent ACK compaction"
)]
async fn direct_outcome_retention_drains_core_before_close_and_replays_after_ack() {
    use super::super::ProductionCodexErrorKind;
    use crate::CodexCoreAdapter as _;
    use winwincode_domain::{ChangeBatchId, ExecutionAckSequence};
    use winwincode_execution_port::generated::{
        ExecutionOutcome, ExecutionOutcomeStatus, JobOutcomeAckMessage, JobOutcomeAckMessageKind,
        JobOutcomeAckMessageStatus, JobOutcomeMessage, JobOutcomeMessageKind, LeaseWriteStatus,
        RepairLoopCounters, RepairLoopStopReason, RuntimeAckMessage, RuntimeAckMessageKind,
    };
    for delegated_stop in [false, true] {
        let root = std::env::temp_dir().join(format!(
            "wwc-direct-outcome-core-fact-{}",
            uuid::Uuid::new_v4()
        ));
        let (mut record, mut binding) = delegated_record_and_binding();
        record.workspace = root.join("workspace");
        std::fs::create_dir_all(&record.workspace).unwrap();
        binding.authority.lease.fencing_token = FencingToken("1".into());
        binding.authority.lease.lease_id = LeaseId("lse_00000000000000000000000001".into());
        binding.authority.lease.issued_at = Instant("2026-08-28T00:00:00.000Z".into());
        binding.authority.lease.expires_at = Instant("2026-08-28T01:00:00.000Z".into());
        let key = binding.run_key.clone();
        let session = record.kernel_session_id.clone();
        let thread = binding.canonical_thread_id.clone();
        let now = Instant("2026-08-28T00:00:02.000Z".into());
        if delegated_stop {
            record.delegated_stop = Some(crate::DelegatedLoopStopFact {
                batch_id: ChangeBatchId(format!("sha256:{}", "b".repeat(64))),
                reason: RepairLoopStopReason::RepairRoundLimitReached,
                counters: RepairLoopCounters {
                    change_batches: 1,
                    context_pack_bytes: 0,
                    elapsed_millis: 0,
                    observer_calls: 0,
                    primary_model_calls: 0,
                    repair_rounds: 0,
                    total_cost_microunits: 0,
                    total_tokens: 0,
                },
                stopped_at: now.clone(),
            });
        } else {
            record.terminal = Some(super::super::StoredTerminal::Cancelled {
                artifacts: Vec::new(),
            });
        }
        let mut adapter = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
        adapter
            .install_active_run(&key, record, binding.clone(), false, false)
            .unwrap();
        adapter.bridge.authority().update_now(&now).unwrap();
        let fact_json = serde_json::json!({
            "kind": "cell", "fact": {
                "schema_version": 1, "sequence": 17, "thread_id": session,
                "parent_request_sequence": 1, "cell_id": "final-cell",
                "scope_id": "final-scope", "owner_id": "core-owner",
                "lifecycle": "closed", "revision": 2,
            }
        });
        let fact = adapter
            .prepare_tool_fact(
                &key,
                &session,
                KernelToolRuntimeEvent {
                    source_sequence: 17,
                    fact_json: fact_json.to_string(),
                },
                &now,
            )
            .unwrap();
        adapter.runs.get_mut(&key).unwrap().record.core_tool_pending = Some(fact.clone());
        adapter.persist_run(&key).unwrap();
        let outcome = JobOutcomeMessage {
            kind: JobOutcomeMessageKind::JobOutcome,
            lease: binding.authority.lease.clone(),
            message_id: ExecutionMessageId("xmsg_00000000000000000000000001".into()),
            outcome: ExecutionOutcome {
                artifacts: Vec::new(),
                codex_thread_id: Some(thread.clone()),
                error: None,
                finished_at: now.clone(),
                last_event_sequence: ExecutionAckSequence(0),
                status: if delegated_stop {
                    ExecutionOutcomeStatus::Failed
                } else {
                    ExecutionOutcomeStatus::Cancelled
                },
                summary: "retained terminal".into(),
                usage: None,
            },
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: now.clone(),
            session_identity: binding.authority.session_identity.clone(),
            worker_session_id: binding.authority.worker_session_id.clone(),
        };
        // Final candidate, delegated stop and Worker shutdown bypass the normal poll loop.
        let delivery = adapter.retain_job_outcome(&thread, &outcome).await.unwrap();
        let ExecutionPortMessage::JobOutcomeMessage(canonical) = &delivery.message else {
            panic!("canonical outcome")
        };
        assert_eq!(
            canonical.outcome.last_event_sequence,
            ExecutionAckSequence(1)
        );
        let stored = load_stored_run(&adapter.store, &key).unwrap().unwrap();
        assert_eq!(stored.core_tool_final_cursor, Some(17));
        assert!(stored.core_tool_pending.is_none());
        let pending = adapter.outbox.pending().unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(
            pending[0].message,
            ExecutionPortMessage::RuntimeEventMessage(fact.clone())
        );
        assert_eq!(pending[1], delivery);
        adapter.close_thread(&thread).await.unwrap();
        assert_eq!(
            adapter.outbox.pending().unwrap(),
            pending,
            "closing Core preserves pending receipts"
        );
        drop(adapter);

        let mut restarted = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
        let stored = load_stored_run(&restarted.store, &key).unwrap().unwrap();
        assert!(stored.terminal_trace.is_none());
        let mut missing_authority = stored.clone();
        missing_authority.terminal = None;
        missing_authority.delegated_stop = None;
        let mut missing_cut = stored.clone();
        missing_cut.core_tool_final_cursor = None;
        let mut mismatched_cut = stored.clone();
        mismatched_cut.core_tool_final_cursor = Some(16);
        let mut pending_fact = stored.clone();
        pending_fact.core_tool_pending = Some(fact.clone());
        let mut missing_message = stored.clone();
        missing_message.terminal_message_id = None;
        let mut foreign_message = stored.clone();
        foreign_message.terminal_message_id =
            Some(ExecutionMessageId("xmsg_00000000000000000000000009".into()));
        for broken in vec![
            missing_authority,
            missing_cut,
            mismatched_cut,
            pending_fact,
            missing_message,
            foreign_message,
        ]
        .into_boxed_slice()
        {
            assert!(
                restarted
                    .install_active_run(&key, broken, binding.clone(), false, true)
                    .is_err(),
                "an outcome phase cannot replace its authority, canonical frame or final Core cut"
            );
        }
        if delegated_stop {
            let mut duplicate_authority = stored.clone();
            duplicate_authority.terminal = Some(super::super::StoredTerminal::Cancelled {
                artifacts: Vec::new(),
            });
            assert!(
                restarted
                    .install_active_run(&key, duplicate_authority, binding.clone(), false, true)
                    .is_err()
            );
        }
        assert!(
            restarted
                .install_active_run(&key, stored.clone(), binding.clone(), true, true)
                .is_err()
        );
        let saved_snapshot: (String, Vec<u8>) = restarted.store.lock().unwrap().query_row(
            "SELECT frame_digest, frame_json FROM execution_terminal_outcome WHERE run_key = ?1",
            [&key], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        let mut foreign = canonical.clone();
        foreign.lease.worker_id.0 = "wrk_00000000000000000000000002".into();
        let foreign_bytes =
            serde_json::to_vec(&ExecutionPortMessage::JobOutcomeMessage(foreign)).unwrap();
        let foreign_digest = format!("sha256:{:x}", Sha256::digest(&foreign_bytes));
        for (snapshot, expected_error) in [
            (None, ProductionCodexErrorKind::Conflict),
            (
                Some(("corrupt".to_owned(), saved_snapshot.1.clone())),
                ProductionCodexErrorKind::DurableState,
            ),
            (
                Some((foreign_digest, foreign_bytes)),
                ProductionCodexErrorKind::Conflict,
            ),
        ] {
            restarted
                .store
                .lock()
                .unwrap()
                .execute(
                    "DELETE FROM execution_terminal_outcome WHERE run_key = ?1",
                    [&key],
                )
                .unwrap();
            if let Some((digest, bytes)) = snapshot {
                restarted.store.lock().unwrap().execute(
                    "INSERT INTO execution_terminal_outcome(run_key, frame_digest, frame_json) VALUES (?1, ?2, ?3)",
                    rusqlite::params![&key, digest, bytes]).unwrap();
            }
            let error = restarted
                .install_active_run(&key, stored.clone(), binding.clone(), false, true)
                .unwrap_err();
            assert_eq!(
                error.kind(),
                expected_error,
                "a missing, corrupt or foreign canonical snapshot cannot be reconstructed"
            );
        }
        restarted
            .store
            .lock()
            .unwrap()
            .execute(
                "DELETE FROM execution_terminal_outcome WHERE run_key = ?1",
                [&key],
            )
            .unwrap();
        restarted.store.lock().unwrap().execute(
            "INSERT INTO execution_terminal_outcome(run_key, frame_digest, frame_json) VALUES (?1, ?2, ?3)",
            rusqlite::params![&key, saved_snapshot.0, saved_snapshot.1]).unwrap();
        assert_eq!(restarted.outbox.pending().unwrap(), pending);
        restarted
            .install_active_run(&key, stored, binding.clone(), false, true)
            .unwrap();
        restarted.bridge.authority().update_now(&now).unwrap();
        let replayed = restarted
            .retain_job_outcome(&thread, &outcome)
            .await
            .unwrap();
        assert_eq!(replayed, delivery);
        assert_eq!(restarted.outbox.pending().unwrap(), pending);
        let runtime_ack = ExecutionPortMessage::RuntimeAckMessage(RuntimeAckMessage {
            ack_sequence: ExecutionAckSequence(1),
            error: None,
            kind: RuntimeAckMessageKind::RuntimeAck,
            lease: binding.authority.lease.clone(),
            message_id: ExecutionMessageId("xmsg_00000000000000000000000002".into()),
            replay_from_sequence: None,
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: now.clone(),
            session_identity: binding.authority.session_identity.clone(),
            status: LeaseWriteStatus::Accepted,
            worker_session_id: binding.authority.worker_session_id.clone(),
        });
        restarted
            .accept_execution_delivery_ack(&runtime_ack)
            .unwrap();
        let outcome_ack = ExecutionPortMessage::JobOutcomeAckMessage(JobOutcomeAckMessage {
            error: None,
            kind: JobOutcomeAckMessageKind::JobOutcomeAck,
            lease: binding.authority.lease.clone(),
            message_id: ExecutionMessageId("xmsg_00000000000000000000000003".into()),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: now.clone(),
            session_identity: binding.authority.session_identity.clone(),
            status: JobOutcomeAckMessageStatus::Accepted,
            worker_session_id: binding.authority.worker_session_id.clone(),
        });
        restarted
            .accept_execution_delivery_ack(&outcome_ack)
            .unwrap();
        assert!(restarted.outbox.pending().unwrap().is_empty());
        drop(restarted);
        let mut restarted = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
        let stored = load_stored_run(&restarted.store, &key).unwrap().unwrap();
        restarted
            .install_active_run(&key, stored, binding, false, true)
            .unwrap();
        restarted.bridge.authority().update_now(&now).unwrap();
        assert!(restarted.outbox.pending().unwrap().is_empty());
        let compacted = restarted
            .retain_job_outcome(&thread, &outcome)
            .await
            .unwrap();
        assert_eq!(
            compacted, delivery,
            "ACK never changes the canonical outcome"
        );
        assert_eq!(
            restarted.outbox.pending().unwrap(),
            vec![compacted],
            "ACKed Core facts stay compacted during terminal replay"
        );
        drop(restarted);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one durable lifecycle preserves the original outcome across renewal, restart and ACK"
)]
async fn retained_outcome_renewal_preserves_original_frame_across_restart_and_ack() {
    use super::super::ProductionCodexErrorKind;
    use crate::CodexCoreAdapter as _;
    use winwincode_domain::{ExecutionAckSequence, RequestId};
    use winwincode_execution_port::generated::{
        ExecutionOutcome, ExecutionOutcomeStatus, JobOutcomeAckMessage, JobOutcomeAckMessageKind,
        JobOutcomeAckMessageStatus, JobOutcomeMessage, JobOutcomeMessageKind, LeaseRenewMessage,
        LeaseRenewMessageKind,
    };
    let root = std::env::temp_dir().join(format!(
        "wwc-retained-outcome-renewal-{}",
        uuid::Uuid::new_v4()
    ));
    let (mut record, mut binding) = delegated_record_and_binding();
    record.workspace = root.join("workspace");
    std::fs::create_dir_all(&record.workspace).unwrap();
    record.terminal = Some(super::super::StoredTerminal::Cancelled {
        artifacts: Vec::new(),
    });
    binding.authority.lease.fencing_token = FencingToken("1".into());
    binding.authority.lease.lease_id = LeaseId("lse_00000000000000000000000001".into());
    binding.authority.lease.issued_at = Instant("2026-08-28T00:00:00.000Z".into());
    binding.authority.lease.expires_at = Instant("2026-08-28T01:00:00.000Z".into());
    let key = binding.run_key.clone();
    let thread = binding.canonical_thread_id.clone();
    let now = Instant("2026-08-28T00:00:02.000Z".into());
    let outcome = JobOutcomeMessage {
        kind: JobOutcomeMessageKind::JobOutcome,
        lease: binding.authority.lease.clone(),
        message_id: ExecutionMessageId("xmsg_00000000000000000000000001".into()),
        outcome: ExecutionOutcome {
            artifacts: Vec::new(),
            codex_thread_id: Some(thread.clone()),
            error: None,
            finished_at: now.clone(),
            last_event_sequence: ExecutionAckSequence(0),
            status: ExecutionOutcomeStatus::Cancelled,
            summary: "original terminal outcome".into(),
            usage: None,
        },
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: now.clone(),
        session_identity: binding.authority.session_identity.clone(),
        worker_session_id: binding.authority.worker_session_id.clone(),
    };
    let mut adapter = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
    adapter
        .install_active_run(&key, record, binding.clone(), false, false)
        .unwrap();
    let original = adapter.retain_job_outcome(&thread, &outcome).await.unwrap();
    drop(adapter);

    let mut adapter = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
    let stored = load_stored_run(&adapter.store, &key).unwrap().unwrap();
    let mut shortened = binding.clone();
    shortened.authority.lease.expires_at = Instant("2026-08-28T00:30:00.000Z".into());
    let mut foreign = binding.clone();
    foreign.authority.lease.worker_id.0 = "wrk_00000000000000000000000002".into();
    let mut changed_origin = binding.clone();
    changed_origin.authority.lease.issued_at = Instant("2026-08-28T00:00:01.000Z".into());
    for invalid in [shortened, foreign, changed_origin] {
        assert_eq!(
            adapter
                .install_active_run(&key, stored.clone(), invalid, false, true)
                .unwrap_err()
                .kind(),
            ProductionCodexErrorKind::Conflict,
            "only the same lease identity with monotonic expiry can restore an outcome"
        );
    }
    adapter
        .install_active_run(&key, stored, binding.clone(), false, true)
        .unwrap();
    let mut extended = binding.authority.lease.clone();
    extended.expires_at = Instant("2026-08-28T02:00:00.000Z".into());
    let renewal = LeaseRenewMessage {
        kind: LeaseRenewMessageKind::LeaseRenew,
        lease: extended.clone(),
        prior_expires_at: binding.authority.lease.expires_at.clone(),
        message_id: ExecutionMessageId("xmsg_00000000000000000000000002".into()),
        request_id: RequestId("req_00000000000000000000000001".into()),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: now.clone(),
    };
    assert!(adapter.renew_lease(&thread, &renewal, &now).unwrap());
    assert!(!adapter.runs[&key].kernel_live);
    let renewed_binding = adapter.runs[&key].binding.clone();
    assert_eq!(renewed_binding.authority.lease, extended);
    let mut current_proposal = outcome.clone();
    current_proposal.lease = extended;
    current_proposal.message_id = ExecutionMessageId("xmsg_00000000000000000000000003".into());
    current_proposal.sent_at = Instant("2026-08-28T00:00:03.000Z".into());
    current_proposal.outcome.finished_at = current_proposal.sent_at.clone();
    current_proposal.outcome.summary = "Worker retried under its renewed lease".into();
    assert_eq!(
        adapter
            .retain_job_outcome(&thread, &current_proposal)
            .await
            .unwrap(),
        original,
        "a current Worker proposal replays the complete original frame"
    );
    assert_eq!(adapter.outbox.pending().unwrap(), vec![original.clone()]);
    drop(adapter);

    let mut adapter = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
    let stored = load_stored_run(&adapter.store, &key).unwrap().unwrap();
    adapter
        .install_active_run(&key, stored, renewed_binding.clone(), false, true)
        .unwrap();
    assert_eq!(
        adapter
            .retain_job_outcome(&thread, &current_proposal)
            .await
            .unwrap(),
        original,
        "restart retains the original message, lease, cursor and payload"
    );
    let acknowledgement = ExecutionPortMessage::JobOutcomeAckMessage(JobOutcomeAckMessage {
        error: None,
        kind: JobOutcomeAckMessageKind::JobOutcomeAck,
        lease: outcome.lease.clone(),
        message_id: ExecutionMessageId("xmsg_00000000000000000000000004".into()),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: current_proposal.sent_at.clone(),
        session_identity: outcome.session_identity.clone(),
        status: JobOutcomeAckMessageStatus::Accepted,
        worker_session_id: outcome.worker_session_id.clone(),
    });
    adapter
        .accept_execution_delivery_ack(&acknowledgement)
        .unwrap();
    assert!(adapter.outbox.pending().unwrap().is_empty());
    drop(adapter);

    let mut adapter = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
    let stored = load_stored_run(&adapter.store, &key).unwrap().unwrap();
    adapter
        .install_active_run(&key, stored, renewed_binding, false, true)
        .unwrap();
    assert!(adapter.outbox.pending().unwrap().is_empty());
    let replayed = adapter
        .retain_job_outcome(&thread, &current_proposal)
        .await
        .unwrap();
    assert_eq!(replayed, original, "ACK cannot replace the original frame");
    assert_eq!(adapter.outbox.pending().unwrap(), vec![replayed]);
    drop(adapter);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn failed_core_close_cannot_seal_or_publish_a_final_source_cursor() {
    let root = std::env::temp_dir().join(format!("wwc-failed-core-close-{}", uuid::Uuid::new_v4()));
    let (mut record, mut binding) = delegated_record_and_binding();
    record.workspace = root.join("workspace");
    std::fs::create_dir_all(&record.workspace).unwrap();
    binding.authority.lease.fencing_token = FencingToken("1".into());
    binding.authority.lease.lease_id = LeaseId("lse_00000000000000000000000001".into());
    binding.authority.lease.issued_at = Instant("2026-08-28T00:00:00.000Z".into());
    binding.authority.lease.expires_at = Instant("2026-08-28T01:00:00.000Z".into());
    let key = binding.run_key.clone();
    let thread = binding.canonical_thread_id.clone();
    let now = Instant("2026-08-28T00:00:02.000Z".into());
    let mut adapter = ProductionCodexAdapter::open(diagnostic_adapter_config(&root)).unwrap();
    // This real Kernel error is deterministic: the claimed live session was never registered.
    adapter
        .install_active_run(&key, record, binding, true, false)
        .unwrap();
    adapter.bridge.authority().update_now(&now).unwrap();
    assert!(
        adapter
            .poll_infrastructure_terminal(&key, &now)
            .await
            .is_err()
    );
    assert!(adapter.runs[&key].kernel_live);
    let stored = load_stored_run(&adapter.store, &key).unwrap().unwrap();
    assert_eq!(stored.core_tool_final_cursor, None);
    assert!(
        crate::CodexCoreAdapter::poll(&mut adapter, &thread, &now)
            .await
            .is_err()
    );
    assert_eq!(
        load_stored_run(&adapter.store, &key)
            .unwrap()
            .unwrap()
            .core_tool_final_cursor,
        None
    );
    crate::CodexCoreAdapter::shutdown(&mut adapter)
        .await
        .unwrap();
    drop(adapter);
    std::fs::remove_dir_all(root).unwrap();
}
