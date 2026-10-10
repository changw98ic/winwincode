// SPDX-License-Identifier: Apache-2.0
//! Offline measurements of production storage methods at the frozen audit commit.

use std::cell::RefCell;

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub(crate) struct Metrics {
    pub replay_reads: u64,
    pub replay_read_bytes: u64,
    pub replay_writes: u64,
    pub replay_write_bytes: u64,
    pub run_writes: u64,
    pub run_write_bytes: u64,
    pub pending_vm_steps: u64,
    pub runtime_rows: u64,
    pub runtime_row_bytes: u64,
    pub diagnostic_validations: u64,
    pub diagnostic_raw_bytes: u64,
}

thread_local! {
    static MEASUREMENTS: RefCell<Option<Metrics>> = const { RefCell::new(None) };
}

pub(crate) fn reset() {
    MEASUREMENTS.with(|value| *value.borrow_mut() = Some(Metrics::default()));
}

pub(crate) fn finish() -> Metrics {
    MEASUREMENTS.with(|value| value.borrow_mut().take().unwrap_or_default())
}

fn record(action: impl FnOnce(&mut Metrics)) {
    MEASUREMENTS.with(|value| {
        if let Some(metrics) = value.borrow_mut().as_mut() {
            action(metrics);
        }
    });
}

pub(crate) fn snapshot_read(table: &str, bytes: usize) {
    if table == "runtime_replay" {
        record(|metrics| {
            metrics.replay_reads += 1;
            metrics.replay_read_bytes += bytes as u64;
        });
    }
}

pub(crate) fn snapshot_write(table: &str, bytes: usize) {
    if table == "runtime_replay" {
        record(|metrics| {
            metrics.replay_writes += 1;
            metrics.replay_write_bytes += bytes as u64;
        });
    }
}

pub(crate) fn run_write(bytes: usize) {
    record(|metrics| {
        metrics.run_writes += 1;
        metrics.run_write_bytes += bytes as u64;
    });
}

pub(crate) fn pending_query(steps: i32) {
    record(|metrics| metrics.pending_vm_steps += u64::try_from(steps).unwrap());
}

pub(crate) fn runtime_row(bytes: usize) {
    record(|metrics| {
        metrics.runtime_rows += 1;
        metrics.runtime_row_bytes += bytes as u64;
    });
}

pub(crate) fn diagnostic_validation(bytes: usize) {
    record(|metrics| {
        metrics.diagnostic_validations += 1;
        metrics.diagnostic_raw_bytes += bytes as u64;
    });
}

fn root(name: &str) -> std::path::PathBuf {
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "winwincode-storage-mechanism-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn fixture(kind: &str) -> winwincode_execution_port::generated::ExecutionPortMessage {
    let value: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    serde_json::from_value(
        value["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["kind"] == kind)
            .unwrap()
            .clone(),
    )
    .unwrap()
}

fn runtime(
    sequence: u64,
    stream: u64,
) -> winwincode_execution_port::generated::RuntimeEventMessage {
    use winwincode_domain::{
        CodexThreadId, ExecutionEventId, ExecutionJobId, ExecutionMessageId, ExecutionSequence,
        LeaseId, WorkRunId, WorkerSessionId,
    };
    use winwincode_execution_port::generated::ExecutionPortMessage;
    let ExecutionPortMessage::RuntimeEventMessage(mut event) = fixture("runtime.event") else {
        panic!("runtime fixture")
    };
    let seed = stream * 1_000_000 + sequence;
    event.message_id = ExecutionMessageId(format!("xmsg_{seed:026}"));
    event.event.event_id = ExecutionEventId(format!("xevt_{seed:026}"));
    event.event.sequence = ExecutionSequence(i64::try_from(sequence).unwrap());
    event.lease.job_id = ExecutionJobId(format!("job_{stream:026}"));
    event.lease.lease_id = LeaseId(format!("lse_{stream:026}"));
    event.codex_thread_id = CodexThreadId(format!("cdx_{stream:026}"));
    event.worker_session_id = WorkerSessionId(format!("wsn_{stream:026}"));
    event.session_identity.codex_thread_id = event.codex_thread_id.clone();
    event.session_identity.worker_session_id = event.worker_session_id.clone();
    event.session_identity.work_run_id = Some(WorkRunId(format!("wrn_{stream:026}")));
    event.event.summary = "synthetic offline runtime event".into();
    event
}

fn ack(
    event: &winwincode_execution_port::generated::RuntimeEventMessage,
) -> winwincode_execution_port::generated::RuntimeAckMessage {
    use winwincode_domain::ExecutionAckSequence;
    use winwincode_execution_port::generated::{ExecutionPortMessage, LeaseWriteStatus};
    let ExecutionPortMessage::RuntimeAckMessage(mut ack) = fixture("runtime.ack") else {
        panic!("runtime ACK fixture")
    };
    ack.lease = event.lease.clone();
    ack.worker_session_id = event.worker_session_id.clone();
    ack.session_identity = event.session_identity.clone();
    ack.ack_sequence = ExecutionAckSequence(event.event.sequence.0);
    ack.status = LeaseWriteStatus::Accepted;
    ack.error = None;
    ack.replay_from_sequence = None;
    ack
}

struct Authority(winwincode_execution_port::runtime_replay::RuntimeReplayIdentity);

impl winwincode_execution_port::replay::ReplayAuthority for Authority {
    type Context = winwincode_execution_port::runtime_replay::RuntimeReplayIdentity;
    type Error = &'static str;

    fn validate_active_lease(
        &self,
        stream: &winwincode_execution_port::replay::ReplayStreamKey,
        identity: &Self::Context,
    ) -> Result<(), Self::Error> {
        if identity == &self.0 && stream == &self.0.stream_key() {
            Ok(())
        } else {
            Err("foreign authority")
        }
    }
}

#[test]
#[ignore = "mechanism audit: runtime replay snapshot cost baseline"]
fn m04_runtime_responder_uses_real_adapter_snapshot_for_events_and_acks() {
    use winwincode_execution_port::replay::{ReplayDecision, ReplayStore};
    use winwincode_execution_port::runtime_replay::{
        RuntimeReplayIdentity, RuntimeReplayResponder,
    };
    let mut samples = Vec::new();
    for size in [100_u64, 200, 400] {
        let root = root("runtime");
        let mut store = crate::store::AdapterStore::open(&root).unwrap();
        let first = runtime(1, 1);
        let authority = Authority(RuntimeReplayIdentity {
            lease: first.lease.clone(),
            worker_session_id: first.worker_session_id.clone(),
            session_identity: first.session_identity.clone(),
            codex_thread_id: first.codex_thread_id.clone(),
        });
        let responder = RuntimeReplayResponder::new();
        reset();
        for sequence in 1..=size {
            let event = runtime(sequence, 1);
            assert!(matches!(
                responder
                    .retain_runtime_event(&mut store, &authority, &event)
                    .unwrap(),
                ReplayDecision::Accepted { .. }
            ));
            let receipt = responder
                .acknowledge(&mut store, &authority, &ack(&event))
                .unwrap();
            assert_eq!(receipt.ack_sequence.0, event.event.sequence.0);
        }
        let metrics = finish();
        assert_eq!(metrics.replay_writes, 2 * size);
        let snapshot = store.load(&authority.0.stream_key()).unwrap().unwrap();
        assert_eq!(snapshot.events.len(), usize::try_from(size).unwrap());
        assert_eq!(snapshot.ack_sequence, size);
        let last = runtime(size, 1);
        reset();
        assert!(matches!(
            responder
                .retain_runtime_event(&mut store, &authority, &last)
                .unwrap(),
            ReplayDecision::Duplicate { .. }
        ));
        let duplicate = finish();
        assert_eq!(duplicate.replay_writes, 0);
        let mut changed = last;
        changed.event.summary = "changed duplicate".into();
        assert!(matches!(
            responder
                .retain_runtime_event(&mut store, &authority, &changed)
                .unwrap(),
            ReplayDecision::Conflict { .. }
        ));
        println!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({"id":"M04", "size":size, "metrics":metrics, "retained_events_after_ack":snapshot.events.len(), "duplicate":duplicate})
        );
        samples.push(metrics.replay_write_bytes);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    assert!(samples[1] > 3 * samples[0]);
    assert!(samples[2] > 3 * samples[1]);
}

#[test]
#[ignore = "mechanism audit: retained transport history scan cost baseline"]
fn m07_real_pending_batch_scans_retained_sent_transport_history() {
    use winwincode_domain::{
        ExecutionJobId, ExecutionMessageId, LeaseId, RequestId, WorkerSessionId,
    };
    use winwincode_execution_port::generated::ExecutionPortMessage;
    let mut steps = Vec::new();
    for size in [1_000_u64, 10_000] {
        let root = root("pending");
        let store = crate::store::AdapterStore::open(&root).unwrap();
        let outbox = crate::outbox::ExecutionOutbox::open(store.clone()).unwrap();
        let ExecutionPortMessage::JobDispatchResultMessage(mut message) =
            fixture("job.dispatch_result")
        else {
            panic!("transport fixture")
        };
        for sequence in 1..=size {
            message.message_id = ExecutionMessageId(format!("xmsg_{sequence:026}"));
            message.request_id = RequestId(format!("req_{sequence:026}"));
            message.job_id = ExecutionJobId(format!("job_{sequence:026}"));
            message.lease.job_id = message.job_id.clone();
            message.lease.lease_id = LeaseId(format!("lse_{sequence:026}"));
            message.worker_session_id = Some(WorkerSessionId(format!("wsn_{sequence:026}")));
            let delivery = outbox
                .retain(&ExecutionPortMessage::JobDispatchResultMessage(
                    message.clone(),
                ))
                .unwrap();
            outbox.record_sent(&delivery.delivery_id).unwrap();
        }
        reset();
        assert!(outbox.pending_batch(None, 100).unwrap().is_empty());
        let metrics = finish();
        println!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({"id":"M07", "size":size, "metrics":metrics, "empty":true})
        );
        steps.push(metrics.pending_vm_steps);
        drop(outbox);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    assert!(steps[1] > 8 * steps[0]);
}

#[test]
#[ignore = "mechanism audit: exact runtime ACK with large cross-stream backlog"]
fn storage_c02_real_runtime_ack_decodes_other_stream_backlog_and_compacts_only_target() {
    use winwincode_domain::ExecutionAckSequence;
    use winwincode_execution_port::{
        generated::ExecutionPortMessage, runtime_replay::RuntimeReplayAckReceipt,
    };
    for size in [100_u64, 200, 400] {
        let root = root("ack-backlog");
        let store = crate::store::AdapterStore::open(&root).unwrap();
        let outbox = crate::outbox::ExecutionOutbox::open(store.clone()).unwrap();
        for stream in 1..=size {
            outbox
                .retain(&ExecutionPortMessage::RuntimeEventMessage(runtime(
                    1, stream,
                )))
                .unwrap();
        }
        let message = ack(&runtime(1, 1));
        let receipt = RuntimeReplayAckReceipt {
            status: message.status.clone(),
            ack_sequence: ExecutionAckSequence(1),
            highest_sequence: ExecutionAckSequence(1),
            replay_from_sequence: None,
            replay: None,
        };
        reset();
        assert!(
            outbox
                .apply_runtime_ack(&message, &receipt)
                .unwrap()
                .is_empty()
        );
        let metrics = finish();
        assert_eq!(metrics.runtime_rows, size);
        assert_eq!(
            outbox.pending().unwrap().len(),
            usize::try_from(size - 1).unwrap()
        );
        reset();
        outbox.apply_runtime_ack(&message, &receipt).unwrap();
        let duplicate = finish();
        assert_eq!(duplicate.runtime_rows, size - 1);
        println!(
            "STORAGE_MECHANISM {}",
            serde_json::json!({"id":"STORAGE-C02", "size":size, "metrics":metrics, "remaining_other_streams":size-1, "duplicate":duplicate, "classification":"conditional backlog cost"})
        );
        drop(outbox);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
