use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use winwincode_execution_port::replay::{
    ReplayAcknowledgementStore, ReplayAuthority, ReplayDecision, ReplayError, ReplayFrame,
    ReplaySequence, ReplaySnapshot, ReplayStateError, ReplayStateMachine, ReplayStore,
    ReplayStreamKey,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct LeaseContext {
    worker_instance_id: String,
    fencing_token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthorityError {
    ExpiredLease,
    StaleFencingToken,
}

#[derive(Debug, Clone, Copy)]
struct FakeAuthority {
    valid: bool,
    error: AuthorityError,
}

impl ReplayAuthority for FakeAuthority {
    type Context = LeaseContext;
    type Error = AuthorityError;

    fn validate_active_lease(
        &self,
        _stream: &ReplayStreamKey,
        _context: &Self::Context,
    ) -> Result<(), Self::Error> {
        if self.valid { Ok(()) } else { Err(self.error) }
    }
}

#[derive(Debug, Clone, Default)]
struct DurableRows(BTreeMap<ReplayStreamKey, ReplaySnapshot>);

#[derive(Debug, Clone, Default)]
struct FakeStore {
    durable: DurableRows,
    loads: usize,
    writes: usize,
}

impl FakeStore {
    fn from_durable(durable: DurableRows) -> Self {
        Self {
            durable,
            loads: 0,
            writes: 0,
        }
    }

    fn restart(&self) -> Self {
        Self::from_durable(self.durable.clone())
    }
}

impl ReplayStore for FakeStore {
    type Error = &'static str;

    fn load(&mut self, stream: &ReplayStreamKey) -> Result<Option<ReplaySnapshot>, Self::Error> {
        self.loads += 1;
        // This fake intentionally keeps rows in a cloneable durable map; a real
        // adapter must implement the same operations in one SQLite transaction.
        Ok(self.durable.0.get(stream).cloned())
    }

    fn append(
        &mut self,
        stream: &ReplayStreamKey,
        expected_ack_sequence: ReplaySequence,
        frame: &ReplayFrame,
    ) -> Result<(), Self::Error> {
        let snapshot = self.durable.0.entry(stream.clone()).or_default();
        if snapshot.highest_sequence != expected_ack_sequence {
            return Err("expected ack sequence changed");
        }
        snapshot.events.push(frame.clone());
        snapshot.highest_sequence = expected_ack_sequence
            .checked_add(1)
            .ok_or("replay sequence overflow")?;
        self.writes += 1;
        Ok(())
    }
}

impl ReplayAcknowledgementStore for FakeStore {
    fn record_acknowledgement(
        &mut self,
        stream: &ReplayStreamKey,
        expected_ack_sequence: ReplaySequence,
        ack_sequence: ReplaySequence,
    ) -> Result<(), Self::Error> {
        let snapshot = self
            .durable
            .0
            .get_mut(stream)
            .ok_or("replay stream missing")?;
        if snapshot.ack_sequence != expected_ack_sequence {
            return Err("expected acknowledgement sequence changed");
        }
        if ack_sequence > snapshot.highest_sequence {
            return Err("acknowledgement exceeds highest sequence");
        }
        snapshot.ack_sequence = ack_sequence;
        self.writes += 1;
        Ok(())
    }
}

impl FakeStore {
    fn prune_acknowledged(&mut self, stream: &ReplayStreamKey) {
        let snapshot = self
            .durable
            .0
            .get_mut(stream)
            .expect("replay stream exists before pruning");
        let ack_sequence = snapshot.ack_sequence;
        snapshot
            .events
            .retain(|event| event.sequence > ack_sequence);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedFrame {
    event_id: String,
    sequence: ReplaySequence,
    digest: String,
    frame: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedSnapshot {
    ack_sequence: ReplaySequence,
    highest_sequence: ReplaySequence,
    events: Vec<PersistedFrame>,
}

#[derive(Debug, Clone)]
struct ReopenableFileStore {
    path: PathBuf,
    loads: usize,
    writes: usize,
}

impl ReopenableFileStore {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            loads: 0,
            writes: 0,
        }
    }

    fn reopen(&self) -> Self {
        Self::new(self.path.clone())
    }

    fn read_snapshot(&self) -> Result<Option<ReplaySnapshot>, String> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("read replay fixture: {error}")),
        };
        let persisted: PersistedSnapshot = serde_json::from_slice(&bytes)
            .map_err(|error| format!("decode replay fixture: {error}"))?;
        Ok(Some(ReplaySnapshot::from(persisted)))
    }

    fn write_snapshot(&self, snapshot: &ReplaySnapshot) -> Result<(), String> {
        let bytes = serde_json::to_vec(&PersistedSnapshot::from(snapshot))
            .map_err(|error| format!("encode replay fixture: {error}"))?;
        let temporary_path = self.path.with_extension("json.tmp");
        std::fs::write(&temporary_path, bytes)
            .map_err(|error| format!("write replay fixture: {error}"))?;
        std::fs::rename(&temporary_path, &self.path)
            .map_err(|error| format!("replace replay fixture: {error}"))
    }
}

impl ReplayStore for ReopenableFileStore {
    type Error = String;

    fn load(&mut self, _stream: &ReplayStreamKey) -> Result<Option<ReplaySnapshot>, Self::Error> {
        self.loads += 1;
        self.read_snapshot()
    }

    fn append(
        &mut self,
        _stream: &ReplayStreamKey,
        expected_highest_sequence: ReplaySequence,
        frame: &ReplayFrame,
    ) -> Result<(), Self::Error> {
        let mut snapshot = self.read_snapshot()?.unwrap_or_default();
        if snapshot.highest_sequence != expected_highest_sequence {
            return Err(format!(
                "expected highest sequence {expected_highest_sequence}, found {}",
                snapshot.highest_sequence
            ));
        }
        let next_sequence = expected_highest_sequence
            .checked_add(1)
            .ok_or_else(|| "replay sequence overflow".to_owned())?;
        if frame.sequence != next_sequence {
            return Err(format!(
                "expected frame sequence {next_sequence}, found {}",
                frame.sequence
            ));
        }
        snapshot.events.push(frame.clone());
        snapshot.highest_sequence = next_sequence;
        self.write_snapshot(&snapshot)?;
        self.writes += 1;
        Ok(())
    }
}

impl From<PersistedSnapshot> for ReplaySnapshot {
    fn from(snapshot: PersistedSnapshot) -> Self {
        Self {
            ack_sequence: snapshot.ack_sequence,
            highest_sequence: snapshot.highest_sequence,
            events: snapshot.events.into_iter().map(ReplayFrame::from).collect(),
        }
    }
}

impl From<&ReplaySnapshot> for PersistedSnapshot {
    fn from(snapshot: &ReplaySnapshot) -> Self {
        Self {
            ack_sequence: snapshot.ack_sequence,
            highest_sequence: snapshot.highest_sequence,
            events: snapshot.events.iter().map(PersistedFrame::from).collect(),
        }
    }
}

impl From<PersistedFrame> for ReplayFrame {
    fn from(frame: PersistedFrame) -> Self {
        Self {
            event_id: frame.event_id,
            sequence: frame.sequence,
            digest: frame.digest,
            frame: frame.frame,
        }
    }
}

impl From<&ReplayFrame> for PersistedFrame {
    fn from(frame: &ReplayFrame) -> Self {
        Self {
            event_id: frame.event_id.clone(),
            sequence: frame.sequence,
            digest: frame.digest.clone(),
            frame: frame.frame.clone(),
        }
    }
}

static NEXT_REPLAY_FIXTURE: AtomicU64 = AtomicU64::new(0);

fn temporary_replay_path(stream_label: &str) -> PathBuf {
    let ordinal = NEXT_REPLAY_FIXTURE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-execution-port-{stream_label}-{}-{ordinal}.json",
        std::process::id()
    ))
}

// The tests exercise only the public replay seam. The fake store is cloneable
// to model a process restart while keeping the same durable rows.
#[test]
fn disconnect_then_resume_replays_only_events_after_the_highest_acknowledged_sequence() {
    let machine = ReplayStateMachine::new();
    let stream = ReplayStreamKey::new("runtime:job_1:lease_1");
    let lease = LeaseContext {
        worker_instance_id: "worker-instance-1".to_owned(),
        fencing_token: "1".to_owned(),
    };
    let authority = FakeAuthority {
        valid: true,
        error: AuthorityError::ExpiredLease,
    };
    let mut store = FakeStore::default();

    for sequence in 1..=3 {
        let outcome = machine
            .accept(
                &mut store,
                &authority,
                &stream,
                &lease,
                &frame(
                    sequence,
                    &format!("evt_{sequence}"),
                    &format!("digest-{sequence}"),
                ),
            )
            .expect("contiguous event is accepted");
        assert_eq!(
            outcome,
            ReplayDecision::Accepted {
                highest_sequence: sequence
            }
        );
    }

    let mut restarted_store = store.restart();
    let restarted_machine = ReplayStateMachine::new();
    let replay = restarted_machine
        .resume(&mut restarted_store, &authority, &stream, &lease, 1, 100)
        .expect("active lease can resume");

    assert_eq!(
        replay.ack_sequence, 0,
        "the Control Plane has not acknowledged frames"
    );
    assert_eq!(replay.highest_sequence, 3);
    assert_eq!(
        replay
            .events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert_eq!(
        replay.events,
        vec![frame(2, "evt_2", "digest-2"), frame(3, "evt_3", "digest-3"),]
    );
    assert_eq!(restarted_store.writes, 0, "resume is read-only");

    let accepted_after_restart = restarted_machine
        .accept(
            &mut restarted_store,
            &authority,
            &stream,
            &lease,
            &frame(4, "evt_4", "digest-4"),
        )
        .expect("a restarted process can continue the durable stream");
    assert_eq!(
        accepted_after_restart,
        ReplayDecision::Accepted {
            highest_sequence: 4
        }
    );
}

#[test]
fn acknowledged_prefix_can_be_pruned_and_resumed_after_restart() {
    let machine = ReplayStateMachine::new();
    let stream = ReplayStreamKey::new("runtime:job_pruned:lease_1");
    let lease = LeaseContext {
        worker_instance_id: "worker-instance-1".to_owned(),
        fencing_token: "1".to_owned(),
    };
    let authority = FakeAuthority {
        valid: true,
        error: AuthorityError::ExpiredLease,
    };
    let mut store = FakeStore::default();

    for sequence in 1..=7 {
        machine
            .accept(
                &mut store,
                &authority,
                &stream,
                &lease,
                &frame(
                    sequence,
                    &format!("evt_pruned_{sequence}"),
                    &format!("digest-pruned-{sequence}"),
                ),
            )
            .expect("contiguous event is accepted");
    }
    assert_eq!(
        machine.acknowledge(&mut store, &authority, &stream, &lease, 5),
        Ok(5)
    );
    store.prune_acknowledged(&stream);

    let mut restarted_store = store.restart();
    let replay = machine
        .resume(&mut restarted_store, &authority, &stream, &lease, 5, 100)
        .expect("acknowledged frames may be pruned before restart");

    assert_eq!(replay.ack_sequence, 5);
    assert_eq!(replay.highest_sequence, 7);
    assert_eq!(
        replay.events,
        vec![
            frame(6, "evt_pruned_6", "digest-pruned-6"),
            frame(7, "evt_pruned_7", "digest-pruned-7"),
        ]
    );
}

#[test]
fn unacknowledged_prefix_pruning_is_rejected_after_restart() {
    let stream = ReplayStreamKey::new("runtime:job_unacknowledged_pruned:lease_1");
    let lease = LeaseContext {
        worker_instance_id: "worker-instance-1".to_owned(),
        fencing_token: "1".to_owned(),
    };
    let authority = FakeAuthority {
        valid: true,
        error: AuthorityError::ExpiredLease,
    };
    let durable = DurableRows(BTreeMap::from([(
        stream.clone(),
        ReplaySnapshot {
            ack_sequence: 5,
            highest_sequence: 7,
            events: vec![frame(7, "evt_unacknowledged_7", "digest-unacknowledged-7")],
        },
    )]));
    let mut restarted_store = FakeStore::from_durable(durable).restart();

    let error = ReplayStateMachine::new()
        .resume(&mut restarted_store, &authority, &stream, &lease, 5, 100)
        .expect_err("an unacknowledged frame cannot be pruned");

    assert_eq!(
        error,
        ReplayError::CorruptState(ReplayStateError::NonContiguous {
            expected: 6,
            found: 7,
        })
    );
}

#[test]
fn a_gap_keeps_the_highest_contiguous_ack_and_replays_from_the_missing_sequence() {
    let machine = ReplayStateMachine::new();
    let stream = ReplayStreamKey::new("runtime:job_2:lease_1");
    let lease = LeaseContext {
        worker_instance_id: "worker-instance-1".to_owned(),
        fencing_token: "1".to_owned(),
    };
    let authority = FakeAuthority {
        valid: true,
        error: AuthorityError::ExpiredLease,
    };
    let mut store = FakeStore::default();

    machine
        .accept(
            &mut store,
            &authority,
            &stream,
            &lease,
            &frame(1, "evt_1", "digest-1"),
        )
        .expect("first event is accepted");
    let gap = machine
        .accept(
            &mut store,
            &authority,
            &stream,
            &lease,
            &frame(3, "evt_3", "digest-3"),
        )
        .expect("a gap is a protocol result");

    assert_eq!(
        gap,
        ReplayDecision::Gap {
            highest_sequence: 1,
            replay_from_sequence: 2,
        }
    );
    assert_eq!(store.writes, 1, "a gap is not persisted");

    let accepted = machine
        .accept(
            &mut store,
            &authority,
            &stream,
            &lease,
            &frame(2, "evt_2", "digest-2"),
        )
        .expect("missing event is accepted");
    assert_eq!(
        accepted,
        ReplayDecision::Accepted {
            highest_sequence: 2
        }
    );
}

#[test]
fn exact_replay_returns_the_original_frame_and_changed_body_is_a_conflict() {
    let machine = ReplayStateMachine::new();
    let stream = ReplayStreamKey::new("runtime:job_3:lease_1");
    let lease = LeaseContext {
        worker_instance_id: "worker-instance-1".to_owned(),
        fencing_token: "1".to_owned(),
    };
    let authority = FakeAuthority {
        valid: true,
        error: AuthorityError::ExpiredLease,
    };
    let mut store = FakeStore::default();
    let original = frame(1, "evt_1", "digest-1");

    let accepted = machine
        .accept(&mut store, &authority, &stream, &lease, &original)
        .expect("first event is accepted");
    assert_eq!(
        accepted,
        ReplayDecision::Accepted {
            highest_sequence: 1
        }
    );

    let duplicate = machine
        .accept(&mut store, &authority, &stream, &lease, &original)
        .expect("exact replay is a protocol result");
    assert_eq!(
        duplicate,
        ReplayDecision::Duplicate {
            highest_sequence: 1,
            original: original.clone(),
        }
    );
    assert_eq!(store.writes, 1, "duplicate does not append a row");

    let conflict = machine
        .accept(
            &mut store,
            &authority,
            &stream,
            &lease,
            &frame_with_body(1, "evt_1", "digest-changed", b"changed body"),
        )
        .expect("changed body is a protocol result");
    assert_eq!(
        conflict,
        ReplayDecision::Conflict {
            highest_sequence: 1
        }
    );
    assert_eq!(store.writes, 1, "conflict does not overwrite the original");
}

#[test]
fn stale_or_expired_authority_is_checked_before_the_store_can_write() {
    let machine = ReplayStateMachine::new();
    let stream = ReplayStreamKey::new("runtime:job_4:lease_1");
    let lease = LeaseContext {
        worker_instance_id: "worker-instance-1".to_owned(),
        fencing_token: "0".to_owned(),
    };
    let authority = FakeAuthority {
        valid: false,
        error: AuthorityError::StaleFencingToken,
    };
    let mut store = FakeStore::default();

    let error = machine
        .accept(
            &mut store,
            &authority,
            &stream,
            &lease,
            &frame(1, "evt_1", "digest-1"),
        )
        .expect_err("stale fencing is rejected by caller authority");

    assert_eq!(
        error,
        ReplayError::Authority(AuthorityError::StaleFencingToken)
    );
    assert_eq!(store.writes, 0);
    assert_eq!(store.loads, 0, "authority rejection precedes state reads");
}

#[test]
fn generic_replay_contract_covers_runtime_artifact_and_model_stream_labels() {
    for stream_label in ["runtime.event", "artifact.chunk", "model.chunk"] {
        assert_generic_replay_contract(stream_label);
    }
}

fn assert_generic_replay_contract(stream_label: &str) {
    let machine = ReplayStateMachine::new();
    let stream = ReplayStreamKey::new(format!("{stream_label}:job_1:lease_1"));
    let lease = LeaseContext {
        worker_instance_id: "worker-instance-1".to_owned(),
        fencing_token: "1".to_owned(),
    };
    let authority = FakeAuthority {
        valid: true,
        error: AuthorityError::ExpiredLease,
    };
    let first = frame(1, &format!("{stream_label}:event_1"), "digest-1");
    let second = frame(2, &format!("{stream_label}:event_2"), "digest-2");

    assert_generic_decisions(
        machine,
        &stream,
        &lease,
        authority,
        stream_label,
        &first,
        &second,
    );
    assert_generic_restart(machine, &stream, &lease, authority, stream_label);
    assert_generic_lease_rejection(machine, &stream, &lease, stream_label);
}

fn assert_generic_decisions(
    machine: ReplayStateMachine,
    stream: &ReplayStreamKey,
    lease: &LeaseContext,
    authority: FakeAuthority,
    stream_label: &str,
    first: &ReplayFrame,
    second: &ReplayFrame,
) {
    let mut store = FakeStore::default();
    assert_eq!(
        machine
            .accept(&mut store, &authority, stream, lease, first)
            .expect("first frame is accepted"),
        ReplayDecision::Accepted {
            highest_sequence: 1
        }
    );
    assert_eq!(
        machine
            .accept(&mut store, &authority, stream, lease, first)
            .expect("same frame is a duplicate"),
        ReplayDecision::Duplicate {
            highest_sequence: 1,
            original: first.clone(),
        }
    );
    assert_eq!(
        machine
            .accept(
                &mut store,
                &authority,
                stream,
                lease,
                &frame_with_body(
                    1,
                    &format!("{stream_label}:event_1"),
                    "digest-changed",
                    b"changed body",
                ),
            )
            .expect("changed body is a conflict"),
        ReplayDecision::Conflict {
            highest_sequence: 1
        }
    );
    assert_eq!(
        machine
            .accept(
                &mut store,
                &authority,
                stream,
                lease,
                &frame(3, &format!("{stream_label}:event_3"), "digest-3"),
            )
            .expect("gap is a stable protocol result"),
        ReplayDecision::Gap {
            highest_sequence: 1,
            replay_from_sequence: 2,
        }
    );
    assert_eq!(store.writes, 1, "duplicate, conflict, and gap do not write");
    assert_eq!(
        machine
            .accept(&mut store, &authority, stream, lease, second)
            .expect("missing frame is accepted"),
        ReplayDecision::Accepted {
            highest_sequence: 2
        }
    );
}

fn assert_generic_restart(
    machine: ReplayStateMachine,
    stream: &ReplayStreamKey,
    lease: &LeaseContext,
    authority: FakeAuthority,
    stream_label: &str,
) {
    let path = temporary_replay_path(stream_label);
    let mut file_store = ReopenableFileStore::new(path.clone());
    for sequence in 1..=3 {
        let event = frame(
            sequence,
            &format!("{stream_label}:restart_event_{sequence}"),
            &format!("restart-digest-{sequence}"),
        );
        machine
            .accept(&mut file_store, &authority, stream, lease, &event)
            .expect("frame is durably accepted before restart");
    }
    let mut reopened_store = file_store.reopen();
    let replay = machine
        .resume(&mut reopened_store, &authority, stream, lease, 1, 10)
        .expect("reopened store resumes the stream");
    assert_eq!(replay.highest_sequence, 3);
    assert_eq!(
        replay
            .events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert_eq!(
        replay.events,
        vec![
            frame(
                2,
                &format!("{stream_label}:restart_event_2"),
                "restart-digest-2",
            ),
            frame(
                3,
                &format!("{stream_label}:restart_event_3"),
                "restart-digest-3",
            ),
        ]
    );
    assert_eq!(
        reopened_store.writes, 0,
        "resume is read-only after restart"
    );
    assert_eq!(
        machine
            .accept(
                &mut reopened_store,
                &authority,
                stream,
                lease,
                &frame(
                    4,
                    &format!("{stream_label}:restart_event_4"),
                    "restart-digest-4",
                ),
            )
            .expect("reopened store continues the durable stream"),
        ReplayDecision::Accepted {
            highest_sequence: 4
        }
    );
    drop(reopened_store);
    drop(file_store);
    let _ = std::fs::remove_file(path);
}

fn assert_generic_lease_rejection(
    machine: ReplayStateMachine,
    stream: &ReplayStreamKey,
    lease: &LeaseContext,
    stream_label: &str,
) {
    let expired_authority = FakeAuthority {
        valid: false,
        error: AuthorityError::ExpiredLease,
    };
    let mut rejected_store = FakeStore::default();
    let error = machine
        .accept(
            &mut rejected_store,
            &expired_authority,
            stream,
            lease,
            &frame(1, &format!("{stream_label}:event_1"), "digest-1"),
        )
        .expect_err("inactive lease is rejected before replay state access");
    assert_eq!(error, ReplayError::Authority(AuthorityError::ExpiredLease));
    assert_eq!(rejected_store.loads, 0);
    assert_eq!(rejected_store.writes, 0);
}

fn frame(sequence: ReplaySequence, event_id: &str, digest: &str) -> ReplayFrame {
    frame_with_body(
        sequence,
        event_id,
        digest,
        format!("event body {sequence}").as_bytes(),
    )
}

fn frame_with_body(
    sequence: ReplaySequence,
    event_id: &str,
    digest: &str,
    body: &[u8],
) -> ReplayFrame {
    ReplayFrame {
        event_id: event_id.to_owned(),
        sequence,
        digest: digest.to_owned(),
        frame: body.to_vec(),
    }
}
