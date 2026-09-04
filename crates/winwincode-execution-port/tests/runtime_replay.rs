use std::{collections::BTreeMap, fs, path::PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use winwincode_domain::{
    ExecutionAckSequence, ExecutionEventId, ExecutionMessageId, ExecutionSequence,
};
use winwincode_execution_port::generated::{
    ExecutionPortMessage, LeaseWriteStatus, RuntimeAckMessage, RuntimeAckMessageKind,
    RuntimeEventMessage, RuntimeReplayRequestMessage,
};
use winwincode_execution_port::replay::{
    ReplayAcknowledgementStore, ReplayAuthority, ReplayError, ReplayFrame, ReplaySequence,
    ReplaySnapshot, ReplayStore, ReplayStreamKey,
};
use winwincode_execution_port::runtime_replay::{
    RuntimeReplayBatch, RuntimeReplayCore, RuntimeReplayError, RuntimeReplayIdentity,
    RuntimeReplayOutput, RuntimeReplayResponder, runtime_ack_stream_key, runtime_event_stream_key,
    runtime_replay_stream_key,
};
use winwincode_execution_port::transport::{
    EndpointSide, FrameDirection, LocalWorkerAdapter, RemoteTransportAdapter, TypedFrame,
};

const VALID_FIXTURE: &str =
    include_str!("../../../tests/fixtures/contracts/execution-port.valid.json");

#[derive(Debug, Clone, PartialEq, Eq)]
enum AuthorityError {
    Expired,
    Stale,
}

#[derive(Debug, Clone)]
struct WorkerAuthority {
    valid: bool,
    error: AuthorityError,
}

impl ReplayAuthority for WorkerAuthority {
    type Context = RuntimeReplayIdentity;
    type Error = AuthorityError;

    fn validate_active_lease(
        &self,
        _stream: &ReplayStreamKey,
        _identity: &Self::Context,
    ) -> Result<(), Self::Error> {
        if self.valid {
            Ok(())
        } else {
            Err(self.error.clone())
        }
    }
}

#[derive(Debug, Clone, Default)]
struct MemoryStore {
    snapshots: BTreeMap<ReplayStreamKey, ReplaySnapshot>,
    loads: usize,
    writes: usize,
}

impl ReplayStore for MemoryStore {
    type Error = &'static str;

    fn load(&mut self, stream: &ReplayStreamKey) -> Result<Option<ReplaySnapshot>, Self::Error> {
        self.loads += 1;
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
            return Err("replay highest sequence changed");
        }
        snapshot.events.push(frame.clone());
        snapshot.highest_sequence += 1;
        self.writes += 1;
        Ok(())
    }
}

impl ReplayAcknowledgementStore for MemoryStore {
    fn record_acknowledgement(
        &mut self,
        stream: &ReplayStreamKey,
        expected_ack_sequence: ReplaySequence,
        ack_sequence: ReplaySequence,
    ) -> Result<(), Self::Error> {
        let snapshot = self
            .snapshots
            .get_mut(stream)
            .ok_or("replay stream missing")?;
        if snapshot.ack_sequence != expected_ack_sequence {
            return Err("replay acknowledgement changed");
        }
        if ack_sequence > snapshot.highest_sequence {
            return Err("replay acknowledgement exceeds retained range");
        }
        snapshot.ack_sequence = ack_sequence;
        self.writes += 1;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileFrame {
    event_id: String,
    sequence: ReplaySequence,
    digest: String,
    frame: Vec<u8>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FileSnapshot {
    ack_sequence: ReplaySequence,
    highest_sequence: ReplaySequence,
    events: Vec<FileFrame>,
}

/// Re-openable test-only durable store. The production Worker injects its own
/// durable adapter at this seam; this fixture makes restart a real file close
/// and reopen rather than moving an in-memory map.
#[derive(Debug, Clone)]
struct FileStore {
    path: PathBuf,
    loads: usize,
    writes: usize,
}

impl FileStore {
    fn open(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            loads: 0,
            writes: 0,
        }
    }

    fn read_all(&self) -> Result<BTreeMap<String, FileSnapshot>, String> {
        if !self.path.exists() {
            return Ok(BTreeMap::new());
        }
        let bytes = fs::read(&self.path).map_err(|error| error.to_string())?;
        if bytes.is_empty() {
            return Ok(BTreeMap::new());
        }
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())
    }

    fn write_all(&self, rows: &BTreeMap<String, FileSnapshot>) -> Result<(), String> {
        let bytes = serde_json::to_vec(rows).map_err(|error| error.to_string())?;
        fs::write(&self.path, bytes).map_err(|error| error.to_string())
    }
}

impl ReplayStore for FileStore {
    type Error = String;

    fn load(&mut self, stream: &ReplayStreamKey) -> Result<Option<ReplaySnapshot>, Self::Error> {
        self.loads += 1;
        let rows = self.read_all()?;
        Ok(rows
            .get(stream.as_str())
            .cloned()
            .map(|snapshot| ReplaySnapshot {
                ack_sequence: snapshot.ack_sequence,
                highest_sequence: snapshot.highest_sequence,
                events: snapshot
                    .events
                    .into_iter()
                    .map(|frame| {
                        ReplayFrame::new(frame.event_id, frame.sequence, frame.digest, frame.frame)
                    })
                    .collect(),
            }))
    }

    fn append(
        &mut self,
        stream: &ReplayStreamKey,
        expected_highest_sequence: ReplaySequence,
        frame: &ReplayFrame,
    ) -> Result<(), Self::Error> {
        let mut rows = self.read_all()?;
        let snapshot = rows.entry(stream.as_str().to_owned()).or_default();
        if snapshot.highest_sequence != expected_highest_sequence {
            return Err("replay highest sequence changed".to_owned());
        }
        snapshot.events.push(FileFrame {
            event_id: frame.event_id.clone(),
            sequence: frame.sequence,
            digest: frame.digest.clone(),
            frame: frame.frame.clone(),
        });
        snapshot.highest_sequence = expected_highest_sequence
            .checked_add(1)
            .ok_or_else(|| "replay sequence overflow".to_owned())?;
        self.write_all(&rows)?;
        self.writes += 1;
        Ok(())
    }
}

impl ReplayAcknowledgementStore for FileStore {
    fn record_acknowledgement(
        &mut self,
        stream: &ReplayStreamKey,
        expected_ack_sequence: ReplaySequence,
        ack_sequence: ReplaySequence,
    ) -> Result<(), Self::Error> {
        let mut rows = self.read_all()?;
        let snapshot = rows
            .get_mut(stream.as_str())
            .ok_or_else(|| "replay stream missing".to_owned())?;
        if snapshot.ack_sequence != expected_ack_sequence {
            return Err("replay acknowledgement changed".to_owned());
        }
        if ack_sequence > snapshot.highest_sequence {
            return Err("replay acknowledgement exceeds retained range".to_owned());
        }
        snapshot.ack_sequence = ack_sequence;
        self.write_all(&rows)?;
        self.writes += 1;
        Ok(())
    }
}

fn fixture_message(kind: &str) -> Value {
    let fixture: Value = serde_json::from_str(VALID_FIXTURE).expect("fixture JSON");
    fixture["messages"]
        .as_array()
        .expect("fixture messages")
        .iter()
        .find(|message| message["kind"] == kind)
        .cloned()
        .expect("fixture message kind")
}

fn runtime_event() -> RuntimeEventMessage {
    serde_json::from_value(fixture_message("runtime.event")).expect("runtime event fixture")
}

fn replay_request(event: &RuntimeEventMessage) -> RuntimeReplayRequestMessage {
    let mut request: RuntimeReplayRequestMessage =
        serde_json::from_value(fixture_message("runtime.replay_request"))
            .expect("replay request fixture");
    request.lease = event.lease.clone();
    request.session_identity = event.session_identity.clone();
    request.worker_session_id = event.worker_session_id.clone();
    request
}

fn runtime_ack(
    event: &RuntimeEventMessage,
    status: LeaseWriteStatus,
    ack_sequence: i64,
    replay_from_sequence: Option<i64>,
) -> RuntimeAckMessage {
    let mut message: RuntimeAckMessage =
        serde_json::from_value(fixture_message("runtime.ack")).expect("runtime ack fixture");
    message.kind = RuntimeAckMessageKind::RuntimeAck;
    message.lease = event.lease.clone();
    message.session_identity = event.session_identity.clone();
    message.worker_session_id = event.worker_session_id.clone();
    message.status = status;
    message.ack_sequence = ExecutionAckSequence(ack_sequence);
    message.replay_from_sequence = replay_from_sequence.map(ExecutionSequence);
    message
}

fn seed_event(store: &mut MemoryStore, authority: &WorkerAuthority, event: &RuntimeEventMessage) {
    RuntimeReplayResponder::new()
        .retain_runtime_event(store, authority, event)
        .expect("worker accepted replay frame");
}

fn runtime_messages() -> (
    RuntimeEventMessage,
    RuntimeEventMessage,
    RuntimeEventMessage,
    RuntimeReplayRequestMessage,
) {
    let first = runtime_event();
    let mut second = first.clone();
    second.event.event_id = ExecutionEventId("xevt_0000000000000000000000000C".into());
    second.event.sequence = ExecutionSequence(2);
    second.message_id = ExecutionMessageId("xmsg_0000000000000000000000000C".into());
    let mut third = second.clone();
    third.event.event_id = ExecutionEventId("xevt_0000000000000000000000000D".into());
    third.event.sequence = ExecutionSequence(3);
    third.message_id = ExecutionMessageId("xmsg_0000000000000000000000000D".into());
    let request = replay_request(&first);
    (first, second, third, request)
}

fn seeded_runtime() -> (
    MemoryStore,
    WorkerAuthority,
    RuntimeEventMessage,
    RuntimeEventMessage,
    RuntimeEventMessage,
    RuntimeReplayRequestMessage,
) {
    let (first, second, third, request) = runtime_messages();
    let authority = WorkerAuthority {
        valid: true,
        error: AuthorityError::Expired,
    };
    let mut store = MemoryStore::default();
    seed_event(&mut store, &authority, &first);
    seed_event(&mut store, &authority, &second);
    seed_event(&mut store, &authority, &third);
    (store, authority, first, second, third, request)
}

#[test]
fn worker_replay_core_returns_original_frames_after_ack_and_uses_the_same_stream_key() {
    let (store, authority, first, second, _third, mut request) = seeded_runtime();
    request.after_sequence = ExecutionAckSequence(1);
    request.max_events = 1;

    assert_eq!(
        runtime_event_stream_key(&first),
        runtime_replay_stream_key(&request)
    );
    assert_eq!(
        runtime_event_stream_key(&first),
        runtime_ack_stream_key(&runtime_ack(&first, LeaseWriteStatus::Accepted, 0, None,))
    );

    let mut core = RuntimeReplayCore::new(store, authority.clone());
    let typed = TypedFrame::new(
        FrameDirection::ControlPlaneToWorker,
        ExecutionPortMessage::RuntimeReplayRequestMessage(request.clone()),
    )
    .expect("control-plane replay request frame");
    let replay = LocalWorkerAdapter::new(&mut core, EndpointSide::Worker)
        .accept(&typed)
        .expect("worker local replay");
    assert_eq!(
        replay,
        RuntimeReplayOutput::Replay(RuntimeReplayBatch {
            ack_sequence: ExecutionAckSequence(0),
            highest_sequence: ExecutionAckSequence(3),
            events: vec![second.clone()],
        })
    );

    let persisted = core.into_store();
    assert_eq!(persisted.writes, 3);
    assert_eq!(persisted.loads, 4, "three accepts and one resume load");
}

#[test]
fn local_and_remote_worker_replay_responders_match_and_restart_from_the_same_store() {
    let (local_store, authority, _first, _second, _third, mut request) = seeded_runtime();
    let (remote_store, remote_authority, _first, second, third, _) = seeded_runtime();
    request.after_sequence = ExecutionAckSequence(1);
    request.max_events = 100;
    let frame = TypedFrame::new(
        FrameDirection::ControlPlaneToWorker,
        ExecutionPortMessage::RuntimeReplayRequestMessage(request.clone()),
    )
    .expect("control-plane replay request frame");
    let encoded =
        RemoteTransportAdapter::<RuntimeReplayCore<MemoryStore, WorkerAuthority>>::encode(&frame)
            .expect("remote replay request encoding");

    let mut local_core = RuntimeReplayCore::new(local_store, authority.clone());
    let local = LocalWorkerAdapter::new(&mut local_core, EndpointSide::Worker)
        .accept(&frame)
        .expect("local worker replay");
    let mut remote_core = RuntimeReplayCore::new(remote_store, remote_authority);
    let remote = RemoteTransportAdapter::new(&mut remote_core, EndpointSide::Worker)
        .accept(&encoded)
        .expect("remote worker replay");
    let expected = RuntimeReplayOutput::Replay(RuntimeReplayBatch {
        ack_sequence: ExecutionAckSequence(0),
        highest_sequence: ExecutionAckSequence(3),
        events: vec![second, third],
    });
    assert_eq!(local, expected.clone());
    assert_eq!(remote, expected.clone());

    let durable_store = local_core.into_store();
    let mut restarted_core = RuntimeReplayCore::new(durable_store, authority);
    let resumed = LocalWorkerAdapter::new(&mut restarted_core, EndpointSide::Worker)
        .accept(&frame)
        .expect("replay after Worker restart");
    assert_eq!(resumed, expected);
}

#[test]
fn worker_restart_reopens_a_durable_file_and_replays_original_frames() {
    let path = std::env::temp_dir().join(format!(
        "winwincode-runtime-replay-{}-{}.json",
        std::process::id(),
        1_u128
    ));
    let _ = fs::remove_file(&path);
    let (first, second, third, mut request) = runtime_messages();
    let authority = WorkerAuthority {
        valid: true,
        error: AuthorityError::Expired,
    };
    {
        let mut store = FileStore::open(&path);
        let responder = RuntimeReplayResponder::new();
        responder
            .retain_runtime_event(&mut store, &authority, &first)
            .expect("first event is retained");
        responder
            .retain_runtime_event(&mut store, &authority, &second)
            .expect("second event is retained");
        responder
            .retain_runtime_event(&mut store, &authority, &third)
            .expect("third event is retained");
        responder
            .acknowledge(
                &mut store,
                &authority,
                &runtime_ack(&first, LeaseWriteStatus::Accepted, 1, None),
            )
            .expect("Control Plane acknowledgement is durable");
    }

    request.after_sequence = ExecutionAckSequence(1);
    request.max_events = 100;
    let frame = TypedFrame::new(
        FrameDirection::ControlPlaneToWorker,
        ExecutionPortMessage::RuntimeReplayRequestMessage(request),
    )
    .expect("control-plane replay request frame");
    let mut restarted_core = RuntimeReplayCore::new(FileStore::open(&path), authority);
    let resumed = LocalWorkerAdapter::new(&mut restarted_core, EndpointSide::Worker)
        .accept(&frame)
        .expect("replay after Worker process restart");
    assert_eq!(
        resumed,
        RuntimeReplayOutput::Replay(RuntimeReplayBatch {
            ack_sequence: ExecutionAckSequence(1),
            highest_sequence: ExecutionAckSequence(3),
            events: vec![second, third],
        })
    );
    let _ = fs::remove_file(path);
}

#[test]
fn worker_ack_advances_only_the_control_plane_watermark_and_gap_replays() {
    let (mut store, authority, first, second, third, _request) = seeded_runtime();
    let responder = RuntimeReplayResponder::new();

    let gap = runtime_ack(&first, LeaseWriteStatus::Gap, 0, Some(1));
    let gap_receipt = responder
        .acknowledge(&mut store, &authority, &gap)
        .expect("gap acknowledgement is handled by the Worker");
    assert_eq!(gap_receipt.ack_sequence, ExecutionAckSequence(0));
    assert_eq!(gap_receipt.highest_sequence, ExecutionAckSequence(3));
    assert_eq!(
        gap_receipt
            .replay
            .expect("gap acknowledgement returns replay")
            .events,
        vec![first.clone(), second.clone(), third.clone()]
    );

    let accepted = runtime_ack(&first, LeaseWriteStatus::Accepted, 1, None);
    let accepted_receipt = responder
        .acknowledge(&mut store, &authority, &accepted)
        .expect("accepted acknowledgement advances watermark");
    assert_eq!(accepted_receipt.ack_sequence, ExecutionAckSequence(1));
    assert!(accepted_receipt.replay.is_none());

    let duplicate = runtime_ack(&first, LeaseWriteStatus::Duplicate, 1, None);
    let writes_before_duplicate = store.writes;
    let duplicate_receipt = responder
        .acknowledge(&mut store, &authority, &duplicate)
        .expect("duplicate acknowledgement is idempotent");
    assert_eq!(duplicate_receipt.ack_sequence, ExecutionAckSequence(1));
    assert_eq!(store.writes, writes_before_duplicate);

    let later = runtime_ack(&third, LeaseWriteStatus::Accepted, 3, None);
    let later_receipt = responder
        .acknowledge(&mut store, &authority, &later)
        .expect("later acknowledgement advances the contiguous watermark");
    assert_eq!(later_receipt.ack_sequence, ExecutionAckSequence(3));
    let writes_before_stale_duplicate = store.writes;
    let stale_duplicate = runtime_ack(&first, LeaseWriteStatus::Duplicate, 1, None);
    let stale_duplicate_receipt = responder
        .acknowledge(&mut store, &authority, &stale_duplicate)
        .expect("older duplicate response keeps the durable watermark");
    assert_eq!(
        stale_duplicate_receipt.ack_sequence,
        ExecutionAckSequence(3)
    );
    assert_eq!(store.writes, writes_before_stale_duplicate);

    let regression = runtime_ack(&first, LeaseWriteStatus::Accepted, 0, None);
    assert!(matches!(
        responder.acknowledge(&mut store, &authority, &regression),
        Err(RuntimeReplayError::Replay(
            ReplayError::AckRegression { .. }
        ))
    ));
    let ahead = runtime_ack(&first, LeaseWriteStatus::Accepted, 4, None);
    assert!(matches!(
        responder.acknowledge(&mut store, &authority, &ahead),
        Err(RuntimeReplayError::Replay(ReplayError::AckAhead { .. }))
    ));
}

#[test]
fn runtime_ack_uses_the_same_worker_core_transport_seam() {
    let (store, authority, first, _second, _third, _request) = seeded_runtime();
    let ack = runtime_ack(&first, LeaseWriteStatus::Accepted, 1, None);
    let frame = TypedFrame::new(
        FrameDirection::ControlPlaneToWorker,
        ExecutionPortMessage::RuntimeAckMessage(ack),
    )
    .expect("control-plane runtime acknowledgement frame");
    let mut core = RuntimeReplayCore::new(store, authority);
    let output = LocalWorkerAdapter::new(&mut core, EndpointSide::Worker)
        .accept(&frame)
        .expect("worker core accepts runtime acknowledgement");
    let RuntimeReplayOutput::Ack(receipt) = output else {
        panic!("runtime acknowledgement must produce an acknowledgement receipt");
    };
    assert_eq!(receipt.ack_sequence, ExecutionAckSequence(1));
    assert_eq!(receipt.highest_sequence, ExecutionAckSequence(3));
    let persisted = core.into_store();
    let snapshot = persisted.snapshots.values().next().expect("runtime stream");
    assert_eq!(snapshot.ack_sequence, 1);
    assert_eq!(snapshot.highest_sequence, 3);
}

#[test]
fn runtime_ack_rejects_an_inactive_lease_before_reading_the_store() {
    let (mut store, _authority, first, _second, _third, _request) = seeded_runtime();
    store.loads = 0;
    let authority = WorkerAuthority {
        valid: false,
        error: AuthorityError::Stale,
    };
    let frame = TypedFrame::new(
        FrameDirection::ControlPlaneToWorker,
        ExecutionPortMessage::RuntimeAckMessage(runtime_ack(
            &first,
            LeaseWriteStatus::Accepted,
            1,
            None,
        )),
    )
    .expect("control-plane runtime acknowledgement frame");
    let mut core = RuntimeReplayCore::new(store, authority);
    let error = LocalWorkerAdapter::new(&mut core, EndpointSide::Worker)
        .accept(&frame)
        .expect_err("inactive lease must fail closed");
    assert_eq!(
        error,
        winwincode_execution_port::transport::AdapterError::Core(RuntimeReplayError::Replay(
            ReplayError::Authority(AuthorityError::Stale)
        ))
    );
    assert_eq!(core.into_store().loads, 0);
}

#[test]
fn worker_retain_handles_duplicate_changed_body_and_gap_without_advancing_highest() {
    let (first, second, third, _request) = runtime_messages();
    let authority = WorkerAuthority {
        valid: true,
        error: AuthorityError::Expired,
    };
    let responder = RuntimeReplayResponder::new();
    let mut store = MemoryStore::default();
    assert!(matches!(
        responder.retain_runtime_event(&mut store, &authority, &first),
        Ok(
            winwincode_execution_port::replay::ReplayDecision::Accepted {
                highest_sequence: 1
            }
        )
    ));
    let duplicate = responder
        .retain_runtime_event(&mut store, &authority, &first)
        .expect("exact duplicate is a protocol result");
    assert!(matches!(
        duplicate,
        winwincode_execution_port::replay::ReplayDecision::Duplicate {
            highest_sequence: 1,
            ..
        }
    ));
    let mut changed = first.clone();
    changed.event.summary.push_str(" changed");
    let conflict = responder
        .retain_runtime_event(&mut store, &authority, &changed)
        .expect("changed duplicate is a protocol result");
    assert_eq!(
        conflict,
        winwincode_execution_port::replay::ReplayDecision::Conflict {
            highest_sequence: 1
        }
    );
    let gap = responder
        .retain_runtime_event(&mut store, &authority, &third)
        .expect("future event is a gap result");
    assert_eq!(
        gap,
        winwincode_execution_port::replay::ReplayDecision::Gap {
            highest_sequence: 1,
            replay_from_sequence: 2
        }
    );
    assert_eq!(
        store
            .snapshots
            .values()
            .next()
            .expect("stream")
            .highest_sequence,
        1
    );
    assert_eq!(store.writes, 1);
    let _ = second;
}

#[test]
fn worker_retain_treats_a_new_envelope_id_as_duplicate_for_the_same_event_digest() {
    let first = runtime_event();
    let authority = WorkerAuthority {
        valid: true,
        error: AuthorityError::Expired,
    };
    let responder = RuntimeReplayResponder::new();
    let mut store = MemoryStore::default();
    assert!(matches!(
        responder.retain_runtime_event(&mut store, &authority, &first),
        Ok(
            winwincode_execution_port::replay::ReplayDecision::Accepted {
                highest_sequence: 1
            }
        )
    ));

    let mut replay = first.clone();
    replay.message_id = ExecutionMessageId("xmsg_0000000000000000000000000C".into());
    let duplicate = responder
        .retain_runtime_event(&mut store, &authority, &replay)
        .expect("same event digest with a new envelope id is a duplicate");
    assert!(matches!(
        duplicate,
        winwincode_execution_port::replay::ReplayDecision::Duplicate {
            highest_sequence: 1,
            ..
        }
    ));

    replay.event.summary.push_str(" changed");
    assert_eq!(
        responder
            .retain_runtime_event(&mut store, &authority, &replay)
            .expect("changed event body is a conflict"),
        winwincode_execution_port::replay::ReplayDecision::Conflict {
            highest_sequence: 1
        }
    );
    assert_eq!(store.writes, 1);
}

#[test]
fn worker_replay_rejects_inactive_lease_before_reading_the_store() {
    let (mut store, _authority, _first, _second, _third, request) = seeded_runtime();
    store.loads = 0;
    let authority = WorkerAuthority {
        valid: false,
        error: AuthorityError::Stale,
    };
    let mut core = RuntimeReplayCore::new(store, authority);
    let frame = TypedFrame::new(
        FrameDirection::ControlPlaneToWorker,
        ExecutionPortMessage::RuntimeReplayRequestMessage(request),
    )
    .expect("control-plane replay request frame");
    let error = LocalWorkerAdapter::new(&mut core, EndpointSide::Worker)
        .accept(&frame)
        .expect_err("inactive lease must fail closed");
    assert_eq!(
        error,
        winwincode_execution_port::transport::AdapterError::Core(RuntimeReplayError::Replay(
            ReplayError::Authority(AuthorityError::Stale)
        ))
    );
    assert_eq!(core.into_store().loads, 0);
}
