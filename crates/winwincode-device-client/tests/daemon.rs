// SPDX-License-Identifier: Apache-2.0

//! End-to-end daemon exchange-loop scenarios driven by in-memory fake
//! transports: enrollment, hello, heartbeat, acknowledgement advancement,
//! exponential backoff after network failures, gap replay, restart recovery
//! from the durable outbox, and graceful shutdown. Temporary-directory
//! infrastructure mirrors `crates/winwincode-storage/tests/sqlite.rs`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use winwincode_client_port::domain::{
    ClientArchitecture, ClientCapacityReport, ClientLockState, ClientPlatformTarget, PresenceState,
};
use winwincode_client_port::messages::{
    CLIENT_CONTROL_PORT_SCHEMA_VERSION, ClientHeartbeatPayload, ClientToServerMessage,
    ServerEnrollmentAcceptedPayload, ServerToClientEnvelope, ServerToClientMessage,
};
use winwincode_device_client::{
    DaemonConfig, DaemonError, DaemonStatus, DeviceDaemon, DeviceIdentitySeed, DeviceStore,
    ExchangeBatchStatus, ExchangeRequest, ExchangeResponse, ExchangeTransport,
    ExchangeTransportError, IdentityRecord, TickOutcome, ensure_device_identity,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-device-client-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn cleanup(root: &Path) {
    fs::remove_dir_all(root).expect("database directory should be released");
}

fn seed() -> DeviceIdentitySeed {
    DeviceIdentitySeed {
        display_name: "Cheng's MacBook".to_owned(),
        platform: "darwin".to_owned(),
        architecture: "arm64".to_owned(),
        client_version: "0.1.0-alpha.1".to_owned(),
    }
}

fn daemon_config(name: &str) -> DaemonConfig {
    DaemonConfig {
        server_profile_id: format!("server-{name}"),
        base_url: format!("https://{name}.example.test/internal/v1/client/exchange"),
        server_display_name: "WinWinCode Control Plane".to_owned(),
        device_display_name: "Cheng's MacBook".to_owned(),
        platform: ClientPlatformTarget::Aarch64AppleDarwin,
        architecture: ClientArchitecture::Aarch64,
        client_version: "0.1.0-alpha.1".to_owned(),
        heartbeat_interval: Duration::from_millis(5),
        enroll_poll_interval: Duration::from_millis(1),
        max_frames_per_exchange: 8,
        initial_backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(16),
        capacity: ClientCapacityReport {
            max_concurrent_worker_sessions: 2,
            running_worker_sessions: 0,
            reserved_worker_sessions: 0,
            draining_worker_sessions: 0,
        },
    }
}

fn open_identity(root: &Path) -> (DeviceStore, IdentityRecord) {
    let mut store = DeviceStore::open(root).expect("device store should open");
    let identity = ensure_device_identity(&mut store, &seed(), "2026-09-04T00:00:00.000Z")
        .expect("device identity should load");
    (store, identity)
}

fn heartbeat_message(capacity: &ClientCapacityReport) -> ClientToServerMessage {
    ClientToServerMessage::Heartbeat(ClientHeartbeatPayload {
        capacity: *capacity,
        accepting_connections: true,
        lock_state: ClientLockState::Unlocked,
        presence_state: PresenceState::Online,
        occupancy_lease_id: None,
    })
}

/// One client-to-server frame the fake server received.
#[derive(Clone, Debug)]
struct SimFrame {
    instance: String,
    sequence: u64,
    kind: String,
    frame: Value,
}

#[derive(Default)]
struct ServerState {
    received: Vec<SimFrame>,
    sequences: HashMap<String, BTreeSet<u64>>,
    downlink_next: HashMap<String, u64>,
    enroll_seen: HashSet<String>,
    acceptance_issued: HashSet<String>,
}

/// In-memory fake of the exchange endpoint: it records every frame,
/// acknowledges the contiguous sequence prefix, and answers the first
/// exchanges after a `client.enroll` with a `client.enrollment_accepted`
/// downlink frame.
struct ServerSim {
    state: Mutex<ServerState>,
    /// Exchanges after the enroll that stay silent before the acceptance.
    withhold_acceptance: AtomicUsize,
    /// publicClientId reported by the acceptance instead of the real one.
    acceptance_client_id_override: Mutex<Option<String>>,
    /// Exchanges that are processed but answered with a gap (a lost
    /// response), forcing the client to replay its retained batch.
    gap_responses_remaining: AtomicUsize,
    /// The `replayFromSequence` the gap responses carry.
    gap_replay_from_sequence: AtomicU64,
}

impl ServerSim {
    fn new() -> Self {
        Self {
            state: Mutex::new(ServerState::default()),
            withhold_acceptance: AtomicUsize::new(0),
            acceptance_client_id_override: Mutex::new(None),
            gap_responses_remaining: AtomicUsize::new(0),
            gap_replay_from_sequence: AtomicU64::new(1),
        }
    }

    fn all_frames(&self) -> Vec<SimFrame> {
        self.state.lock().expect("server sim lock").received.clone()
    }

    fn frames_with_kind(&self, kind: &str) -> Vec<SimFrame> {
        self.state
            .lock()
            .expect("server sim lock")
            .received
            .iter()
            .filter(|frame| frame.kind == kind)
            .cloned()
            .collect()
    }

    fn respond(
        status: ExchangeBatchStatus,
        ack: u64,
        replay: Option<u64>,
        frames: Vec<Value>,
    ) -> Result<Vec<u8>, ExchangeTransportError> {
        serde_json::to_vec(&ExchangeResponse {
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            status,
            ack_sequence: ack,
            replay_from_sequence: replay,
            frames,
        })
        .map_err(|error| ExchangeTransportError::new(format!("fake response encode: {error}")))
    }

    fn acceptance_frame(&self, node: &str, sequence: u64) -> Value {
        let public_client_id = self
            .acceptance_client_id_override
            .lock()
            .expect("override lock")
            .clone()
            .unwrap_or_else(|| node.to_owned());
        let envelope = ServerToClientEnvelope {
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            message_id: format!("srv-accept-{sequence}"),
            client_node_id: "control-plane".to_owned(),
            client_instance_id: "control-plane".to_owned(),
            sequence,
            occurred_at: "2026-09-04T00:00:00.000Z".to_owned(),
            message: ServerToClientMessage::EnrollmentAccepted(ServerEnrollmentAcceptedPayload {
                public_client_id,
                heartbeat_interval_ms: 10,
                server_time: "2026-09-04T00:00:00.000Z".to_owned(),
            }),
        };
        serde_json::to_value(&envelope).expect("acceptance value")
    }

    fn contiguous_ack(state: &ServerState, node: &str) -> u64 {
        let mut expected = 1_u64;
        while state
            .sequences
            .get(node)
            .is_some_and(|sequences| sequences.contains(&expected))
        {
            expected += 1;
        }
        expected - 1
    }
}

impl ExchangeTransport for ServerSim {
    fn exchange(&self, request_bytes: &[u8]) -> Result<Vec<u8>, ExchangeTransportError> {
        let request: ExchangeRequest = serde_json::from_slice(request_bytes).map_err(|error| {
            ExchangeTransportError::new(format!("fake request decode: {error}"))
        })?;
        let mut downlink = Vec::new();
        let ack;
        {
            let mut state = self.state.lock().expect("server sim lock");
            let node = request.client_node_id.clone();
            for frame in &request.frames {
                let kind = frame
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned();
                let sequence = frame
                    .get("sequence")
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
                state
                    .sequences
                    .entry(node.clone())
                    .or_default()
                    .insert(sequence);
                state.received.push(SimFrame {
                    instance: request.client_instance_id.clone(),
                    sequence,
                    kind: kind.clone(),
                    frame: frame.clone(),
                });
                if kind == "client.enroll" {
                    state.enroll_seen.insert(node.clone());
                }
            }
            // The acceptance rides any exchange once the enroll was seen; the
            // withhold counter keeps the waiting period going.
            if state.enroll_seen.contains(&node)
                && !state.acceptance_issued.contains(&node)
                && self
                    .withhold_acceptance
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                        value.checked_sub(1)
                    })
                    .is_err()
            {
                let next = state.downlink_next.entry(node.clone()).or_insert(1);
                let sequence = *next;
                *next += 1;
                downlink.push(self.acceptance_frame(&node, sequence));
                state.acceptance_issued.insert(node.clone());
            }
            ack = Self::contiguous_ack(&state, &node);
        }
        // A configured lost response: the batch was processed, but the client
        // receives a gap and must replay from the hint.
        if self
            .gap_responses_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                value.checked_sub(1)
            })
            .is_ok()
        {
            return Self::respond(
                ExchangeBatchStatus::Gap,
                0,
                Some(self.gap_replay_from_sequence.load(Ordering::SeqCst)),
                downlink,
            );
        }
        Self::respond(ExchangeBatchStatus::Accepted, ack, None, downlink)
    }
}

/// Fake transport whose first calls fail like a network outage before
/// delegating to the endpoint fake.
struct FlakyTransport {
    inner: Arc<ServerSim>,
    failures_remaining: AtomicUsize,
}

impl ExchangeTransport for FlakyTransport {
    fn exchange(&self, request_bytes: &[u8]) -> Result<Vec<u8>, ExchangeTransportError> {
        if self.failures_remaining.load(Ordering::SeqCst) > 0 {
            self.failures_remaining.fetch_sub(1, Ordering::SeqCst);
            return Err(ExchangeTransportError::new("connection refused"));
        }
        self.inner.exchange(request_bytes)
    }
}

/// Fake transport whose first exchange blocks until the test releases it and
/// then completes against the endpoint fake: the frame is "taken" (in
/// flight) while the test shuts the daemon down.
struct BlockingTransport {
    inner: Arc<ServerSim>,
    started: Arc<(Mutex<bool>, Condvar)>,
    release: Arc<(Mutex<bool>, Condvar)>,
    first_exchange_done: AtomicBool,
}

impl ExchangeTransport for BlockingTransport {
    fn exchange(&self, request_bytes: &[u8]) -> Result<Vec<u8>, ExchangeTransportError> {
        let blocked = !self.first_exchange_done.swap(true, Ordering::SeqCst);
        if blocked {
            {
                let (mutex, condvar) = &*self.started;
                let mut flag = mutex.lock().expect("started lock");
                *flag = true;
                condvar.notify_all();
            }
            let (mutex, condvar) = &*self.release;
            let mut flag = mutex.lock().expect("release lock");
            while !*flag {
                flag = condvar.wait(flag).expect("release wait");
            }
        }
        self.inner.exchange(request_bytes)
    }
}

const DRIVE_SLEEP_CAP: Duration = Duration::from_millis(10);

/// Drives the loop deterministically: at most `max_ticks` exchanges or
/// waits, sleeping only the durations the loop itself schedules.
fn drive(daemon: &mut DeviceDaemon, shutdown: &AtomicBool, max_ticks: usize) {
    for _ in 0..max_ticks {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        match daemon.tick(Instant::now()) {
            Ok(outcome @ (TickOutcome::Waiting { .. } | TickOutcome::Retrying { .. })) => {
                let ready_in = match outcome {
                    TickOutcome::Waiting { ready_in } => ready_in,
                    TickOutcome::Retrying { after, .. } => after,
                    _ => unreachable!("matched above"),
                };
                thread::sleep(ready_in.min(DRIVE_SLEEP_CAP).max(Duration::from_millis(1)));
            }
            Ok(TickOutcome::Reacquiring { reannounce_in }) => {
                thread::sleep(
                    reannounce_in
                        .min(DRIVE_SLEEP_CAP)
                        .max(Duration::from_millis(1)),
                );
            }
            Ok(TickOutcome::Exchanged { .. }) => thread::sleep(Duration::from_millis(2)),
            Err(DaemonError::IdentityMismatch { .. }) => return,
            Err(error) => panic!("daemon tick failed fatally: {error:?}"),
        }
    }
}

fn assert_fully_exchanged(daemon: &mut DeviceDaemon) {
    let snapshot = daemon.outbox_snapshot().expect("outbox snapshot");
    assert!(
        snapshot.frames.is_empty(),
        "a fully exchanged session must have no retained frames: {snapshot:?}"
    );
    assert_eq!(snapshot.ack_sequence, snapshot.highest_sequence);
    let status = daemon.status();
    assert_eq!(status.consecutive_failures, 0);
    assert_eq!(status.current_backoff, Duration::ZERO);
}

#[test]
fn enroll_hello_heartbeat_and_ack_advance_end_to_end() {
    let root = temporary_directory("daemon-exchange");
    let sim = Arc::new(ServerSim::new());
    let config = daemon_config("exchange");
    let (store, identity) = open_identity(&root);
    let mut daemon =
        DeviceDaemon::start(config.clone(), store, sim.clone(), &identity).expect("daemon start");

    drive(&mut daemon, &AtomicBool::new(false), 60);

    let status: DaemonStatus = daemon.status().clone();
    assert!(daemon.is_enrolled(), "the enrollment must be accepted");
    assert!(status.enrolled);
    assert!(
        status.heartbeats_enqueued >= 2,
        "the heartbeat cadence must fire: {status:?}"
    );

    // Wire order: enroll first, hello after acceptance, then heartbeats.
    let enroll_position = sim
        .frames_with_kind("client.enroll")
        .first()
        .expect("the enroll must be sent")
        .sequence;
    assert_eq!(enroll_position, 1);
    let all_frames = sim.all_frames();
    let hello_position = all_frames
        .iter()
        .position(|frame| frame.kind == "client.hello")
        .expect("hello must be sent");
    assert!(
        all_frames[..hello_position]
            .iter()
            .all(|frame| frame.kind != "client.heartbeat"),
        "hello precedes the heartbeat cadence: {all_frames:?}"
    );
    let heartbeats = sim.frames_with_kind("client.heartbeat");
    assert!(heartbeats.len() >= 2, "{all_frames:?}");
    let capacity = heartbeats[0]
        .frame
        .get("payload")
        .and_then(|payload| payload.get("capacity"))
        .cloned()
        .map(serde_json::from_value::<ClientCapacityReport>)
        .and_then(Result::ok)
        .expect("heartbeat capacity report");
    assert_eq!(capacity, config.capacity, "the skeleton capacity rides");

    assert_fully_exchanged(&mut daemon);

    // Enrollment is persisted as the server profile row.
    let profile = daemon
        .store_mut()
        .server_profile(&config.server_profile_id)
        .expect("profile read")
        .expect("enrollment must persist the server profile");
    assert_eq!(profile.base_url, config.base_url);
    assert_eq!(profile.display_name, config.server_display_name);

    // The durable inbox cursor tracks the accepted downlink frame.
    let cursor = daemon
        .store_mut()
        .inbox_cursor(&config.server_profile_id)
        .expect("cursor read")
        .expect("the acceptance frame advanced the inbox cursor");
    assert_eq!(cursor.last_sequence, 1);
    assert_eq!(status.downlink_accepted_through, 1);

    daemon.into_store().close().expect("store close");
    cleanup(&root);
}

#[test]
fn network_errors_back_off_and_recover() {
    let root = temporary_directory("daemon-backoff");
    let sim = Arc::new(ServerSim::new());
    let config = daemon_config("backoff");
    let (store, identity) = open_identity(&root);
    let transport = Arc::new(FlakyTransport {
        inner: sim.clone(),
        failures_remaining: AtomicUsize::new(2),
    });
    let mut daemon =
        DeviceDaemon::start(config.clone(), store, transport, &identity).expect("daemon start");

    drive(&mut daemon, &AtomicBool::new(false), 80);

    let status = daemon.status().clone();
    assert!(
        status.exchanges_started >= status.exchanges_succeeded + 2,
        "the two network failures must show as attempts without successes: {status:?}"
    );
    assert!(
        status.last_error.is_some(),
        "the last transport failure stays visible: {status:?}"
    );
    assert_eq!(
        status.consecutive_failures, 0,
        "recovery resets the failure streak: {status:?}"
    );
    assert_eq!(status.current_backoff, Duration::ZERO);
    assert!(daemon.is_enrolled(), "{status:?}");

    // Persist-before-send: nothing was lost across the failures.
    assert_fully_exchanged(&mut daemon);
    let enroll_deliveries = sim.frames_with_kind("client.enroll");
    assert_eq!(
        enroll_deliveries.len(),
        1,
        "the failed attempts never reached the server: {enroll_deliveries:?}"
    );
    let highest = daemon.outbox_snapshot().expect("snapshot").highest_sequence;
    assert_eq!(status.acked_through, highest);

    daemon.into_store().close().expect("store close");
    cleanup(&root);
}

#[test]
fn gap_response_triggers_replay_from_the_hint() {
    let root = temporary_directory("daemon-gap");
    let sim = Arc::new(ServerSim::new());
    // The first exchange (the enroll batch) is processed but its response is
    // a gap pointing at sequence 1, so the client must replay the batch.
    sim.gap_responses_remaining.store(1, Ordering::SeqCst);
    sim.gap_replay_from_sequence.store(1, Ordering::SeqCst);
    let config = daemon_config("gap");
    let (store, identity) = open_identity(&root);
    let mut daemon =
        DeviceDaemon::start(config.clone(), store, sim.clone(), &identity).expect("daemon start");

    drive(&mut daemon, &AtomicBool::new(false), 80);

    let status = daemon.status().clone();
    assert!(
        status.replays >= 1,
        "the gap must record a replay: {status:?}"
    );
    let enroll_deliveries = sim.frames_with_kind("client.enroll");
    assert!(
        enroll_deliveries.len() >= 2,
        "the replay re-delivers the first batch: {enroll_deliveries:?} status: {status:?}"
    );
    assert_eq!(enroll_deliveries[0].sequence, 1);
    assert_eq!(enroll_deliveries[1].sequence, 1);
    assert!(daemon.is_enrolled(), "{status:?}");
    assert_fully_exchanged(&mut daemon);

    daemon.into_store().close().expect("store close");
    cleanup(&root);
}

#[test]
fn enrollment_polls_until_accepted_and_retries_with_backoff() {
    let root = temporary_directory("daemon-enroll-wait");
    let sim = Arc::new(ServerSim::new());
    sim.withhold_acceptance.store(3, Ordering::SeqCst);
    let config = daemon_config("enroll-wait");
    let (store, identity) = open_identity(&root);
    let mut daemon =
        DeviceDaemon::start(config.clone(), store, sim.clone(), &identity).expect("daemon start");

    drive(&mut daemon, &AtomicBool::new(false), 150);

    let status = daemon.status().clone();
    assert!(
        status.exchanges_started > 3,
        "the waiting period polls with exchanges: {status:?}"
    );
    assert!(daemon.is_enrolled(), "acceptance ends the wait: {status:?}");
    let enroll_deliveries = sim.frames_with_kind("client.enroll");
    assert_eq!(
        enroll_deliveries.len(),
        1,
        "the durable enroll frame is delivered once and acked, never re-enqueued: \
         {enroll_deliveries:?}"
    );
    assert_fully_exchanged(&mut daemon);

    daemon.into_store().close().expect("store close");
    cleanup(&root);
}

#[test]
fn enrollment_identity_mismatch_is_fatal() {
    let root = temporary_directory("daemon-enroll-mismatch");
    let sim = Arc::new(ServerSim::new());
    *sim.acceptance_client_id_override.lock().expect("override") = Some("9999999999".to_owned());
    let config = daemon_config("enroll-mismatch");
    let (store, identity) = open_identity(&root);
    let mut daemon =
        DeviceDaemon::start(config, store, sim.clone(), &identity).expect("daemon start");

    let mut mismatch = None;
    for _ in 0..20 {
        match daemon.tick(Instant::now()) {
            Ok(
                TickOutcome::Waiting { ready_in }
                | TickOutcome::Retrying {
                    after: ready_in, ..
                },
            ) => {
                thread::sleep(ready_in.min(DRIVE_SLEEP_CAP).max(Duration::from_millis(1)));
            }
            Ok(TickOutcome::Reacquiring { reannounce_in }) => {
                thread::sleep(
                    reannounce_in
                        .min(DRIVE_SLEEP_CAP)
                        .max(Duration::from_millis(1)),
                );
            }
            Ok(TickOutcome::Exchanged { .. }) => thread::sleep(Duration::from_millis(2)),
            Err(DaemonError::IdentityMismatch { local, reported }) => {
                mismatch = Some((local, reported));
                break;
            }
            Err(other) => panic!("unexpected fatal error: {other:?}"),
        }
    }
    let (local, reported) = mismatch.expect("the foreign acceptance must fail the daemon");
    assert_eq!(local, daemon.client_node_id());
    assert_eq!(reported, "9999999999");
    assert!(!daemon.is_enrolled());

    daemon.into_store().close().expect("store close");
    cleanup(&root);
}

#[test]
fn restart_recovers_unacked_frames_from_the_persistent_outbox() {
    let root = temporary_directory("daemon-restart");
    let sim = Arc::new(ServerSim::new());
    let config = daemon_config("restart");
    let (store, first_identity) = open_identity(&root);
    let mut daemon = DeviceDaemon::start(config.clone(), store, sim.clone(), &first_identity)
        .expect("daemon start");

    // Enqueue three frames but never exchange them: a crash-shaped state.
    for _ in 0..3 {
        daemon
            .enqueue(heartbeat_message(&config.capacity))
            .expect("enqueue before crash");
    }
    let snapshot = daemon.outbox_snapshot().expect("snapshot");
    assert_eq!(snapshot.frames.len(), 3);
    assert_eq!(snapshot.highest_sequence, 3);
    daemon.into_store().close().expect("crash close");

    // Restart: a new process launch rotates clientInstanceId, while the
    // device identity stays stable.
    let (store, second_identity) = {
        let mut store = DeviceStore::open(&root).expect("restarted store");
        let identity = ensure_device_identity(&mut store, &seed(), "2026-09-04T01:00:00.000Z")
            .expect("restarted identity");
        (store, identity)
    };
    assert_eq!(
        second_identity.identity().device_id(),
        first_identity.identity().device_id(),
        "device_id must be stable across restarts"
    );
    assert_eq!(
        second_identity.identity().public_client_id(),
        first_identity.identity().public_client_id(),
        "publicClientId must be stable across restarts"
    );
    assert_ne!(
        second_identity.current_instance_id(),
        first_identity.current_instance_id(),
        "each launch rotates clientInstanceId"
    );

    let mut daemon = DeviceDaemon::start(config.clone(), store, sim.clone(), &second_identity)
        .expect("restarted daemon");

    // The pending frames of the superseded instance were re-issued under the
    // current launch instance with their stream sequences kept: nothing was
    // lost and the node stream stays contiguous.
    assert_eq!(
        daemon.client_instance_id(),
        second_identity.current_instance_id()
    );
    let snapshot = daemon.outbox_snapshot().expect("migrated snapshot");
    assert_eq!(snapshot.frames.len(), 3, "{snapshot:?}");
    assert_eq!(
        snapshot
            .frames
            .iter()
            .map(|frame| frame.sequence)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(snapshot.highest_sequence, 3);
    assert_eq!(snapshot.ack_sequence, 0);
    let pending = daemon
        .store_mut()
        .pending_outbox_envelopes()
        .expect("pending rows");
    assert_eq!(pending.len(), 3);
    assert!(
        pending
            .iter()
            .all(|entry| entry.client_instance_id == second_identity.current_instance_id()),
        "migrated frames carry the current launch instance: {pending:?}"
    );

    drive(&mut daemon, &AtomicBool::new(false), 80);

    let status = daemon.status().clone();
    assert!(daemon.is_enrolled(), "{status:?}");
    assert_fully_exchanged(&mut daemon);
    let heartbeats = sim.frames_with_kind("client.heartbeat");
    assert!(
        heartbeats.len() >= 3,
        "the three migrated frames must reach the server: {}",
        heartbeats.len()
    );
    let migrated = &heartbeats[..3];
    assert!(
        migrated
            .iter()
            .all(|frame| frame.sequence <= 3
                && frame.instance == second_identity.current_instance_id()),
        "migrated frames replay under the current instance: {migrated:?}"
    );

    daemon.into_store().close().expect("restarted store close");
    cleanup(&root);
}

#[test]
fn graceful_shutdown_keeps_taken_frames_durable() {
    let root = temporary_directory("daemon-shutdown");
    let sim = Arc::new(ServerSim::new());
    let config = daemon_config("shutdown");
    let (store, identity) = open_identity(&root);
    let started = Arc::new((Mutex::new(false), Condvar::new()));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let transport = Arc::new(BlockingTransport {
        inner: sim.clone(),
        started: started.clone(),
        release: release.clone(),
        first_exchange_done: AtomicBool::new(false),
    });
    let mut daemon = DeviceDaemon::start(
        DaemonConfig {
            max_frames_per_exchange: 1,
            ..config.clone()
        },
        store,
        transport,
        &identity,
    )
    .expect("daemon start");
    for _ in 0..3 {
        daemon
            .enqueue(heartbeat_message(&config.capacity))
            .expect("enqueue");
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    thread::scope(|scope| {
        let handle = scope.spawn(|| daemon.run(&shutdown));

        // Wait until the first frame is taken by the transport...
        {
            let (mutex, condvar) = &*started;
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut flag = mutex.lock().expect("started lock");
            while !*flag {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(!remaining.is_zero(), "the exchange never started");
                let (next, timeout) = condvar.wait_timeout(flag, remaining).expect("started wait");
                assert!(!timeout.timed_out(), "the exchange never started");
                flag = next;
            }
        }
        // ...then shut the daemon down while the frame is in flight.
        shutdown.store(true, Ordering::Relaxed);
        {
            let (mutex, condvar) = &*release;
            let mut flag = mutex.lock().expect("release lock");
            *flag = true;
            condvar.notify_all();
        }

        let status = handle.join().expect("run thread joins");
        assert!(status.is_ok(), "graceful shutdown must not fail");
    });

    // The in-flight frame (the enroll, sequence 1) was acknowledged and
    // confirmed; the frames the loop never reached stay durable exactly as
    // they were taken.
    let snapshot = daemon.outbox_snapshot().expect("post-shutdown snapshot");
    assert_eq!(snapshot.ack_sequence, 1);
    assert_eq!(snapshot.highest_sequence, 4);
    assert_eq!(
        snapshot
            .frames
            .iter()
            .map(|frame| frame.sequence)
            .collect::<Vec<_>>(),
        [2, 3, 4],
        "unconfirmed frames survive shutdown untouched"
    );

    // A resumed daemon delivers exactly the surviving frames.
    let resumed_shutdown = AtomicBool::new(false);
    drive(&mut daemon, &resumed_shutdown, 80);
    assert_fully_exchanged(&mut daemon);

    daemon.into_store().close().expect("store close");
    cleanup(&root);
}
