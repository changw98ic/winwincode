// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use winwincode_control_plane::recovery_router::{
    BrowserReconnectRequest, BrowserRecoveryStream, BrowserReplayAuthorityError,
    RecoveryExecutionAuthority, RecoveryReplayAuthorityError, RecoveryRouter, RecoveryRoutingError,
    RecoveryRuntimeEvent, RecoveryWriteStatus, SessionRecoveryPlan, SessionRecoveryRequest,
    SessionRecoveryState, ThreadRecoveryCapability, browser_replay_stream_key,
};
use winwincode_domain::{
    CodexThreadId, ExecutionJobId, ExecutionMessageId, FencingToken, Instant, LeaseId,
    ProductSessionId, RequestId, SessionIdentity, Sha256Digest, StageRunId, WorkerId,
    WorkerInstanceId, WorkerSessionId,
};
use winwincode_execution_port::generated::ExecutionLeaseStamp;
use winwincode_execution_port::replay::{
    ReplayDecision, ReplayError, ReplayFrame, ReplaySequence, ReplaySnapshot, ReplayStore,
    ReplayStreamKey,
};
use winwincode_execution_port::runtime_replay::RuntimeReplayIdentity;
use winwincode_storage::{WorkerSlotAuthority, WorkerSlotResources, WorkerSlotState};

fn id(prefix: &str, number: u64) -> String {
    format!("{prefix}_{number:026}")
}

fn at(second: u64) -> Instant {
    Instant(format!("2026-08-27T00:00:{second:02}.000Z"))
}

fn digest(byte: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", byte.to_string().repeat(64)))
}

fn authority(
    job: u64,
    worker: u64,
    attempt: u64,
    fence: u64,
    thread: u64,
) -> RecoveryExecutionAuthority {
    RecoveryExecutionAuthority {
        slot: WorkerSlotAuthority {
            worker_id: WorkerId(id("wrk", worker)),
            worker_instance_id: WorkerInstanceId(id("wki", worker)),
            worker_session_id: WorkerSessionId(id("wsn", worker)),
            codex_thread_id: CodexThreadId(id("cdx", thread)),
            job_id: ExecutionJobId(id("job", job)),
            lease_id: LeaseId(id("lse", fence)),
            attempt,
            fencing_token: FencingToken(fence.to_string()),
        },
        issued_at: at(20),
        expires_at: at(50),
    }
}

fn state(capability: ThreadRecoveryCapability) -> SessionRecoveryState {
    SessionRecoveryState {
        product_session_id: ProductSessionId(id("psn", 1)),
        stage_run_id: Some(StageRunId(id("run", 1))),
        browser_stream_id: "product-session.events".to_owned(),
        browser_authorization_epoch: 7,
        revision: 8,
        slot_revision: 3,
        slot_resources: WorkerSlotResources {
            memory_bytes: 1_024,
            disk_bytes: 2_048,
            process_slots: 1,
        },
        authority: authority(1, 1, 1, 1, 1),
        confirmed_runtime_sequence: 4,
        thread_capability: capability,
    }
}

fn recovery_request(
    replacement: RecoveryExecutionAuthority,
    replay_message_id: Option<ExecutionMessageId>,
    max_replay_events: u64,
) -> SessionRecoveryRequest {
    SessionRecoveryRequest {
        request_id: RequestId(id("req", 1)),
        product_session_id: ProductSessionId(id("psn", 1)),
        expected_revision: 8,
        replacement,
        recovered_at: at(30),
        replay_message_id,
        max_replay_events,
    }
}

fn runtime_identity(
    state: &SessionRecoveryState,
    authority: &RecoveryExecutionAuthority,
) -> RuntimeReplayIdentity {
    RuntimeReplayIdentity {
        lease: ExecutionLeaseStamp {
            attempt: i64::try_from(authority.slot.attempt).expect("attempt"),
            expires_at: authority.expires_at.clone(),
            fencing_token: authority.slot.fencing_token.clone(),
            issued_at: authority.issued_at.clone(),
            job_id: authority.slot.job_id.clone(),
            lease_id: authority.slot.lease_id.clone(),
            worker_id: authority.slot.worker_id.clone(),
            worker_instance_id: authority.slot.worker_instance_id.clone(),
        },
        worker_session_id: authority.slot.worker_session_id.clone(),
        session_identity: SessionIdentity {
            codex_thread_id: authority.slot.codex_thread_id.clone(),
            product_session_id: state.product_session_id.clone(),
            stage_run_id: state.stage_run_id.clone(),
            worker_session_id: authority.slot.worker_session_id.clone(),
        },
        codex_thread_id: authority.slot.codex_thread_id.clone(),
    }
}

#[derive(Default)]
struct MemoryReplayStore {
    snapshots: BTreeMap<ReplayStreamKey, ReplaySnapshot>,
    writes: usize,
}

impl ReplayStore for MemoryReplayStore {
    type Error = &'static str;

    fn load(&mut self, stream: &ReplayStreamKey) -> Result<Option<ReplaySnapshot>, Self::Error> {
        Ok(self.snapshots.get(stream).cloned())
    }

    fn append(
        &mut self,
        stream: &ReplayStreamKey,
        expected_highest_sequence: ReplaySequence,
        frame: &ReplayFrame,
    ) -> Result<(), Self::Error> {
        let snapshot = self.snapshots.entry(stream.clone()).or_default();
        if snapshot.highest_sequence != expected_highest_sequence {
            return Err("concurrent replay write");
        }
        snapshot.events.push(frame.clone());
        snapshot.highest_sequence = frame.sequence;
        self.writes += 1;
        Ok(())
    }
}

#[test]
fn transferable_thread_resumes_from_confirmed_cursor_and_replays_after_restart() {
    let initial = state(ThreadRecoveryCapability::TransferableCheckpoint {
        checkpoint_sha256: digest('a'),
    });
    let replacement = authority(1, 2, 2, 2, 1);
    let request = recovery_request(
        replacement.clone(),
        Some(ExecutionMessageId(id("xmsg", 1))),
        100,
    );
    let mut router = RecoveryRouter::default();
    router
        .register_session(initial.clone())
        .expect("session registration");

    let receipt = router
        .recover_session(&request)
        .expect("transferable recovery");
    assert_eq!(receipt.status, RecoveryWriteStatus::Applied);
    assert_eq!(receipt.previous_revision, 8);
    assert_eq!(receipt.current_revision, 9);
    let SessionRecoveryPlan::ResumeTransferableThread {
        worker,
        checkpoint_sha256,
        resume_after_sequence,
        runtime_replay,
    } = &receipt.plan
    else {
        panic!("transferable thread must resume");
    };
    assert_eq!(checkpoint_sha256, &digest('a'));
    assert_eq!(*resume_after_sequence, 4);
    assert_eq!(runtime_replay.job_id, replacement.slot.job_id);
    assert_eq!(runtime_replay.max_events, 100);
    assert_eq!(worker.close_old_slot.authority, initial.authority.slot);
    assert_eq!(worker.close_old_slot.expected_revision, 3);
    assert_eq!(
        worker.close_old_slot.outcome,
        WorkerSlotState::RecoveryFailed
    );
    assert_eq!(worker.open_replacement_slot.authority, replacement.slot);
    assert_eq!(
        worker.open_replacement_slot.resources,
        initial.slot_resources
    );

    assert_eq!(
        router
            .recover_session(&request)
            .expect("exact request replay")
            .status,
        RecoveryWriteStatus::Duplicate
    );
    let mut restored = RecoveryRouter::restore(router.snapshot()).expect("Control Plane restart");
    assert_eq!(
        restored
            .recover_session(&request)
            .expect("durable receipt replay")
            .status,
        RecoveryWriteStatus::Duplicate
    );
    let mut changed = request;
    changed.max_replay_events = 99;
    assert_eq!(
        restored.recover_session(&changed),
        Err(RecoveryRoutingError::IdempotencyConflict)
    );
}

#[test]
fn process_local_thread_ends_old_attempt_and_requires_a_new_thread() {
    let initial = state(ThreadRecoveryCapability::ProcessLocalOnly);
    let replacement = authority(1, 2, 2, 2, 2);
    let request = recovery_request(replacement.clone(), None, 0);
    let mut router = RecoveryRouter::default();
    router
        .register_session(initial.clone())
        .expect("session registration");
    let receipt = router
        .recover_session(&request)
        .expect("fresh recovery attempt");
    let SessionRecoveryPlan::StartFreshAttempt {
        worker,
        ended_codex_thread_id,
        new_codex_thread_id,
    } = receipt.plan
    else {
        panic!("process-local thread must not resume");
    };
    assert_eq!(
        ended_codex_thread_id,
        initial.authority.slot.codex_thread_id
    );
    assert_eq!(new_codex_thread_id, replacement.slot.codex_thread_id);
    assert_eq!(worker.open_replacement_slot.authority.attempt, 2);
    assert_eq!(worker.open_replacement_slot.authority.fencing_token.0, "2");

    let mut reused_router = RecoveryRouter::default();
    reused_router
        .register_session(initial.clone())
        .expect("second registration");
    let reused = recovery_request(authority(1, 2, 2, 2, 1), None, 0);
    assert_eq!(
        reused_router.recover_session(&reused),
        Err(RecoveryRoutingError::NonTransferableThreadReused)
    );

    let mut metadata_router = RecoveryRouter::default();
    metadata_router
        .register_session(initial)
        .expect("third registration");
    let with_replay = recovery_request(
        authority(1, 2, 2, 2, 2),
        Some(ExecutionMessageId(id("xmsg", 2))),
        10,
    );
    assert_eq!(
        metadata_router.recover_session(&with_replay),
        Err(RecoveryRoutingError::ReplayMetadataForbidden)
    );
}

#[test]
fn old_worker_is_fenced_and_runtime_event_duplicates_survive_control_plane_restart() {
    let initial = state(ThreadRecoveryCapability::TransferableCheckpoint {
        checkpoint_sha256: digest('b'),
    });
    let replacement = authority(1, 2, 2, 2, 1);
    let request = recovery_request(
        replacement.clone(),
        Some(ExecutionMessageId(id("xmsg", 3))),
        100,
    );
    let mut router = RecoveryRouter::default();
    router
        .register_session(initial.clone())
        .expect("session registration");
    router
        .recover_session(&request)
        .expect("replacement recovery");

    let old_event = RecoveryRuntimeEvent {
        product_session_id: initial.product_session_id.clone(),
        identity: runtime_identity(&initial, &initial.authority),
        frame: ReplayFrame::new("event-old", 1, "digest-old", b"old"),
    };
    let mut store = MemoryReplayStore::default();
    assert_eq!(
        router.accept_runtime_event(&mut store, &old_event),
        Err(ReplayError::Authority(
            RecoveryReplayAuthorityError::OldWorkerFenced
        ))
    );
    assert_eq!(store.writes, 0);

    let current_event = RecoveryRuntimeEvent {
        product_session_id: initial.product_session_id.clone(),
        identity: runtime_identity(&initial, &replacement),
        frame: ReplayFrame::new("event-current", 1, "digest-current", b"current"),
    };
    assert_eq!(
        router
            .accept_runtime_event(&mut store, &current_event)
            .expect("current Worker event"),
        ReplayDecision::Accepted {
            highest_sequence: 1
        }
    );
    assert_eq!(store.writes, 1);
    assert!(matches!(
        router
            .accept_runtime_event(&mut store, &current_event)
            .expect("duplicate event"),
        ReplayDecision::Duplicate {
            highest_sequence: 1,
            ..
        }
    ));
    assert_eq!(store.writes, 1);

    let restored = RecoveryRouter::restore(router.snapshot()).expect("Control Plane restart");
    assert!(matches!(
        restored
            .accept_runtime_event(&mut store, &current_event)
            .expect("duplicate after restart"),
        ReplayDecision::Duplicate {
            highest_sequence: 1,
            ..
        }
    ));
    assert_eq!(store.writes, 1);
}

#[test]
fn browser_reconnect_uses_persisted_cursor_and_exact_authorization_epoch() {
    let initial = state(ThreadRecoveryCapability::ProcessLocalOnly);
    let mut router = RecoveryRouter::default();
    router
        .register_session(initial.clone())
        .expect("session registration");
    let stream = BrowserRecoveryStream {
        product_session_id: initial.product_session_id,
        stream_id: initial.browser_stream_id,
        authorization_epoch: initial.browser_authorization_epoch,
    };
    let stream_key = browser_replay_stream_key(&stream);
    let mut store = MemoryReplayStore::default();
    store.snapshots.insert(
        stream_key,
        ReplaySnapshot {
            ack_sequence: 1,
            highest_sequence: 3,
            events: vec![
                ReplayFrame::new("browser-1", 1, "digest-1", b"one"),
                ReplayFrame::new("browser-2", 2, "digest-2", b"two"),
                ReplayFrame::new("browser-3", 3, "digest-3", b"three"),
            ],
        },
    );
    let request = BrowserReconnectRequest {
        stream: stream.clone(),
        after_sequence: 1,
        max_events: 10,
    };
    let replay = router
        .resume_browser(&mut store, &request)
        .expect("browser resume");
    assert_eq!(replay.ack_sequence, 1);
    assert_eq!(replay.highest_sequence, 3);
    assert_eq!(
        replay
            .events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![2, 3]
    );

    let mut foreign_epoch = request;
    foreign_epoch.stream.authorization_epoch = 8;
    assert_eq!(
        router.resume_browser(&mut store, &foreign_epoch),
        Err(ReplayError::Authority(
            BrowserReplayAuthorityError::ForeignStream
        ))
    );
}

#[test]
fn replacement_rejects_same_worker_process_stale_fence_and_skipped_attempt() {
    let initial = state(ThreadRecoveryCapability::TransferableCheckpoint {
        checkpoint_sha256: digest('c'),
    });
    let mut same_process = RecoveryRouter::default();
    same_process
        .register_session(initial.clone())
        .expect("same process registration");
    let same = recovery_request(
        RecoveryExecutionAuthority {
            slot: WorkerSlotAuthority {
                worker_session_id: WorkerSessionId(id("wsn", 2)),
                attempt: 2,
                fencing_token: FencingToken("2".to_owned()),
                lease_id: LeaseId(id("lse", 2)),
                ..initial.authority.slot.clone()
            },
            issued_at: at(20),
            expires_at: at(50),
        },
        Some(ExecutionMessageId(id("xmsg", 4))),
        10,
    );
    assert_eq!(
        same_process.recover_session(&same),
        Err(RecoveryRoutingError::WorkerInstanceNotReplaced)
    );

    let mut stale_fence = RecoveryRouter::default();
    stale_fence
        .register_session(initial.clone())
        .expect("stale fence registration");
    let stale = recovery_request(
        RecoveryExecutionAuthority {
            slot: WorkerSlotAuthority {
                fencing_token: FencingToken("1".to_owned()),
                ..authority(1, 2, 2, 2, 1).slot
            },
            issued_at: at(20),
            expires_at: at(50),
        },
        Some(ExecutionMessageId(id("xmsg", 5))),
        10,
    );
    assert_eq!(
        stale_fence.recover_session(&stale),
        Err(RecoveryRoutingError::LeaseNotNewer)
    );

    let mut skipped_attempt = RecoveryRouter::default();
    skipped_attempt
        .register_session(initial)
        .expect("skipped attempt registration");
    let skipped = recovery_request(
        authority(1, 2, 3, 3, 1),
        Some(ExecutionMessageId(id("xmsg", 6))),
        10,
    );
    assert_eq!(
        skipped_attempt.recover_session(&skipped),
        Err(RecoveryRoutingError::LeaseNotNewer)
    );
}
