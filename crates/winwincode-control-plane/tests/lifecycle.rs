use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use winwincode_control_plane::{
    CommitError, CommitReceipt, ControlPlane, ControlPlaneConfig, EventPublishError,
    EventPublisher, NewOutboxEvent, OutboxEvent, ProductStateStorage, ShutdownError,
    ShutdownReport, StartError, StateCommit, StorageError, StoredState,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct RecordingPublisher {
    trace: Arc<Mutex<Vec<String>>>,
    sent_events: Arc<Mutex<Vec<OutboxEvent>>>,
    fail_publish: bool,
    closed: Arc<AtomicBool>,
}

impl RecordingPublisher {
    fn successful() -> (Self, Arc<Mutex<Vec<OutboxEvent>>>) {
        let publisher = Self::default();
        let sent_events = Arc::clone(&publisher.sent_events);
        (publisher, sent_events)
    }

    fn failing() -> (Self, Arc<AtomicBool>) {
        let publisher = Self {
            fail_publish: true,
            ..Self::default()
        };
        let closed = Arc::clone(&publisher.closed);
        (publisher, closed)
    }
}

impl EventPublisher for RecordingPublisher {
    fn publish(&mut self, event: &OutboxEvent) -> Result<(), EventPublishError> {
        self.trace
            .lock()
            .expect("trace lock")
            .push(format!("publisher.publish:{}", event.event_id));
        if self.fail_publish {
            return Err(EventPublishError::new("publisher is unavailable"));
        }
        self.sent_events
            .lock()
            .expect("published event lock")
            .push(event.clone());
        Ok(())
    }

    fn close(&mut self) -> Result<(), EventPublishError> {
        self.trace
            .lock()
            .expect("trace lock")
            .push("publisher.close".to_owned());
        self.closed.store(true, Ordering::Release);
        Ok(())
    }
}

struct TraceStorage {
    trace: Arc<Mutex<Vec<String>>>,
    state: Mutex<Option<StoredState>>,
    committed_events: Mutex<Vec<OutboxEvent>>,
    pending_schedule: Mutex<VecDeque<Vec<OutboxEvent>>>,
    owned_directory: Option<PathBuf>,
    closed: Arc<AtomicBool>,
}

impl TraceStorage {
    fn new(
        trace: Arc<Mutex<Vec<String>>>,
        pending_schedule: Vec<Vec<OutboxEvent>>,
        owned_directory: Option<PathBuf>,
    ) -> (Self, Arc<AtomicBool>) {
        let closed = Arc::new(AtomicBool::new(false));
        (
            Self {
                trace,
                state: Mutex::new(None),
                committed_events: Mutex::new(Vec::new()),
                pending_schedule: Mutex::new(pending_schedule.into()),
                owned_directory,
                closed: Arc::clone(&closed),
            },
            closed,
        )
    }
}

impl ProductStateStorage for TraceStorage {
    fn commit(&mut self, commit: &StateCommit) -> Result<CommitReceipt, StorageError> {
        self.trace
            .lock()
            .expect("trace lock")
            .push("storage.commit".to_owned());
        let revision = commit.expected_revision + 1;
        *self.state.lock().expect("state lock") = Some(StoredState {
            stream_id: commit.stream_id.clone(),
            revision,
            payload: commit.state.clone(),
        });
        *self.committed_events.lock().expect("event lock") = commit
            .events
            .iter()
            .enumerate()
            .map(|(index, event)| OutboxEvent {
                sequence: u64::try_from(index + 1).expect("test event sequence"),
                event_id: event.event_id.clone(),
                topic: event.topic.clone(),
                payload: event.payload.clone(),
            })
            .collect();
        Ok(CommitReceipt {
            request_id: commit.request_id.clone(),
            stream_id: commit.stream_id.clone(),
            revision,
            event_ids: commit
                .events
                .iter()
                .map(|event| event.event_id.clone())
                .collect(),
            idempotent_replay: false,
        })
    }

    fn load_state(&self, _stream_id: &str) -> Result<Option<StoredState>, StorageError> {
        Ok(self.state.lock().expect("state lock").clone())
    }

    fn pending_events(&self) -> Result<Vec<OutboxEvent>, StorageError> {
        self.trace
            .lock()
            .expect("trace lock")
            .push("storage.pending".to_owned());
        if let Some(events) = self
            .pending_schedule
            .lock()
            .expect("pending schedule lock")
            .pop_front()
        {
            return Ok(events);
        }
        Ok(self.committed_events.lock().expect("event lock").clone())
    }

    fn mark_published(&mut self, event_id: &str) -> Result<(), StorageError> {
        self.trace
            .lock()
            .expect("trace lock")
            .push(format!("storage.mark:{event_id}"));
        self.committed_events
            .lock()
            .expect("event lock")
            .retain(|event| event.event_id != event_id);
        Ok(())
    }

    fn close(self: Box<Self>) -> Result<(), StorageError> {
        self.trace
            .lock()
            .expect("trace lock")
            .push("storage.close".to_owned());
        self.closed.store(true, Ordering::Release);
        if let Some(directory) = &self.owned_directory {
            fs::remove_dir_all(directory).map_err(|error| {
                StorageError::adapter(format!("failed to release test directory: {error}"))
            })?;
        }
        Ok(())
    }
}

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-control-plane-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn event(event_id: &str) -> NewOutboxEvent {
    NewOutboxEvent::new(event_id, "control-plane.state.changed", b"event".to_vec())
}

fn committed_event(sequence: u64, event_id: &str) -> OutboxEvent {
    OutboxEvent {
        sequence,
        event_id: event_id.to_owned(),
        topic: "control-plane.state.changed".to_owned(),
        payload: b"event".to_vec(),
    }
}

fn commit(request_id: &str, expected_revision: u64, state: &[u8], event_id: &str) -> StateCommit {
    StateCommit::new(
        request_id,
        "product-session:one",
        expected_revision,
        state.to_vec(),
        vec![event(event_id)],
    )
}

#[test]
fn startup_migrates_storage_before_the_control_plane_accepts_commits() {
    let root = temporary_directory("startup");
    let config = ControlPlaneConfig::local(&root);
    let (publisher, _) = RecordingPublisher::successful();

    let control_plane = ControlPlane::start_local(config, Box::new(publisher))
        .expect("the local Control Plane should start after applying SQLite migrations");

    assert!(root.join("control-plane.sqlite3").is_file());
    let temporary_root = control_plane.temporary_root().to_path_buf();
    assert!(
        temporary_root
            .join(".winwincode-control-plane-owner")
            .is_file(),
        "the running instance must mark its owned temporary root"
    );

    control_plane
        .shutdown()
        .expect("the local Control Plane should stop cleanly");
    assert!(!temporary_root.exists());
    fs::remove_dir_all(root).expect("shutdown should release the temporary database directory");
}

#[test]
fn commit_persists_state_and_outbox_before_publishing_the_event() {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let (storage, _) = TraceStorage::new(Arc::clone(&trace), vec![vec![]], None);
    let publisher = RecordingPublisher {
        trace: Arc::clone(&trace),
        ..RecordingPublisher::default()
    };
    let mut control_plane = ControlPlane::start(Box::new(storage), Box::new(publisher))
        .expect("the Control Plane should start");
    trace.lock().expect("trace lock").clear();

    let receipt = control_plane
        .commit(commit("request-1", 0, b"state-v1", "event-1"))
        .expect("state and event should commit");

    assert_eq!(receipt.revision, 1);
    assert_eq!(
        *trace.lock().expect("trace lock"),
        [
            "storage.commit",
            "storage.pending",
            "publisher.publish:event-1",
            "storage.mark:event-1",
        ]
    );
    control_plane.shutdown().expect("shutdown should succeed");
}

#[test]
fn failed_outbox_insert_rolls_back_the_state_write() {
    let root = temporary_directory("rollback");
    let (publisher, _) = RecordingPublisher::successful();
    let mut control_plane =
        ControlPlane::start_local(ControlPlaneConfig::local(&root), Box::new(publisher))
            .expect("the local Control Plane should start");
    control_plane
        .commit(commit("request-1", 0, b"state-v1", "duplicate-event"))
        .expect("the first commit should succeed");

    let error = control_plane
        .commit(commit("request-2", 1, b"state-v2", "duplicate-event"))
        .expect_err("the duplicate outbox event should fail the transaction");
    assert!(matches!(error, CommitError::Storage(_)));
    let state = control_plane
        .load_state("product-session:one")
        .expect("state should remain readable")
        .expect("the first state should remain");
    assert_eq!((state.revision, state.payload), (1, b"state-v1".to_vec()));

    control_plane.shutdown().expect("shutdown should succeed");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn publish_failure_keeps_committed_state_and_pending_outbox_for_restart() {
    let root = temporary_directory("restart");
    let (failing_publisher, failing_closed) = RecordingPublisher::failing();
    let mut control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(failing_publisher),
    )
    .expect("the first Control Plane should start");

    let error = control_plane
        .commit(commit("request-1", 0, b"state-v1", "event-for-restart"))
        .expect_err("publication should fail after the database commit");
    let receipt = error
        .committed_receipt()
        .expect("the error must identify the durable commit");
    assert_eq!(receipt.revision, 1);
    assert_eq!(
        control_plane
            .load_state("product-session:one")
            .expect("committed state should remain readable")
            .expect("committed state should exist")
            .payload,
        b"state-v1"
    );
    control_plane
        .shutdown()
        .expect_err("shutdown should report that publication is still pending");
    assert!(failing_closed.load(Ordering::Acquire));

    let (successful_publisher, sent_events) = RecordingPublisher::successful();
    let restarted = ControlPlane::start_local(
        ControlPlaneConfig::local(&root),
        Box::new(successful_publisher),
    )
    .expect("restart should replay the pending outbox event");
    assert_eq!(
        sent_events
            .lock()
            .expect("published event lock")
            .iter()
            .map(|event| event.event_id.as_str())
            .collect::<Vec<_>>(),
        ["event-for-restart"]
    );
    restarted.shutdown().expect("restart should stop cleanly");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn restart_replays_committed_but_unpublished_outbox_events() {
    let root = temporary_directory("sequence");
    let mut storage =
        winwincode_storage::SqliteStorage::open(&root).expect("SQLite storage should open");
    storage
        .commit(&StateCommit::new(
            "request-1",
            "stream-1",
            0,
            b"one".to_vec(),
            vec![event("event-z"), event("event-a")],
        ))
        .expect("outbox events should commit in supplied order");
    Box::new(storage)
        .close()
        .expect("direct storage should close");

    let (publisher, sent_events) = RecordingPublisher::successful();
    let control_plane =
        ControlPlane::start_local(ControlPlaneConfig::local(&root), Box::new(publisher))
            .expect("startup should replay the durable outbox");
    assert_eq!(
        sent_events
            .lock()
            .expect("published event lock")
            .iter()
            .map(|event| event.event_id.as_str())
            .collect::<Vec<_>>(),
        ["event-z", "event-a"]
    );
    control_plane.shutdown().expect("shutdown should succeed");
    fs::remove_dir_all(root).expect("database directory should be released");
}

#[test]
fn shutdown_flushes_outbox_then_closes_publisher_and_storage() {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let shutdown_event = committed_event(7, "shutdown-event");
    let (storage, _) =
        TraceStorage::new(Arc::clone(&trace), vec![vec![], vec![shutdown_event]], None);
    let publisher = RecordingPublisher {
        trace: Arc::clone(&trace),
        ..RecordingPublisher::default()
    };
    let control_plane = ControlPlane::start(Box::new(storage), Box::new(publisher))
        .expect("the Control Plane should start");
    trace.lock().expect("trace lock").clear();

    let report: ShutdownReport = control_plane.shutdown().expect("shutdown should succeed");

    assert_eq!(report.published_event_count, 1);
    assert_eq!(
        *trace.lock().expect("trace lock"),
        [
            "storage.pending",
            "publisher.publish:shutdown-event",
            "storage.mark:shutdown-event",
            "publisher.close",
            "storage.close",
        ]
    );
}

#[test]
fn shutdown_releases_the_sqlite_connection_and_temporary_directory() {
    let root = temporary_directory("release");
    let (publisher, _) = RecordingPublisher::successful();
    let control_plane =
        ControlPlane::start_local(ControlPlaneConfig::local(&root), Box::new(publisher))
            .expect("the local Control Plane should start");
    let temporary_root = control_plane.temporary_root().to_path_buf();

    control_plane.shutdown().expect("shutdown should succeed");

    assert!(!temporary_root.exists());
    fs::remove_dir_all(&root).expect("all SQLite handles should be released after shutdown");
    assert!(!root.exists());
}

#[test]
fn startup_does_not_delete_a_preexisting_temporary_root_without_a_proven_stale_lease() {
    let root = temporary_directory("preexisting-temporary-root");
    let temporary_parent = root.join("runtime");
    let preexisting_root = temporary_parent.join("instance-stale-candidate");
    fs::create_dir_all(&preexisting_root).expect("preexisting root should exist");
    fs::write(
        preexisting_root.join(".winwincode-control-plane-owner"),
        b"winwincode-control-plane\npid=1\ninstance=old\n",
    )
    .expect("preexisting marker should exist");
    let (publisher, _) = RecordingPublisher::successful();

    let control_plane = ControlPlane::start_local(
        ControlPlaneConfig::local(root.join("data")).with_temporary_parent(&temporary_parent),
        Box::new(publisher),
    )
    .expect("startup should create a separate owned root");
    let current_root = control_plane.temporary_root().to_path_buf();

    assert_ne!(current_root, preexisting_root);
    assert!(preexisting_root.exists());
    control_plane.shutdown().expect("shutdown should succeed");
    assert!(!current_root.exists());
    assert!(
        preexisting_root.exists(),
        "a PID or old-looking marker is not proof of a stale lease"
    );

    fs::remove_dir_all(root).expect("test should remove the deliberately retained root");
}

#[test]
fn failed_startup_closes_storage_and_releases_temporary_directory() {
    let root = temporary_directory("failed-startup");
    fs::create_dir_all(&root).expect("test directory should exist");
    let trace = Arc::new(Mutex::new(Vec::new()));
    let pending = committed_event(1, "startup-event");
    let (storage, storage_closed) =
        TraceStorage::new(Arc::clone(&trace), vec![vec![pending]], Some(root.clone()));
    let (mut publisher, publisher_closed) = RecordingPublisher::failing();
    publisher.trace = Arc::clone(&trace);

    let error: StartError = match ControlPlane::start(Box::new(storage), Box::new(publisher)) {
        Ok(control_plane) => {
            control_plane.shutdown().ok();
            panic!("startup should fail when durable outbox replay fails");
        }
        Err(error) => error,
    };

    assert!(error.to_string().contains("durable outbox"));
    assert!(publisher_closed.load(Ordering::Acquire));
    assert!(storage_closed.load(Ordering::Acquire));
    assert!(!root.exists());
    assert_eq!(
        *trace.lock().expect("trace lock"),
        [
            "storage.pending",
            "publisher.publish:startup-event",
            "publisher.close",
            "storage.close",
        ]
    );
}

#[test]
fn local_storage_open_failure_closes_the_event_publisher() {
    let root = temporary_directory("blocked-data-directory");
    fs::create_dir_all(&root).expect("test root should exist");
    let blocked_data_directory = root.join("not-a-directory");
    fs::write(&blocked_data_directory, b"file").expect("blocking file should exist");
    let (publisher, publisher_closed) = RecordingPublisher::failing();
    let temporary_parent = root.join("runtime");

    let result = ControlPlane::start_local(
        ControlPlaneConfig::local(&blocked_data_directory).with_temporary_parent(&temporary_parent),
        Box::new(publisher),
    );

    assert!(result.is_err());
    assert!(
        publisher_closed.load(Ordering::Acquire),
        "startup must explicitly close the event publisher when storage cannot open"
    );
    assert_eq!(
        fs::read_dir(&temporary_parent)
            .expect("temporary parent should remain readable")
            .count(),
        0,
        "failed startup must release the instance-owned temporary root"
    );
    fs::remove_dir_all(root).expect("failed startup should leave no open file handles");
}

#[test]
fn shutdown_publish_failure_still_closes_storage_and_releases_temporary_directory() {
    let root = temporary_directory("failed-shutdown");
    fs::create_dir_all(&root).expect("test directory should exist");
    let trace = Arc::new(Mutex::new(Vec::new()));
    let pending = committed_event(1, "shutdown-event");
    let (storage, storage_closed) = TraceStorage::new(
        Arc::clone(&trace),
        vec![vec![], vec![pending]],
        Some(root.clone()),
    );
    let (mut publisher, publisher_closed) = RecordingPublisher::failing();
    publisher.trace = Arc::clone(&trace);
    let control_plane = ControlPlane::start(Box::new(storage), Box::new(publisher))
        .expect("startup has no pending event and should succeed");
    let temporary_root = control_plane.temporary_root().to_path_buf();
    trace.lock().expect("trace lock").clear();

    let error: ShutdownError = control_plane
        .shutdown()
        .expect_err("shutdown should report the publish failure");

    assert!(error.to_string().contains("outbox flush failed"));
    assert!(publisher_closed.load(Ordering::Acquire));
    assert!(storage_closed.load(Ordering::Acquire));
    assert!(!root.exists());
    assert!(!temporary_root.exists());
    assert_eq!(
        *trace.lock().expect("trace lock"),
        [
            "storage.pending",
            "publisher.publish:shutdown-event",
            "publisher.close",
            "storage.close",
        ]
    );
}
