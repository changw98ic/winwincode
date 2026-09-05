// SPDX-License-Identifier: Apache-2.0

//! End-to-end daemon exchange-loop scenarios driven by in-memory fake
//! transports speaking the endpoint's canonical wire contract: enrollment
//! adoption, hello, heartbeat, acknowledgement advancement, exponential
//! backoff after network failures, gap replay, restart recovery from the
//! durable outbox, and graceful shutdown. Temporary-directory
//! infrastructure mirrors `crates/winwincode-storage/tests/sqlite.rs`.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};
use winwincode_client_port::domain::{
    ClientArchitecture, ClientCapacityReport, ClientLockState, ClientPlatformTarget, PresenceState,
};
use winwincode_client_port::messages::{
    CLIENT_CONTROL_PORT_SCHEMA_VERSION, ClientHeartbeatPayload, ClientToServerMessage,
    ServerEnrollmentAcceptedPayload, ServerToClientEnvelope, ServerToClientMessage,
};
use winwincode_device_client::{
    DaemonConfig, DaemonError, DaemonStatus, DeviceDaemon, DeviceIdentitySeed, DeviceStore,
    EnrollmentIssuance, ExchangeRequest, ExchangeResponse, ExchangeTransport,
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

/// A configuration whose heartbeat cadence stays quiet for the whole test:
/// scenarios that stage exact crash shapes around the enroll/hello exchange
/// keep the stream deterministic.
fn quiet_daemon_config(name: &str) -> DaemonConfig {
    DaemonConfig {
        heartbeat_interval: Duration::from_hours(12),
        ..daemon_config(name)
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

/// The canonical server-issued enrollment identity the fake endpoint hands
/// out (`cnd_` + 26 Crockford, 10 public digits, one 32-byte credential).
const ASSIGNED_NODE: &str = "cnd_A1A1A1A1A1A1A1A1A1A1A1A1A1";
const ASSIGNED_PUBLIC_CLIENT_ID: &str = "0123456789";
const ISSUED_SECRET: [u8; 32] = [0xab; 32];

fn issued_credential_hex() -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(ISSUED_SECRET.len() * 2);
    for byte in ISSUED_SECRET {
        encoded.push(HEX[usize::from(byte >> 4)] as char);
        encoded.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

fn issued_credential_digest() -> String {
    format!("sha256:{:x}", Sha256::digest(ISSUED_SECRET))
}

fn issued_enrollment() -> EnrollmentIssuance {
    issued_enrollment_with_heartbeat(10)
}

fn issued_enrollment_with_heartbeat(heartbeat_interval_ms: u32) -> EnrollmentIssuance {
    EnrollmentIssuance {
        client_node_id: ASSIGNED_NODE.to_owned(),
        public_client_id: ASSIGNED_PUBLIC_CLIENT_ID.to_owned(),
        device_credential: issued_credential_hex(),
        device_credential_digest: issued_credential_digest(),
        heartbeat_interval_ms,
        server_time: "2026-09-04T00:00:00.000Z".to_owned(),
        downlink_from_sequence: 1,
    }
}

/// One client-to-server frame the fake server received.
#[derive(Clone, Debug)]
struct SimFrame {
    node: String,
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
    enroll_node: Option<String>,
    assigned: bool,
}

/// In-memory fake of the canonical exchange endpoint: it records every
/// frame keyed by the frame envelope's `clientNodeId`, acknowledges the
/// contiguous sequence prefix, and answers the first `client.enroll`
/// exchange with the assigned identity, the acceptance downlink frame, and
/// the one-time credential issuance. Like the real endpoint, the assigned
/// stream is credited with the settled enroll sequence (the enrollment
/// settlement starts the assigned node's client-to-server stream at 1).
struct ServerSim {
    state: Mutex<ServerState>,
    /// The first exchange is answered with a gap instead of settling (a
    /// lost batch), forcing the client to replay its durable frames.
    gap_first_exchange: AtomicBool,
    /// The enrollment issuance names a non-canonical clientNodeId (server
    /// misbehavior the daemon must refuse fatally).
    corrupt_issuance: AtomicBool,
    /// After the enrollment settled once, every later enroll exchange is
    /// refused like the endpoint's uniform authentication rejection.
    enrollment_settled: AtomicBool,
    /// The heartbeat cadence the acceptance demands (the daemon adopts it
    /// over its configured cadence). Tests that stage exact crash shapes
    /// around the enroll/hello window raise it so the idle daemon stays
    /// silent while the window is staged.
    acceptance_heartbeat_ms: AtomicU32,
}

impl ServerSim {
    fn new() -> Self {
        Self {
            state: Mutex::new(ServerState::default()),
            gap_first_exchange: AtomicBool::new(false),
            corrupt_issuance: AtomicBool::new(false),
            enrollment_settled: AtomicBool::new(false),
            acceptance_heartbeat_ms: AtomicU32::new(10),
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

    fn acceptance_frame(node: &str, sequence: u64, heartbeat_interval_ms: u32) -> Value {
        let envelope = ServerToClientEnvelope {
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            message_id: format!("srv-accept-{sequence}"),
            client_node_id: node.to_owned(),
            client_instance_id: "srv-instance".to_owned(),
            sequence,
            occurred_at: "2026-09-04T00:00:00.000Z".to_owned(),
            message: ServerToClientMessage::EnrollmentAccepted(ServerEnrollmentAcceptedPayload {
                public_client_id: ASSIGNED_PUBLIC_CLIENT_ID.to_owned(),
                heartbeat_interval_ms,
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

    /// Settles one canonical exchange request like the endpoint.
    #[allow(clippy::too_many_lines)]
    fn settle(&self, request: &ExchangeRequest) -> Result<Vec<u8>, ExchangeTransportError> {
        let mut downlink = Vec::new();
        let mut issuance = None;
        let mut gap_response = false;
        let ack;
        {
            let mut state = self.state.lock().expect("server sim lock");
            let gap_fired = self
                .gap_first_exchange
                .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok();
            let Some(first) = request.frames.first() else {
                return Err(ExchangeTransportError::new("empty batch is invalid"));
            };
            let node = first
                .get("clientNodeId")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned();
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
                let instance = frame
                    .get("clientInstanceId")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned();
                state
                    .sequences
                    .entry(node.clone())
                    .or_default()
                    .insert(sequence);
                state.received.push(SimFrame {
                    node: node.clone(),
                    instance,
                    sequence,
                    kind: kind.clone(),
                    frame: frame.clone(),
                });
                if kind == "client.enroll" && state.enroll_node.is_none() {
                    state.enroll_node = Some(node.clone());
                }
            }
            if gap_fired {
                // The batch was processed but the answer is a gap: the
                // cursor stays and the client must replay its batch.
                gap_response = true;
                ack = 0;
            } else {
                // First enroll settles: assign the identity, credit the
                // assigned stream with the settled enroll sequence, and
                // deliver the acceptance with the one-time issuance.
                if let Some(enroll_node) = state.enroll_node.clone()
                    && !state.assigned
                {
                    if self.enrollment_settled.load(Ordering::SeqCst) {
                        return Err(ExchangeTransportError::new(
                            "device credential authentication failed",
                        ));
                    }
                    state.assigned = true;
                    let next = state
                        .downlink_next
                        .entry(ASSIGNED_NODE.to_owned())
                        .or_insert(1);
                    let sequence = *next;
                    *next += 1;
                    let heartbeat_ms = self.acceptance_heartbeat_ms.load(Ordering::SeqCst);
                    downlink.push(Self::acceptance_frame(
                        ASSIGNED_NODE,
                        sequence,
                        heartbeat_ms,
                    ));
                    issuance = Some(if self.corrupt_issuance.load(Ordering::SeqCst) {
                        EnrollmentIssuance {
                            client_node_id: enroll_node.clone(),
                            ..issued_enrollment()
                        }
                    } else {
                        issued_enrollment_with_heartbeat(heartbeat_ms)
                    });
                    // The enroll settlement starts the assigned stream at 1.
                    state
                        .sequences
                        .entry(ASSIGNED_NODE.to_owned())
                        .or_default()
                        .insert(1);
                }
                ack = Self::contiguous_ack(&state, &node);
            }
        }
        let response = ExchangeResponse {
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            ack_sequence: ack,
            replay_from_sequence: gap_response.then_some(1),
            frames: downlink,
            enrollment: issuance,
        };
        serde_json::to_vec(&response)
            .map_err(|error| ExchangeTransportError::new(format!("fake response encode: {error}")))
    }
}

impl ExchangeTransport for ServerSim {
    fn exchange(
        &self,
        _credential: Option<&str>,
        request_bytes: &[u8],
    ) -> Result<Vec<u8>, ExchangeTransportError> {
        let request: ExchangeRequest = serde_json::from_slice(request_bytes).map_err(|error| {
            ExchangeTransportError::new(format!("fake request decode: {error}"))
        })?;
        self.settle(&request)
    }
}

/// Fake transport whose first exchange is processed by the endpoint fake but
/// whose response is lost in transit — the worst enrollment case, because
/// the credential material crossed a transport that never delivered it.
struct LostFirstResponseTransport {
    inner: Arc<ServerSim>,
    first_exchange_done: AtomicBool,
}

impl ExchangeTransport for LostFirstResponseTransport {
    fn exchange(
        &self,
        credential: Option<&str>,
        request_bytes: &[u8],
    ) -> Result<Vec<u8>, ExchangeTransportError> {
        if !self.first_exchange_done.swap(true, Ordering::SeqCst) {
            let _ = self.inner.exchange(credential, request_bytes);
            return Err(ExchangeTransportError::new(
                "connection reset before the response arrived",
            ));
        }
        self.inner.exchange(credential, request_bytes)
    }
}

/// Fake transport whose first calls fail like a network outage before
/// delegating to the endpoint fake.
struct FlakyTransport {
    inner: Arc<ServerSim>,
    failures_remaining: AtomicUsize,
}

impl ExchangeTransport for FlakyTransport {
    fn exchange(
        &self,
        credential: Option<&str>,
        request_bytes: &[u8],
    ) -> Result<Vec<u8>, ExchangeTransportError> {
        if self.failures_remaining.load(Ordering::SeqCst) > 0 {
            self.failures_remaining.fetch_sub(1, Ordering::SeqCst);
            return Err(ExchangeTransportError::new("connection refused"));
        }
        self.inner.exchange(credential, request_bytes)
    }
}

/// Fake transport whose next exchange blocks once the test arms it: the
/// frame is "taken" (in flight) while the test shuts the daemon down.
struct ArmedBlockingTransport {
    inner: Arc<ServerSim>,
    armed: AtomicBool,
    started: Arc<(Mutex<bool>, std::sync::Condvar)>,
    release: Arc<(Mutex<bool>, std::sync::Condvar)>,
    blocked_once: AtomicBool,
}

impl ExchangeTransport for ArmedBlockingTransport {
    fn exchange(
        &self,
        credential: Option<&str>,
        request_bytes: &[u8],
    ) -> Result<Vec<u8>, ExchangeTransportError> {
        let blocked =
            self.armed.load(Ordering::SeqCst) && !self.blocked_once.swap(true, Ordering::SeqCst);
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
        self.inner.exchange(credential, request_bytes)
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
                    TickOutcome::Exchanged { .. } => unreachable!("matched above"),
                };
                thread::sleep(ready_in.min(DRIVE_SLEEP_CAP).max(Duration::from_millis(1)));
            }
            Ok(TickOutcome::Exchanged { .. }) => thread::sleep(Duration::from_millis(2)),
            Err(error) => panic!("daemon tick failed fatally: {error:?}"),
        }
    }
}

/// Drives the loop until the enrollment settled, the announcement hello was
/// exchanged, and every durable frame was delivered (the quiet cadence keeps
/// the stream empty afterwards).
fn drive_until_enrolled_and_settled(daemon: &mut DeviceDaemon) {
    for _ in 0..200 {
        if daemon.is_enrolled() && daemon.status().frames_sent >= 2 {
            let snapshot = daemon.outbox_snapshot().expect("outbox snapshot");
            if snapshot.frames.is_empty() && snapshot.ack_sequence == snapshot.highest_sequence {
                return;
            }
        }
        match daemon.tick(Instant::now()) {
            Ok(outcome @ (TickOutcome::Waiting { .. } | TickOutcome::Retrying { .. })) => {
                let ready_in = match outcome {
                    TickOutcome::Waiting { ready_in } => ready_in,
                    TickOutcome::Retrying { after, .. } => after,
                    TickOutcome::Exchanged { .. } => unreachable!("matched above"),
                };
                thread::sleep(ready_in.min(DRIVE_SLEEP_CAP).max(Duration::from_millis(1)));
            }
            Ok(TickOutcome::Exchanged { .. }) => thread::sleep(Duration::from_millis(2)),
            Err(error) => panic!("daemon tick failed fatally: {error:?}"),
        }
    }
    panic!("the daemon never reached the enrolled, settled state");
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
    assert!(!daemon.is_enrolled());
    assert_eq!(
        daemon.client_node_id(),
        identity.identity().device_id(),
        "the enrollment rides the local placeholder node id"
    );

    drive(&mut daemon, &AtomicBool::new(false), 60);

    let status: DaemonStatus = daemon.status().clone();
    assert!(daemon.is_enrolled(), "the enrollment must be accepted");
    assert!(status.enrolled);
    assert_eq!(
        daemon.client_node_id(),
        ASSIGNED_NODE,
        "the exchange node id is the server-assigned identity"
    );
    assert!(
        status.heartbeats_enqueued >= 2,
        "the heartbeat cadence must fire: {status:?}"
    );

    // Wire order: enroll first (sequence 1 under the placeholder node), then
    // the hello at sequence 2 under the assigned node.
    let all_frames = sim.all_frames();
    let enroll = sim
        .frames_with_kind("client.enroll")
        .first()
        .expect("the enroll must be sent")
        .clone();
    assert_eq!(enroll.sequence, 1);
    assert_eq!(enroll.node, identity.identity().device_id());
    let hello_position = all_frames
        .iter()
        .position(|frame| frame.kind == "client.hello")
        .expect("hello must be sent");
    let hello = &all_frames[hello_position];
    assert_eq!(hello.sequence, 2, "the assigned stream continues at 2");
    assert_eq!(hello.node, ASSIGNED_NODE);
    assert!(
        all_frames[..hello_position]
            .iter()
            .all(|frame| frame.kind != "client.heartbeat"),
        "hello precedes the heartbeat cadence: {all_frames:?}"
    );
    let heartbeats = sim.frames_with_kind("client.heartbeat");
    assert!(heartbeats.len() >= 2, "{all_frames:?}");
    assert!(
        heartbeats.iter().all(|frame| frame.node == ASSIGNED_NODE),
        "post-enrollment frames ride the assigned node: {heartbeats:?}"
    );
    let capacity: ClientCapacityReport = serde_json::from_value(
        heartbeats[0]
            .frame
            .get("payload")
            .and_then(|payload| payload.get("capacity"))
            .cloned()
            .expect("heartbeat capacity object"),
    )
    .expect("heartbeat capacity report");
    assert_eq!(capacity, config.capacity, "the skeleton capacity rides");

    assert_fully_exchanged(&mut daemon);

    // The enrollment adoption persisted the server profile.
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
    // The first exchange (the enroll batch) is lost: the response is a gap
    // pointing at sequence 1, so the client must replay the durable batch.
    sim.gap_first_exchange.store(true, Ordering::SeqCst);
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
fn a_lost_enrollment_response_never_reenrolls_and_keeps_backing_off() {
    let root = temporary_directory("daemon-enroll-lost");
    let sim = Arc::new(ServerSim::new());
    // The first enroll exchange settles server-side (the identity was
    // issued) but its response — carrying the one-time credential material —
    // is lost in transit. Every retry is then refused like the endpoint's
    // uniform rejection, because a node with a credential never re-enrolls.
    sim.enrollment_settled.store(true, Ordering::SeqCst);
    let config = daemon_config("enroll-lost");
    let (store, identity) = open_identity(&root);
    let transport = Arc::new(LostFirstResponseTransport {
        inner: sim.clone(),
        first_exchange_done: AtomicBool::new(false),
    });
    let mut daemon =
        DeviceDaemon::start(config.clone(), store, transport, &identity).expect("daemon start");

    drive(&mut daemon, &AtomicBool::new(false), 60);

    let status = daemon.status().clone();
    assert!(
        !daemon.is_enrolled(),
        "the adoption cannot complete: {status:?}"
    );
    assert!(!status.enrolled, "the adoption cannot complete: {status:?}");
    assert!(
        status.consecutive_failures >= 2,
        "the refusals back off: {status:?}"
    );
    assert!(status.last_error.is_some(), "{status:?}");
    // The durable enroll frame redelivers; no second enroll is ever
    // enqueued, and the identity stays unadopted.
    let enroll_deliveries = sim.frames_with_kind("client.enroll");
    assert!(
        enroll_deliveries.len() >= 2,
        "the durable enroll redelivers: {enroll_deliveries:?}"
    );
    assert!(
        enroll_deliveries.iter().all(|frame| frame.sequence == 1),
        "every delivery is the same durable frame: {enroll_deliveries:?}"
    );
    let pending = daemon
        .store_mut()
        .pending_outbox_envelopes()
        .expect("pending rows");
    assert_eq!(
        pending
            .iter()
            .filter(|entry| entry.kind == "client.enroll")
            .count(),
        1,
        "exactly one durable enroll row exists: {pending:?}"
    );
    let reloaded = {
        let mut store = DeviceStore::open(&root).expect("store reopen");
        let record = ensure_device_identity(&mut store, &seed(), "2026-09-04T00:01:00.000Z")
            .expect("identity reload");
        store.close().expect("close");
        record
    };
    assert_eq!(reloaded.identity().client_node_id(), "");

    daemon.into_store().close().expect("store close");
    cleanup(&root);
}

#[test]
fn a_corrupt_enrollment_issuance_is_fatal() {
    let root = temporary_directory("daemon-enroll-corrupt");
    let sim = Arc::new(ServerSim::new());
    sim.corrupt_issuance.store(true, Ordering::SeqCst);
    let config = daemon_config("enroll-corrupt");
    let (store, identity) = open_identity(&root);
    let mut daemon =
        DeviceDaemon::start(config.clone(), store, sim.clone(), &identity).expect("daemon start");

    let mut failure = None;
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
            Ok(TickOutcome::Exchanged { .. }) => thread::sleep(Duration::from_millis(2)),
            Err(error @ DaemonError::Protocol(_)) => {
                failure = Some(error);
                break;
            }
            Err(other) => panic!("unexpected fatal error: {other:?}"),
        }
    }
    let error = failure.expect("the corrupt issuance must fail the daemon fatally");
    let message = error.to_string();
    assert!(
        message.contains("issuance") || message.contains("canonical"),
        "the failure names the enrollment identity problem: {message}"
    );
    assert!(!daemon.is_enrolled());

    daemon.into_store().close().expect("store close");
    cleanup(&root);
}

#[test]
#[allow(clippy::too_many_lines)]
fn restart_recovers_unacked_frames_from_the_persistent_outbox() {
    let root = temporary_directory("daemon-restart");
    let sim = Arc::new(ServerSim::new());
    let config = quiet_daemon_config("restart");
    let (store, first_identity) = open_identity(&root);
    let mut daemon = DeviceDaemon::start(config.clone(), store, sim.clone(), &first_identity)
        .expect("daemon start");

    // Enroll and announce hello; the quiet cadence keeps the stream settled.
    drive_until_enrolled_and_settled(&mut daemon);
    assert_eq!(daemon.client_node_id(), ASSIGNED_NODE);

    // Enqueue three frames that are never exchanged: a crash-shaped state.
    for _ in 0..3 {
        daemon
            .enqueue(heartbeat_message(&config.capacity))
            .expect("enqueue before crash");
    }
    let snapshot = daemon.outbox_snapshot().expect("snapshot");
    assert_eq!(
        snapshot
            .frames
            .iter()
            .map(|frame| frame.sequence)
            .collect::<Vec<_>>(),
        [3, 4, 5]
    );
    daemon.into_store().close().expect("crash close");

    // Restart: a new process launch rotates clientInstanceId, while the
    // enrolled identity stays stable.
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
        second_identity.identity().client_node_id(),
        ASSIGNED_NODE,
        "the adopted clientNodeId must be stable across restarts"
    );
    assert_eq!(
        second_identity.identity().public_client_id(),
        ASSIGNED_PUBLIC_CLIENT_ID,
        "the adopted publicClientId must be stable across restarts"
    );
    assert_eq!(
        second_identity.credential().digest(),
        issued_credential_digest(),
        "the issued credential must survive restarts"
    );
    assert_ne!(
        second_identity.current_instance_id(),
        first_identity.current_instance_id(),
        "each launch rotates clientInstanceId"
    );

    let mut daemon = DeviceDaemon::start(config.clone(), store, sim.clone(), &second_identity)
        .expect("restarted daemon");
    assert_eq!(
        daemon.client_instance_id(),
        second_identity.current_instance_id()
    );
    assert!(
        daemon.is_enrolled(),
        "the enrolled phase restores from the durable identity without any exchange"
    );

    // The pending frames of the superseded instance keep their stream
    // sequences and their original launch instance: the server still
    // accepts them as the current instance, and this launch's announcement
    // hello takes the instance over right behind them.
    let pending = daemon
        .store_mut()
        .pending_outbox_envelopes()
        .expect("pending rows");
    assert_eq!(pending.len(), 3);
    assert!(
        pending
            .iter()
            .all(|entry| entry.client_instance_id == first_identity.current_instance_id()),
        "pending frames keep their original instance: {pending:?}"
    );

    drive(&mut daemon, &AtomicBool::new(false), 80);

    assert_fully_exchanged(&mut daemon);
    let all_frames = sim.all_frames();
    let heartbeats = sim.frames_with_kind("client.heartbeat");
    let replayed = &heartbeats[..3];
    assert!(
        replayed
            .iter()
            .all(|frame| (3..=5).contains(&frame.sequence)
                && frame.instance == first_identity.current_instance_id()),
        "the three durable frames reach the server under their original instance: \
         {replayed:?}"
    );
    let hello = all_frames
        .iter()
        .rfind(|frame| frame.kind == "client.hello")
        .expect("the restarted daemon announces hello");
    assert_eq!(
        hello.instance,
        second_identity.current_instance_id(),
        "the announcement hello carries the new launch instance"
    );
    assert_eq!(
        hello.sequence,
        replayed
            .iter()
            .map(|frame| frame.sequence)
            .max()
            .expect("sequences")
            + 1,
        "the hello continues the stream contiguously"
    );
    let enroll_deliveries = sim.frames_with_kind("client.enroll");
    assert_eq!(
        enroll_deliveries.len(),
        1,
        "a restarted enrolled daemon never re-enrolls: {enroll_deliveries:?}"
    );

    daemon.into_store().close().expect("restarted store close");
    cleanup(&root);
}

#[test]
fn graceful_shutdown_keeps_taken_frames_durable() {
    let root = temporary_directory("daemon-shutdown");
    let sim = Arc::new(ServerSim::new());
    // The staged shutdown window must stay quiet: the demanded heartbeat
    // cadence rises above the window so the idle daemon never injects a
    // heartbeat frame between the test's three enqueues and the armed
    // exchange.
    sim.acceptance_heartbeat_ms
        .store(3_600_000, Ordering::SeqCst);
    // One frame per exchange: the armed transport blocks exactly the first
    // in-flight frame of the shutdown window.
    let config = DaemonConfig {
        max_frames_per_exchange: 1,
        ..quiet_daemon_config("shutdown")
    };
    let (store, identity) = open_identity(&root);
    let started = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let transport = Arc::new(ArmedBlockingTransport {
        inner: sim.clone(),
        armed: AtomicBool::new(false),
        started: started.clone(),
        release: release.clone(),
        blocked_once: AtomicBool::new(false),
    });
    let mut daemon = DeviceDaemon::start(config.clone(), store, transport.clone(), &identity)
        .expect("daemon start");

    // Enroll and announce hello; the stream is settled and empty.
    drive_until_enrolled_and_settled(&mut daemon);
    assert_eq!(daemon.client_node_id(), ASSIGNED_NODE);

    // Enqueue three frames, then arm the transport and run the loop: the
    // first frame is taken (in flight) while the test shuts the daemon down.
    for _ in 0..3 {
        daemon
            .enqueue(heartbeat_message(&config.capacity))
            .expect("enqueue");
    }
    transport.armed.store(true, Ordering::SeqCst);

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

    // The in-flight frame (sequence 3) was acknowledged and confirmed; the
    // frames the loop never reached stay durable exactly as they were taken.
    let snapshot = daemon.outbox_snapshot().expect("post-shutdown snapshot");
    assert_eq!(snapshot.ack_sequence, 3);
    assert_eq!(snapshot.highest_sequence, 5);
    assert_eq!(
        snapshot
            .frames
            .iter()
            .map(|frame| frame.sequence)
            .collect::<Vec<_>>(),
        [4, 5],
        "unconfirmed frames survive shutdown untouched"
    );

    // A resumed daemon delivers exactly the surviving frames.
    let resumed_shutdown = AtomicBool::new(false);
    drive(&mut daemon, &resumed_shutdown, 80);
    assert_fully_exchanged(&mut daemon);

    daemon.into_store().close().expect("store close");
    cleanup(&root);
}
