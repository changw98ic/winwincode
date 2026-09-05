// SPDX-License-Identifier: Apache-2.0

//! WORKER-200.2 coverage: the daemon's `client.worker.launch` downlink lane.
//! A real `SessionSupervisor` (long-lived worker test double over a second
//! store connection on the same durable database) spawns through the daemon
//! while a fake exchange endpoint delivers the launch command and captures
//! the `client.worker.launch_ack`: the accepted grant ack, the stale-stamp
//! refusal, the stale device-instance binding, the missing launch material,
//! the idempotent duplicate, and the cross-validation that the
//! supervisor-written managed-session config is accepted by the fail-closed
//! reader of the real `winwincode-worker --managed-session` entry.
//! Harness mirrors `tests/occupancy.rs`.

use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};
use winwincode_client_port::domain::{
    ClientArchitecture, ClientCapacityReport, ClientPlatformTarget, WorkerLaunchGrant,
    WorkerLaunchGrantState,
};
use winwincode_client_port::messages::{
    CLIENT_CONTROL_PORT_SCHEMA_VERSION, CommandContext, OccupancyCommandContext,
    ServerOccupancyOfferPayload, ServerToClientEnvelope, ServerToClientMessage,
    ServerWorkerLaunchPayload,
};
use winwincode_device_client::{
    DaemonConfig, DeviceDaemon, DeviceIdentitySeed, DeviceStore, ExchangeRequest, ExchangeResponse,
    ExchangeTransport, ExchangeTransportError, IdentityRecord, IssuedEnrollment, SessionSupervisor,
    SupervisorConfig, WORKER_STATE_RUNNING, WorkerLaunchDirectories, WorkerLaunchMaterialSource,
    adopt_enrollment, ensure_device_identity, load_device_identity,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-device-worker-launch-{name}-{}-{suffix}",
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

/// Canonical fixture identities.
const ASSIGNED_NODE: &str = "cnd_A1A1A1A1A1A1A1A1A1A1A1A1A1";
const ASSIGNED_PUBLIC_CLIENT_ID: &str = "0123456789";
const ISSUED_SECRET: [u8; 32] = [0x5d; 32];
const STAMP: &str = "2026-09-04T00:00:00.000Z";
const LEASE: &str = "ocl_AAAAAAAAAAAAAAAAAAAAAAAAAA";
const CLAIM: &str = "ocq_CCCCCCCCCCCCCCCCCCCCCCCCCC";
const HOLDER: &str = "usr_HOLDER0000000000000000000";
const ORIGIN: &str = "https://127.0.0.1:8443";
const GRANT: &str = "wlg_TESTGRANT000000000000001";
const WORKER_SESSION: &str = "ws_TESTSESSION00000000000001";
const WORKER_ID: &str = "wkr_TESTWORKER0000000000001";
const WORKER_INSTANCE: &str = "winst_TESTINSTANCE000000001";
const WORKER_CREDENTIAL: &str = "wsc-launch-test-material";
const BINDING: &str = "rbd_TESTBINDING00000000000001";
const PRODUCT_SESSION: &str = "ps_TESTPRODUCT000000000000001";
const STAGE_RUN: &str = "run_TESTSTAGE000000000000001";

fn worker_credential_digest() -> String {
    format!("sha256:{:x}", Sha256::digest(ISSUED_SECRET))
}

fn issued_credential_material() -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(ISSUED_SECRET.len() * 2);
    for byte in ISSUED_SECRET {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

/// Opens a store whose identity already adopted the server-issued
/// enrollment, so daemon sessions start in the enrolled phase.
fn open_enrolled(root: &Path) -> (DeviceStore, IdentityRecord) {
    let mut store = DeviceStore::open(root).expect("device store should open");
    ensure_device_identity(&mut store, &seed(), STAMP).expect("device identity should load");
    let device_id = load_device_identity(&store)
        .expect("identity read")
        .expect("fresh identity")
        .identity()
        .device_id()
        .to_owned();
    adopt_enrollment(
        &mut store,
        &device_id,
        &IssuedEnrollment {
            client_node_id: ASSIGNED_NODE.to_owned(),
            public_client_id: ASSIGNED_PUBLIC_CLIENT_ID.to_owned(),
            credential_material: issued_credential_material(),
            credential_digest: worker_credential_digest(),
        },
        STAMP,
    )
    .expect("enrollment adoption");
    let record = load_device_identity(&store)
        .expect("identity reload")
        .expect("enrolled identity");
    (store, record)
}

/// One client-to-server frame the fake server received.
#[derive(Clone, Debug)]
struct SimFrame {
    kind: String,
    frame: Value,
}

#[derive(Default)]
struct ServerState {
    received: Vec<SimFrame>,
    sequences: BTreeSet<u64>,
}

/// In-memory fake of the exchange endpoint: it records every frame,
/// acknowledges the contiguous prefix, and delivers one queued downlink
/// batch per exchange.
struct ServerSim {
    state: Mutex<ServerState>,
    downlink: Mutex<VecDeque<Vec<Value>>>,
    next_sequence: AtomicU64,
}

impl ServerSim {
    fn new() -> Self {
        Self {
            state: Mutex::new(ServerState::default()),
            downlink: Mutex::new(VecDeque::new()),
            next_sequence: AtomicU64::new(1),
        }
    }

    fn queue_downlink(&self, message: ServerToClientMessage) {
        let sequence = self.next_sequence.fetch_add(1, Ordering::Relaxed);
        let envelope = ServerToClientEnvelope {
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            message_id: format!("srv-down-{sequence}"),
            client_node_id: ASSIGNED_NODE.to_owned(),
            client_instance_id: "srv-instance".to_owned(),
            sequence,
            occurred_at: STAMP.to_owned(),
            message,
        };
        let frame = serde_json::to_value(&envelope).expect("downlink frame value");
        self.downlink
            .lock()
            .expect("downlink lock")
            .push_back(vec![frame]);
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
}

impl ExchangeTransport for ServerSim {
    fn exchange(
        &self,
        _credential: Option<&str>,
        request_bytes: &[u8],
    ) -> Result<Vec<u8>, ExchangeTransportError> {
        let request: ExchangeRequest = serde_json::from_slice(request_bytes)
            .map_err(|error| ExchangeTransportError::new(format!("fake decode: {error}")))?;
        let (ack, frames) = {
            let mut state = self.state.lock().expect("server sim lock");
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
                state.sequences.insert(sequence);
                state.received.push(SimFrame {
                    kind,
                    frame: frame.clone(),
                });
            }
            let mut ack = 0_u64;
            let mut next = 1_u64;
            while state.sequences.contains(&next) {
                ack = next;
                next += 1;
            }
            let frames = self
                .downlink
                .lock()
                .expect("downlink lock")
                .pop_front()
                .unwrap_or_default();
            (ack, frames)
        };
        let response = ExchangeResponse {
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            ack_sequence: ack,
            replay_from_sequence: None,
            frames,
            enrollment: None,
        };
        serde_json::to_vec(&response)
            .map_err(|error| ExchangeTransportError::new(format!("fake encode: {error}")))
    }
}

fn daemon_config() -> DaemonConfig {
    DaemonConfig {
        server_profile_id: "server-launch".to_owned(),
        base_url: "https://launch.example.test/internal/v1/client/exchange".to_owned(),
        server_display_name: "WinWinCode Control Plane".to_owned(),
        device_display_name: "Cheng's MacBook".to_owned(),
        platform: ClientPlatformTarget::Aarch64AppleDarwin,
        architecture: ClientArchitecture::Aarch64,
        client_version: "0.1.0-alpha.1".to_owned(),
        heartbeat_interval: Duration::from_millis(2),
        enroll_poll_interval: Duration::from_millis(1),
        max_frames_per_exchange: 16,
        initial_backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(16),
        capacity: ClientCapacityReport {
            max_concurrent_worker_sessions: 4,
            running_worker_sessions: 0,
            reserved_worker_sessions: 0,
            draining_worker_sessions: 0,
        },
    }
}

/// A long-lived worker test double: signals readiness next to the config
/// file after its handler is in place, then idles; a terminate exits zero.
const LONG_RUNNING_BODY: &str = "trap 'exit 0' TERM\n\
     echo ready > \"$(dirname \"$2\")/worker-ready\"\n\
     while :; do sleep 0.1; done";

fn write_executable_script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("script writes");
    let mut permissions = fs::metadata(path).expect("script metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("script chmod");
}

/// The launch material fixture: the credential the launch response would
/// have delivered to the local bridge plus the local worker directories.
struct FixedMaterial {
    credential: Mutex<Option<String>>,
    directories: Mutex<Option<WorkerLaunchDirectories>>,
}

impl FixedMaterial {
    fn new(directories: WorkerLaunchDirectories) -> Self {
        Self {
            credential: Mutex::new(Some(WORKER_CREDENTIAL.to_owned())),
            directories: Mutex::new(Some(directories)),
        }
    }

    fn without_credential(directories: WorkerLaunchDirectories) -> Self {
        Self {
            credential: Mutex::new(None),
            directories: Mutex::new(Some(directories)),
        }
    }
}

impl WorkerLaunchMaterialSource for FixedMaterial {
    fn worker_credential(&self, credential_digest: &str) -> Option<String> {
        if credential_digest != worker_credential_digest() {
            return None;
        }
        self.credential.lock().expect("credential lock").clone()
    }

    fn launch_directories(&self, worker_session_id: &str) -> Option<WorkerLaunchDirectories> {
        if worker_session_id != WORKER_SESSION {
            return None;
        }
        self.directories.lock().expect("directories lock").clone()
    }
}

/// Composed worker lane: the wired supervisor (over a second connection to
/// the daemon's durable database) and its local directories. Kills the
/// spawned worker on drop so a failing assertion leaks no process.
struct WorkerLane {
    supervisor: SessionSupervisor,
    material: Arc<FixedMaterial>,
}

impl WorkerLane {
    fn open(root: &Path, client_instance_id: &str, label: &str) -> Self {
        let source_root = root.join("worker-lane");
        fs::create_dir_all(&source_root).expect("worker lane root creates");
        let binary = source_root.join("worker-bin");
        write_executable_script(&binary, LONG_RUNNING_BODY);
        let supervisor_store = DeviceStore::open(root).expect("supervisor store connection opens");
        let supervisor = SessionSupervisor::new(
            SupervisorConfig {
                client_node_id: ASSIGNED_NODE.to_owned(),
                client_instance_id: client_instance_id.to_owned(),
                server_origin: ORIGIN.to_owned(),
                model_route: None,
                worker_binary_path: Some(binary.clone()),
                max_concurrent_worker_sessions: 4,
                stop_grace_period: Duration::from_secs(5),
            },
            supervisor_store,
        )
        .expect("supervisor builds");
        let directories = WorkerLaunchDirectories {
            source_directory: source_root.join("repo"),
            data_directory: source_root.join(format!("data-{label}")),
            worker_root: source_root.join(format!("root-{label}")),
        };
        Self {
            supervisor,
            material: Arc::new(FixedMaterial::new(directories)),
        }
    }

    fn directories(&self) -> WorkerLaunchDirectories {
        self.material
            .directories
            .lock()
            .expect("directories lock")
            .clone()
            .expect("directories")
    }
}

impl Drop for WorkerLane {
    fn drop(&mut self) {
        let _ = self.supervisor.stop(WORKER_SESSION, false);
    }
}

fn grant(client_instance_id: &str, lease: &str, token: u64) -> WorkerLaunchGrant {
    WorkerLaunchGrant {
        worker_launch_grant_id: GRANT.to_owned(),
        client_node_id: ASSIGNED_NODE.to_owned(),
        client_instance_id: client_instance_id.to_owned(),
        occupancy_lease_id: lease.to_owned(),
        occupancy_fencing_token: token,
        repository_binding_id: BINDING.to_owned(),
        product_session_id: PRODUCT_SESSION.to_owned(),
        stage_run_id: STAGE_RUN.to_owned(),
        worker_session_id: WORKER_SESSION.to_owned(),
        worker_id: WORKER_ID.to_owned(),
        worker_instance_id: WORKER_INSTANCE.to_owned(),
        credential_digest: worker_credential_digest(),
        expires_at: "2100-01-01T00:00:00.000Z".to_owned(),
        state: WorkerLaunchGrantState::Issued,
        revision: 1,
    }
}

fn occupancy_stamp(
    expected_revision: u64,
    idempotency_key: &str,
    lease: &str,
    token: u64,
) -> OccupancyCommandContext {
    OccupancyCommandContext {
        command: CommandContext {
            expected_revision,
            idempotency_key: idempotency_key.to_owned(),
        },
        occupancy_lease_id: lease.to_owned(),
        occupancy_fencing_token: token,
    }
}

fn offer(lease: &str, token: u64) -> ServerToClientMessage {
    ServerToClientMessage::OccupancyOffer(ServerOccupancyOfferPayload {
        occupancy: occupancy_stamp(0, "srv-offer", lease, token),
        claim_request_id: CLAIM.to_owned(),
        claimed_at: STAMP.to_owned(),
        holder_user_id: HOLDER.to_owned(),
        idle_expires_at: None,
    })
}

fn launch(
    stamp: OccupancyCommandContext,
    worker_grant: WorkerLaunchGrant,
) -> ServerToClientMessage {
    ServerToClientMessage::WorkerLaunch(ServerWorkerLaunchPayload {
        occupancy: stamp,
        launch_grant: worker_grant,
    })
}

/// Drives ticks until `condition` holds or the budget runs out.
fn drive_until(daemon: &mut DeviceDaemon, mut condition: impl FnMut(&mut DeviceDaemon) -> bool) {
    for _ in 0..600 {
        if condition(daemon) {
            return;
        }
        match daemon.tick(Instant::now()) {
            Ok(_) => thread::sleep(Duration::from_millis(1)),
            Err(error) => panic!("daemon tick failed fatally: {error:?}"),
        }
    }
    panic!(
        "the awaited daemon condition never held: status={:?}",
        daemon.status()
    );
}

/// The settled exchange state: every durable frame delivered and acked.
fn settled(daemon: &mut DeviceDaemon) -> bool {
    let snapshot = daemon.outbox_snapshot().expect("outbox snapshot");
    snapshot.frames.is_empty() && snapshot.ack_sequence == snapshot.highest_sequence
}

fn launch_acks(sim: &ServerSim) -> Vec<Value> {
    sim.frames_with_kind("client.worker.launch_ack")
        .into_iter()
        .map(|frame| frame.frame["payload"].clone())
        .collect()
}

fn wait_for_ready_marker(data_directory: &Path) {
    let marker = data_directory.join("worker-ready");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "the worker never signalled readiness"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn launch_command_spawns_the_worker_and_answers_the_grant_ack() {
    let root = temporary_directory("launch-accepted");
    let sim = Arc::new(ServerSim::new());
    let (store, identity) = open_enrolled(&root);
    let instance_id = identity.current_instance_id().to_owned();
    let mut daemon = DeviceDaemon::start(daemon_config(), store, sim.clone(), &identity)
        .expect("daemon should start enrolled");
    let lane = WorkerLane::open(&root, &instance_id, "accepted");
    daemon.set_worker_supervisor(lane.supervisor.clone());
    daemon.set_worker_launch_material_source(lane.material.clone());

    // The occupancy mirror advances through the real offer lane first: the
    // launch must fence against the persisted revision.
    sim.queue_downlink(offer(LEASE, 7));
    drive_until(&mut daemon, |daemon| {
        daemon.occupancy_mirror().is_some() && daemon.status().occupancy_offers_acked == 1
    });

    sim.queue_downlink(launch(
        occupancy_stamp(1, "srv-launch", LEASE, 7),
        grant(&instance_id, LEASE, 7),
    ));
    drive_until(&mut daemon, |daemon| {
        daemon.status().worker_launches_accepted == 1
    });
    drive_until(&mut daemon, settled);

    // The ack echoes the grant identities, the current mirror revision, and
    // the stable idempotency key.
    let acks = launch_acks(&sim);
    assert_eq!(acks.len(), 1, "{acks:?}");
    let ack = &acks[0];
    assert_eq!(ack["status"], json_str("accepted"));
    assert_eq!(ack["workerLaunchGrantId"], json_str(GRANT));
    assert_eq!(ack["workerSessionId"], json_str(WORKER_SESSION));
    assert_eq!(ack["workerId"], json_str(WORKER_ID));
    assert_eq!(ack["workerInstanceId"], json_str(WORKER_INSTANCE));
    assert_eq!(ack["occupancyLeaseId"], json_str(LEASE));
    assert_eq!(ack["occupancyFencingToken"], json_str("7"));
    assert_eq!(ack["expectedRevision"], serde_json::json!(1));
    assert_eq!(
        ack["idempotencyKey"],
        json_str(&format!("worker-launch-ack-{GRANT}"))
    );
    assert_eq!(
        daemon.status().worker_launches_rejected,
        0,
        "the matching stamp must accept: {:?}",
        daemon.status()
    );

    // The registry carries the grant identities, the live process, and the
    // launch binding; the capacity source reports the running session.
    let record = lane
        .supervisor
        .worker_process(WORKER_SESSION)
        .expect("registry read")
        .expect("the launch registered the worker");
    assert_eq!(record.state, WORKER_STATE_RUNNING);
    assert_eq!(record.worker_id, WORKER_ID);
    assert_eq!(record.worker_instance_id, WORKER_INSTANCE);
    assert_eq!(record.launch_grant_id, GRANT);
    assert_eq!(record.occupancy_lease_id, LEASE);
    assert_eq!(record.repository_binding_id, BINDING);
    let snapshot = winwincode_device_client::WorkerCapacitySnapshot {
        running_worker_sessions: 1,
        reserved_worker_sessions: 0,
    };
    assert_eq!(
        winwincode_device_client::WorkerCapacitySource::worker_capacity(&lane.supervisor),
        snapshot,
        "the spawned worker is the live running fact"
    );

    // The private files exist with mode 0600 and the credential content is
    // exactly the launch-response material.
    let directories = lane.directories();
    wait_for_ready_marker(&directories.data_directory);
    for private in ["managed-session.json", "worker-credential"] {
        let mode = fs::metadata(directories.data_directory.join(private))
            .expect("private file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "{private} must be mode 0600");
    }
    assert_eq!(
        fs::read_to_string(directories.data_directory.join("worker-credential"))
            .expect("credential read"),
        WORKER_CREDENTIAL,
        "the one-time material crossed to the credential file"
    );

    daemon.into_store().close().expect("store close");
    drop(lane);
    cleanup(&root);
}

#[test]
fn a_stale_launch_stamp_is_refused_without_touching_the_device() {
    let root = temporary_directory("launch-stale");
    let sim = Arc::new(ServerSim::new());
    let (store, identity) = open_enrolled(&root);
    let instance_id = identity.current_instance_id().to_owned();
    let mut daemon = DeviceDaemon::start(daemon_config(), store, sim.clone(), &identity)
        .expect("daemon should start enrolled");
    let lane = WorkerLane::open(&root, &instance_id, "stale");
    daemon.set_worker_supervisor(lane.supervisor.clone());
    daemon.set_worker_launch_material_source(lane.material.clone());

    sim.queue_downlink(offer(LEASE, 7));
    drive_until(&mut daemon, |daemon| daemon.occupancy_mirror().is_some());

    // The launch carries a token the mirror never held: refused before any
    // local action.
    sim.queue_downlink(launch(
        occupancy_stamp(1, "srv-launch", LEASE, 9),
        grant(&instance_id, LEASE, 9),
    ));
    drive_until(&mut daemon, |daemon| {
        daemon.status().worker_launches_rejected == 1 && settled(daemon)
    });

    let acks = launch_acks(&sim);
    assert_eq!(acks.len(), 1, "{acks:?}");
    assert_eq!(acks[0]["status"], json_str("rejected_stale_fencing_token"));
    assert_eq!(
        acks[0]["error"]["code"],
        json_str("STALE_FENCING_TOKEN"),
        "{:?}",
        acks[0]["error"]
    );
    assert_eq!(acks[0]["workerInstanceId"], json_str(WORKER_INSTANCE));
    assert!(
        lane.supervisor
            .worker_process(WORKER_SESSION)
            .expect("registry read")
            .is_none(),
        "a refused launch registers nothing"
    );
    assert!(
        !lane
            .directories()
            .data_directory
            .join("worker-credential")
            .exists(),
        "a refused launch writes no private files"
    );

    daemon.into_store().close().expect("store close");
    drop(lane);
    cleanup(&root);
}

#[test]
#[allow(clippy::too_many_lines)]
fn a_launch_bound_to_another_instance_or_deferred_material() {
    let root = temporary_directory("launch-binding");
    let sim = Arc::new(ServerSim::new());
    let (store, identity) = open_enrolled(&root);
    let instance_id = identity.current_instance_id().to_owned();
    let mut daemon = DeviceDaemon::start(daemon_config(), store, sim.clone(), &identity)
        .expect("daemon should start enrolled");
    let lane = WorkerLane::open(&root, &instance_id, "binding");
    daemon.set_worker_supervisor(lane.supervisor.clone());
    // The credential has not crossed to the local bridge in this scenario:
    // the launch still spawns (placeholder private file) because the launch
    // acknowledgement must not wait for the post-consumption delivery.
    let material = Arc::new(FixedMaterial::without_credential(lane.directories()));
    daemon.set_worker_launch_material_source(material);

    sim.queue_downlink(offer(LEASE, 7));
    drive_until(&mut daemon, |daemon| daemon.occupancy_mirror().is_some());

    // A grant minted for a previous device boot can never spawn here.
    sim.queue_downlink(launch(
        occupancy_stamp(1, "srv-launch-old", LEASE, 7),
        grant("cix_PREVIOUS0000000000000000001", LEASE, 7),
    ));
    drive_until(&mut daemon, |daemon| {
        daemon.status().worker_launches_rejected == 1
    });

    // The correct grant with deferred material spawns with a placeholder
    // credential file and answers accepted.
    sim.queue_downlink(launch(
        occupancy_stamp(1, "srv-launch", LEASE, 7),
        grant(&instance_id, LEASE, 7),
    ));
    drive_until(&mut daemon, |daemon| {
        daemon.status().worker_launches_accepted == 1 && settled(daemon)
    });

    let acks = launch_acks(&sim);
    assert_eq!(acks.len(), 2, "{acks:?}");
    assert_eq!(acks[0]["status"], json_str("rejected_wrong_state"));
    assert_eq!(
        acks[0]["error"]["code"],
        json_str("DEVICE_INSTANCE_CHANGED"),
        "{:?}",
        acks[0]["error"]
    );
    assert_eq!(acks[1]["status"], json_str("accepted"));
    let directories = lane.directories();
    wait_for_ready_marker(&directories.data_directory);
    let credential_path = directories.data_directory.join("worker-credential");
    assert_eq!(
        fs::read_to_string(&credential_path).expect("placeholder credential read"),
        "",
        "the deferred delivery starts from an empty private file"
    );

    // The deferred delivery writes the material into the private file.
    let delivered = daemon
        .receive_worker_credential(&worker_credential_digest(), WORKER_CREDENTIAL)
        .expect("deferred delivery");
    assert!(delivered, "the handled launch's digest is known");
    assert_eq!(
        fs::read_to_string(&credential_path).expect("credential read"),
        WORKER_CREDENTIAL,
        "the launch-response material lands in the 0600 credential file"
    );
    assert_eq!(file_mode(&credential_path), 0o600);
    // An unknown digest is a no-op.
    let unknown = daemon
        .receive_worker_credential("sha256:unknown", WORKER_CREDENTIAL)
        .expect("unknown delivery");
    assert!(!unknown);

    assert!(
        lane.supervisor
            .worker_process(WORKER_SESSION)
            .expect("registry read")
            .is_some(),
        "the deferred launch registered its worker session"
    );
    assert_eq!(
        lane.supervisor
            .count_worker_processes_in_state(WORKER_STATE_RUNNING)
            .expect("running count"),
        1,
        "exactly the deferred launch spawned"
    );

    daemon.into_store().close().expect("store close");
    drop(lane);
    cleanup(&root);
}

#[test]
fn a_replayed_launch_answers_duplicate_without_a_second_process() {
    let root = temporary_directory("launch-duplicate");
    let sim = Arc::new(ServerSim::new());
    let (store, identity) = open_enrolled(&root);
    let instance_id = identity.current_instance_id().to_owned();
    let mut daemon = DeviceDaemon::start(daemon_config(), store, sim.clone(), &identity)
        .expect("daemon should start enrolled");
    let lane = WorkerLane::open(&root, &instance_id, "duplicate");
    daemon.set_worker_supervisor(lane.supervisor.clone());
    daemon.set_worker_launch_material_source(lane.material.clone());

    sim.queue_downlink(offer(LEASE, 7));
    drive_until(&mut daemon, |daemon| daemon.occupancy_mirror().is_some());

    // The same launch twice: the first spawns, the second is the
    // one-session-one-worker idempotent replay.
    sim.queue_downlink(launch(
        occupancy_stamp(1, "srv-launch", LEASE, 7),
        grant(&instance_id, LEASE, 7),
    ));
    sim.queue_downlink(launch(
        occupancy_stamp(1, "srv-launch", LEASE, 7),
        grant(&instance_id, LEASE, 7),
    ));
    drive_until(&mut daemon, |daemon| {
        daemon.status().worker_launches_accepted == 2 && settled(daemon)
    });

    let acks = launch_acks(&sim);
    assert_eq!(acks.len(), 2, "{acks:?}");
    assert_eq!(acks[0]["status"], json_str("accepted"));
    assert_eq!(acks[1]["status"], json_str("duplicate"));
    assert_eq!(acks[0]["workerInstanceId"], acks[1]["workerInstanceId"]);
    assert_eq!(
        lane.supervisor
            .count_worker_processes_in_state(WORKER_STATE_RUNNING)
            .expect("running count"),
        1,
        "one session one worker: the replay spawned nothing"
    );

    daemon.into_store().close().expect("store close");
    drop(lane);
    cleanup(&root);
}

/// Cross-validation (WORKER-200.2): the managed-session config the
/// supervisor writes for a daemon-driven launch must be accepted by the
/// fail-closed reader of the real `winwincode-worker --managed-session`
/// entry, field for field.
#[test]
#[allow(clippy::too_many_lines)]
fn the_written_config_is_accepted_by_the_real_worker_entry_reader() {
    use winwincode_worker::managed_session::ManagedSessionConfig;

    let root = temporary_directory("launch-cross-validation");
    let sim = Arc::new(ServerSim::new());
    let (store, identity) = open_enrolled(&root);
    let instance_id = identity.current_instance_id().to_owned();
    let mut daemon = DeviceDaemon::start(daemon_config(), store, sim.clone(), &identity)
        .expect("daemon should start enrolled");
    let lane = WorkerLane::open(&root, &instance_id, "cross-validation");
    daemon.set_worker_supervisor(lane.supervisor.clone());
    daemon.set_worker_launch_material_source(lane.material.clone());

    sim.queue_downlink(offer(LEASE, 7));
    drive_until(&mut daemon, |daemon| daemon.occupancy_mirror().is_some());
    sim.queue_downlink(launch(
        occupancy_stamp(1, "srv-launch", LEASE, 7),
        grant(&instance_id, LEASE, 7),
    ));
    drive_until(&mut daemon, |daemon| {
        daemon.status().worker_launches_accepted == 1
    });

    let directories = lane.directories();
    let config_path = directories.data_directory.join("managed-session.json");
    let config = ManagedSessionConfig::read(&config_path).expect("the real reader accepts");
    assert_eq!(config.client_node_id.0, ASSIGNED_NODE);
    assert_eq!(config.client_instance_id.0, instance_id);
    assert_eq!(config.occupancy_lease_id.0, LEASE);
    assert_eq!(config.occupancy_fencing_token, 7);
    assert_eq!(config.repository_binding_id.0, BINDING);
    assert_eq!(
        config
            .product_session_id
            .as_ref()
            .map(|v| v.0.clone())
            .as_deref(),
        Some(PRODUCT_SESSION)
    );
    assert_eq!(
        config.stage_run_id.as_ref().map(|v| v.0.clone()).as_deref(),
        Some(STAGE_RUN)
    );
    assert_eq!(config.worker_session_id.0, WORKER_SESSION);
    assert_eq!(config.worker_id.0, WORKER_ID);
    assert_eq!(config.worker_instance_id.0, WORKER_INSTANCE);
    assert_eq!(config.source_directory, directories.source_directory);
    assert_eq!(config.data_directory, directories.data_directory);
    assert_eq!(config.server_origin, ORIGIN);
    assert_eq!(
        config.worker_credential_path,
        directories.data_directory.join("worker-credential")
    );

    daemon.into_store().close().expect("store close");
    drop(lane);
    cleanup(&root);
}

fn json_str(value: &str) -> Value {
    Value::String(value.to_owned())
}

fn file_mode(path: &Path) -> u32 {
    fs::metadata(path)
        .expect("file metadata")
        .permissions()
        .mode()
        & 0o777
}
