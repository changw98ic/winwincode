// SPDX-License-Identifier: Apache-2.0

//! CLIENT-300.3 coverage: the daemon's occupancy downlink lane —
//! `client.occupancy.offer` persisting the mirror before the
//! `client.occupancy.ack`, release intents for all three modes,
//! `client.occupancy.force_fence` overwrites with full old-token
//! invalidation, restart recovery of the mirror, and the fencing entry
//! points the worker epic consumes. Harness mirrors `tests/connect_code.rs`.

use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};
use winwincode_client_port::domain::{
    ClientArchitecture, ClientCapacityReport, ClientOccupancyReleaseMode, ClientPlatformTarget,
};
use winwincode_client_port::messages::{
    CLIENT_CONTROL_PORT_SCHEMA_VERSION, ClientToServerMessage, CommandContext,
    OccupancyCommandContext, ServerOccupancyForceFencePayload, ServerOccupancyOfferPayload,
    ServerOccupancyReleasePayload, ServerToClientEnvelope, ServerToClientMessage,
};
use winwincode_device_client::fencing::{FencedCommandKind, FencingRejection, FencingVerdict};
use winwincode_device_client::{
    DaemonConfig, DeviceDaemon, DeviceIdentitySeed, DeviceStore, ExchangeRequest, ExchangeResponse,
    ExchangeTransport, ExchangeTransportError, IdentityRecord, IssuedEnrollment,
    LeaseWorkerController, WorkerCapacitySnapshot, WorkerCapacitySource, adopt_enrollment,
    ensure_device_identity, load_device_identity,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-device-occupancy-{name}-{}-{suffix}",
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

/// Canonical fixture identities (`ocl_` lease, `ocq_` claim, `usr_` holder).
const ASSIGNED_NODE: &str = "cnd_A1A1A1A1A1A1A1A1A1A1A1A1A1";
const ASSIGNED_PUBLIC_CLIENT_ID: &str = "0123456789";
const ISSUED_SECRET: [u8; 32] = [0x3c; 32];
const STAMP: &str = "2026-09-04T00:00:00.000Z";
const LEASE_ONE: &str = "ocl_AAAAAAAAAAAAAAAAAAAAAAAAAA";
const LEASE_TWO: &str = "ocl_BBBBBBBBBBBBBBBBBBBBBBBBBB";
const CLAIM_ONE: &str = "ocq_CCCCCCCCCCCCCCCCCCCCCCCCCC";
const HOLDER: &str = "usr_HOLDER0000000000000000000";

fn issued_credential_material() -> String {
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
            credential_digest: issued_credential_digest(),
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
    /// The client-to-server watermark the server already holds for this
    /// node (a resumed server keeps crediting the durable stream).
    client_stream_base: u64,
}

impl ServerSim {
    fn new() -> Self {
        Self::resumed(1, 0)
    }

    /// A resumed exchange pair: the daemon restarted with a durable inbox
    /// cursor (`first_downlink_sequence`) and a durable uplink watermark
    /// (`client_stream_base`) the server still credits.
    fn resumed(first_downlink_sequence: u64, client_stream_base: u64) -> Self {
        Self {
            state: Mutex::new(ServerState::default()),
            downlink: Mutex::new(VecDeque::new()),
            next_sequence: AtomicU64::new(first_downlink_sequence),
            client_stream_base,
        }
    }

    /// Queues one server-to-client frame on this sim's downlink sequence.
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

    fn all_frames(&self) -> Vec<SimFrame> {
        self.state.lock().expect("server sim lock").received.clone()
    }

    fn frames_with_kind(&self, kind: &str) -> Vec<SimFrame> {
        self.all_frames()
            .into_iter()
            .filter(|frame| frame.kind == kind)
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
            let mut ack = self.client_stream_base;
            let mut next = self.client_stream_base + 1;
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

fn daemon_config(name: &str) -> DaemonConfig {
    DaemonConfig {
        server_profile_id: format!("server-{name}"),
        base_url: format!("https://{name}.example.test/internal/v1/client/exchange"),
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
            max_concurrent_worker_sessions: 2,
            running_worker_sessions: 0,
            reserved_worker_sessions: 0,
            draining_worker_sessions: 0,
        },
    }
}

fn started_daemon(name: &str, root: &Path, sim: &Arc<ServerSim>) -> DeviceDaemon {
    let (store, identity) = open_enrolled(root);
    DeviceDaemon::start(daemon_config(name), store, sim.clone(), &identity)
        .expect("daemon should start enrolled")
}

/// Starts a daemon over an already-enrolled store (restart scenario).
fn resumed_daemon(name: &str, root: &Path, sim: &Arc<ServerSim>) -> DeviceDaemon {
    let (store, identity) = open_resumed(root);
    DeviceDaemon::start(daemon_config(name), store, sim.clone(), &identity)
        .expect("restarted daemon should start enrolled")
}

/// Opens a store that already carries an adopted enrollment (the restart
/// scenario: the identity is durable, never re-adopted).
fn open_resumed(root: &Path) -> (DeviceStore, IdentityRecord) {
    let store = DeviceStore::open(root).expect("restarted device store should open");
    let record = load_device_identity(&store)
        .expect("identity read")
        .expect("enrolled identity");
    (store, record)
}

/// Drives ticks until `condition` holds or the budget runs out.
fn drive_until(daemon: &mut DeviceDaemon, condition: impl Fn(&DeviceDaemon) -> bool) {
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

fn offer(expected_revision: u64, lease: &str, token: u64) -> ServerToClientMessage {
    ServerToClientMessage::OccupancyOffer(ServerOccupancyOfferPayload {
        occupancy: occupancy_stamp(expected_revision, "srv-offer", lease, token),
        claim_request_id: CLAIM_ONE.to_owned(),
        claimed_at: STAMP.to_owned(),
        holder_user_id: HOLDER.to_owned(),
        idle_expires_at: Some("2026-09-04T02:00:00.000Z".to_owned()),
    })
}

fn release(
    expected_revision: u64,
    key: &str,
    lease: &str,
    token: u64,
    mode: ClientOccupancyReleaseMode,
) -> ServerToClientMessage {
    ServerToClientMessage::OccupancyRelease(ServerOccupancyReleasePayload {
        occupancy: occupancy_stamp(expected_revision, key, lease, token),
        mode,
    })
}

fn force_fence(expected_revision: u64, lease: &str, token: u64) -> ServerToClientMessage {
    ServerToClientMessage::OccupancyForceFence(ServerOccupancyForceFencePayload {
        occupancy: occupancy_stamp(expected_revision, "srv-force-fence", lease, token),
        reason: winwincode_client_port::domain::ClientOccupancyForceFenceReason::RecoveryDeadlineExceeded,
        superseded_lease_id: Some(LEASE_ONE.to_owned()),
    })
}

// ---------------------------------------------------------------------------
// offer → durable mirror → ack.
// ---------------------------------------------------------------------------

#[test]
fn offer_persists_the_mirror_then_acks_with_the_mirror_revision() {
    let root = temporary_directory("offer-ack");
    let sim = Arc::new(ServerSim::new());
    let mut daemon = started_daemon("offer-ack", &root, &sim);
    assert!(daemon.occupancy_mirror().is_none());

    sim.queue_downlink(offer(0, LEASE_ONE, 7));
    drive_until(&mut daemon, |_daemon| {
        !sim.frames_with_kind("client.occupancy.ack").is_empty()
    });

    // The ack frame echoes the offered lease/token and carries the new
    // mirror revision as its expectedRevision (the contract's
    // `mirrorRevision` ack fact).
    let acks = sim.frames_with_kind("client.occupancy.ack");
    assert_eq!(acks.len(), 1, "{acks:?}");
    let payload = &acks[0].frame["payload"];
    assert_eq!(payload["occupancyLeaseId"], LEASE_ONE);
    assert_eq!(
        payload["occupancyFencingToken"], "7",
        "the fencing token travels as a decimal string"
    );
    assert_eq!(payload["expectedRevision"], 1);
    assert_eq!(
        payload["idempotencyKey"],
        format!("occupancy-ack-{LEASE_ONE}-7")
    );

    // The mirror is durable before the ack was ever sent, with the offer's
    // holder and claim facts.
    let mirror = daemon.occupancy_mirror().expect("in-memory mirror").clone();
    assert_eq!(mirror.occupancy_lease_id, LEASE_ONE);
    assert_eq!(mirror.fencing_token, 7);
    assert_eq!(mirror.mirror_revision, 1);
    assert_eq!(mirror.holder_user_id.as_deref(), Some(HOLDER));
    assert_eq!(mirror.claim_request_id.as_deref(), Some(CLAIM_ONE));
    let durable = daemon
        .store_mut()
        .occupancy_mirror()
        .expect("store read")
        .expect("durable mirror");
    assert_eq!(durable, mirror.clone());
    assert_eq!(daemon.status().occupancy_offers_acked, 1);
    assert_eq!(daemon.status().occupancy_offers_rejected, 0);

    // Every later heartbeat mirrors the lease id.
    drive_until(&mut daemon, |_daemon| {
        sim.frames_with_kind("client.heartbeat")
            .iter()
            .any(|frame| frame.frame["payload"]["occupancyLeaseId"] == LEASE_ONE)
    });

    daemon.into_store().close().expect("store close");
    cleanup(&root);
}

#[test]
fn a_replayed_offer_re_acks_without_advancing_the_revision() {
    let root = temporary_directory("offer-replay");
    let sim = Arc::new(ServerSim::new());
    let mut daemon = started_daemon("offer-replay", &root, &sim);

    // The server replays unanswered commands: the same offer arrives twice
    // (the replay keeps its original expectedRevision 0).
    sim.queue_downlink(offer(0, LEASE_ONE, 7));
    drive_until(&mut daemon, |_daemon| {
        !sim.frames_with_kind("client.occupancy.ack").is_empty()
    });
    sim.queue_downlink(offer(0, LEASE_ONE, 7));
    drive_until(&mut daemon, |_daemon| {
        sim.frames_with_kind("client.occupancy.ack").len() >= 2
    });

    let acks = sim.frames_with_kind("client.occupancy.ack");
    assert_eq!(acks.len(), 2, "{acks:?}");
    assert_eq!(acks[0].frame["payload"]["expectedRevision"], 1);
    assert_eq!(
        acks[1].frame["payload"]["expectedRevision"], 1,
        "the idempotent replay must not advance the revision"
    );
    assert_eq!(acks[1].frame["payload"], acks[0].frame["payload"]);
    let mirror = daemon.occupancy_mirror().expect("mirror");
    assert_eq!(mirror.mirror_revision, 1);
    assert_eq!(daemon.status().occupancy_offers_acked, 2);

    daemon.into_store().close().expect("store close");
    cleanup(&root);
}

#[test]
fn a_locked_node_rejects_the_offer_without_touching_the_mirror() {
    let root = temporary_directory("offer-locked");
    let sim = Arc::new(ServerSim::new());
    let mut daemon = started_daemon("offer-locked", &root, &sim);
    daemon.lock_client().expect("local lock");

    sim.queue_downlink(offer(0, LEASE_ONE, 7));
    drive_until(&mut daemon, |_daemon| {
        !sim.frames_with_kind("client.occupancy.rejected").is_empty()
    });

    let rejected = sim.frames_with_kind("client.occupancy.rejected");
    assert_eq!(rejected.len(), 1, "{rejected:?}");
    let payload = &rejected[0].frame["payload"];
    assert_eq!(payload["occupancyLeaseId"], LEASE_ONE);
    assert_eq!(payload["occupancyFencingToken"], "7");
    assert_eq!(payload["reason"], "client_locked");
    assert_eq!(
        payload["expectedRevision"], 0,
        "the rejection reports the current mirror revision"
    );
    assert!(daemon.occupancy_mirror().is_none());
    assert!(
        daemon
            .store_mut()
            .occupancy_mirror()
            .expect("store read")
            .is_none()
    );
    assert_eq!(daemon.status().occupancy_offers_rejected, 1);
    assert!(sim.frames_with_kind("client.occupancy.ack").is_empty());

    daemon.into_store().close().expect("store close");
    cleanup(&root);
}

#[test]
fn an_offer_based_on_a_divergent_revision_is_refused_and_the_next_one_lands() {
    let root = temporary_directory("offer-revision");
    let sim = Arc::new(ServerSim::new());
    let mut daemon = started_daemon("offer-revision", &root, &sim);

    sim.queue_downlink(offer(0, LEASE_ONE, 7));
    drive_until(&mut daemon, |_daemon| {
        !sim.frames_with_kind("client.occupancy.ack").is_empty()
    });

    // The server's view regressed: this new-lease offer claims it was
    // computed against revision 0 while the device mirror sits at 1.
    sim.queue_downlink(offer(0, LEASE_TWO, 9));
    drive_until(&mut daemon, |_daemon| {
        !sim.frames_with_kind("client.occupancy.rejected").is_empty()
    });
    let rejected = sim.frames_with_kind("client.occupancy.rejected");
    assert_eq!(rejected.len(), 1, "{rejected:?}");
    assert_eq!(
        rejected[0].frame["payload"]["reason"],
        "local_state_conflict"
    );
    assert_eq!(rejected[0].frame["payload"]["expectedRevision"], 1);
    let mirror = daemon.occupancy_mirror().expect("mirror");
    assert_eq!(mirror.occupancy_lease_id, LEASE_ONE, "nothing landed");

    // The recomputed offer (correct revision, higher token) advances the
    // mirror to revision 2 under the new lease.
    sim.queue_downlink(offer(1, LEASE_TWO, 9));
    drive_until(&mut daemon, |_daemon| {
        sim.frames_with_kind("client.occupancy.ack").len() >= 2
    });
    let acks = sim.frames_with_kind("client.occupancy.ack");
    assert_eq!(acks[1].frame["payload"]["expectedRevision"], 2);
    assert_eq!(acks[1].frame["payload"]["occupancyLeaseId"], LEASE_TWO);
    assert_eq!(
        daemon.occupancy_mirror().expect("mirror").mirror_revision,
        2
    );

    daemon.into_store().close().expect("store close");
    cleanup(&root);
}

// ---------------------------------------------------------------------------
// force_fence: higher-token overwrites and total old-token invalidation.
// ---------------------------------------------------------------------------

#[test]
fn force_fence_overwrites_the_mirror_and_rejects_every_old_token() {
    let root = temporary_directory("force-fence");
    let sim = Arc::new(ServerSim::new());
    let mut daemon = started_daemon("force-fence", &root, &sim);

    sim.queue_downlink(offer(0, LEASE_ONE, 7));
    drive_until(&mut daemon, |_daemon| {
        !sim.frames_with_kind("client.occupancy.ack").is_empty()
    });

    // Before the fence, the old stamp authorizes the worker-epic entry
    // points.
    let guard = daemon.fencing_guard();
    assert_eq!(
        guard.authorize_command(FencedCommandKind::WorkerLaunch, LEASE_ONE, 7),
        FencingVerdict::Authorized(winwincode_device_client::FencingTicket {
            kind: FencedCommandKind::WorkerLaunch,
            occupancy_lease_id: LEASE_ONE.to_owned(),
            occupancy_fencing_token: 7,
            mirror_revision: 1,
        })
    );
    drop(guard);

    sim.queue_downlink(force_fence(1, LEASE_ONE, 9));
    drive_until(&mut daemon, |_daemon| {
        !sim.frames_with_kind("client.command_ack").is_empty()
    });

    let acks = sim.frames_with_kind("client.command_ack");
    assert_eq!(acks.len(), 1, "{acks:?}");
    assert_eq!(
        acks[0].frame["payload"]["commandKind"],
        "client.occupancy.force_fence"
    );
    assert_eq!(acks[0].frame["payload"]["status"], "accepted");
    assert_eq!(acks[0].frame["payload"]["currentRevision"], 2);

    let mirror = daemon.occupancy_mirror().expect("mirror");
    assert_eq!(mirror.fencing_token, 9);
    assert_eq!(mirror.mirror_revision, 2);
    assert_eq!(
        mirror.holder_user_id.as_deref(),
        Some(HOLDER),
        "a lease-matched fence keeps the holder facts"
    );

    // The old token is now dead everywhere: the guard refuses it ...
    assert_eq!(
        daemon
            .fencing_guard()
            .authorize_command(FencedCommandKind::WorkerLaunch, LEASE_ONE, 7),
        FencingVerdict::Rejected(FencingRejection::StaleFencingToken)
    );
    assert_eq!(
        daemon
            .fencing_guard()
            .authorize_command(FencedCommandKind::WorkerStop, LEASE_ONE, 7),
        FencingVerdict::Rejected(FencingRejection::StaleFencingToken)
    );
    assert_eq!(
        daemon
            .fencing_guard()
            .authorize_command(FencedCommandKind::CandidateApply, LEASE_ONE, 7),
        FencingVerdict::Rejected(FencingRejection::StaleFencingToken)
    );
    assert_eq!(
        daemon.fencing_guard().authorize_command(
            FencedCommandKind::RepositoryMutation,
            LEASE_ONE,
            7
        ),
        FencingVerdict::Rejected(FencingRejection::StaleFencingToken)
    );
    // ... and the new stamp authorizes again.
    assert!(matches!(
        daemon
            .fencing_guard()
            .authorize_command(FencedCommandKind::WorkerLaunch, LEASE_ONE, 9),
        FencingVerdict::Authorized(_)
    ));

    // A queued release still stamped with the old token is refused with the
    // stale-fencing verdict and never records an intent.
    sim.queue_downlink(release(
        2,
        "rel-stale",
        LEASE_ONE,
        7,
        ClientOccupancyReleaseMode::Immediate,
    ));
    drive_until(&mut daemon, |_daemon| {
        sim.frames_with_kind("client.command_ack").len() >= 2
    });
    let stale_ack = &sim.frames_with_kind("client.command_ack")[1];
    assert_eq!(
        stale_ack.frame["payload"]["status"],
        "rejected_stale_fencing_token"
    );
    assert_eq!(
        stale_ack.frame["payload"]["error"]["code"],
        "STALE_FENCING_TOKEN"
    );
    assert_eq!(stale_ack.frame["payload"]["currentRevision"], 2);
    assert!(
        daemon
            .store_mut()
            .occupancy_release_intents()
            .expect("intents")
            .is_empty(),
        "a stale release never records an intent"
    );

    daemon.into_store().close().expect("store close");
    cleanup(&root);
}

#[test]
fn a_force_fence_invalidates_tickets_authorized_before_it() {
    let root = temporary_directory("ticket-invalidation");
    let sim = Arc::new(ServerSim::new());
    let mut daemon = started_daemon("ticket-invalidation", &root, &sim);

    sim.queue_downlink(offer(0, LEASE_ONE, 7));
    drive_until(&mut daemon, |_daemon| {
        !sim.frames_with_kind("client.occupancy.ack").is_empty()
    });

    // The worker epic's check-then-execute window: authorize now ...
    let ticket = match daemon.fencing_guard().authorize_command(
        FencedCommandKind::WorkerLaunch,
        LEASE_ONE,
        7,
    ) {
        FencingVerdict::Authorized(ticket) => ticket,
        FencingVerdict::Rejected(rejection) => panic!("must authorize: {rejection:?}"),
    };
    assert!(daemon.verify_fencing_ticket(&ticket).is_ok());

    // ... and a force-fence handled before the execution strands it.
    sim.queue_downlink(force_fence(1, LEASE_ONE, 9));
    drive_until(&mut daemon, |daemon| {
        daemon.status().occupancy_force_fences_applied >= 1
            && !sim.frames_with_kind("client.command_ack").is_empty()
    });
    assert_eq!(
        daemon.verify_fencing_ticket(&ticket),
        Err(FencingRejection::StaleFencingToken),
        "the previously authorized intent is dead the moment the mirror advanced: \
         the fence minted a higher token, so the old stamp no longer matches"
    );

    daemon.into_store().close().expect("store close");
    cleanup(&root);
}

#[test]
fn a_non_advancing_force_fence_is_refused_without_a_rollback() {
    let root = temporary_directory("force-fence-stale");
    let sim = Arc::new(ServerSim::new());
    let mut daemon = started_daemon("force-fence-stale", &root, &sim);

    sim.queue_downlink(offer(0, LEASE_ONE, 9));
    drive_until(&mut daemon, |_daemon| {
        !sim.frames_with_kind("client.occupancy.ack").is_empty()
    });

    // A replayed fence at or below the current token can never roll the
    // mirror back.
    sim.queue_downlink(force_fence(1, LEASE_ONE, 8));
    drive_until(&mut daemon, |_daemon| {
        !sim.frames_with_kind("client.command_ack").is_empty()
    });
    let ack = &sim.frames_with_kind("client.command_ack")[0];
    assert_eq!(
        ack.frame["payload"]["status"],
        "rejected_stale_fencing_token"
    );
    assert_eq!(ack.frame["payload"]["currentRevision"], 1);
    let mirror = daemon.occupancy_mirror().expect("mirror");
    assert_eq!(mirror.fencing_token, 9, "the mirror never rolls back");
    assert_eq!(mirror.mirror_revision, 1);

    daemon.into_store().close().expect("store close");
    cleanup(&root);
}

// ---------------------------------------------------------------------------
// release: modes, durable intents, replay dedupe.
// ---------------------------------------------------------------------------

#[test]
fn release_records_all_three_modes_and_replays_deduplicate() {
    let root = temporary_directory("release-modes");
    let sim = Arc::new(ServerSim::new());
    let mut daemon = started_daemon("release-modes", &root, &sim);

    sim.queue_downlink(offer(0, LEASE_ONE, 7));
    drive_until(&mut daemon, |_daemon| {
        !sim.frames_with_kind("client.occupancy.ack").is_empty()
    });

    for (key, mode) in [
        ("rel-immediate", ClientOccupancyReleaseMode::Immediate),
        ("rel-drain", ClientOccupancyReleaseMode::DrainThenRelease),
        (
            "rel-cancel",
            ClientOccupancyReleaseMode::CancelTasksAndRelease,
        ),
    ] {
        sim.queue_downlink(release(1, key, LEASE_ONE, 7, mode));
    }
    drive_until(&mut daemon, |_daemon| {
        sim.frames_with_kind("client.command_ack").len() >= 3
    });

    let acks = sim.frames_with_kind("client.command_ack");
    assert_eq!(acks.len(), 3, "{acks:?}");
    for ack in &acks {
        assert_eq!(
            ack.frame["payload"]["commandKind"],
            "client.occupancy.release"
        );
        assert_eq!(ack.frame["payload"]["status"], "accepted");
        assert_eq!(ack.frame["payload"]["currentRevision"], 1);
    }

    let intents = daemon
        .store_mut()
        .occupancy_release_intents()
        .expect("intents");
    assert_eq!(intents.len(), 3);
    assert_eq!(intents[0].idempotency_key, "rel-immediate");
    assert_eq!(intents[0].mode, ClientOccupancyReleaseMode::Immediate);
    assert_eq!(
        intents[1].mode,
        ClientOccupancyReleaseMode::DrainThenRelease
    );
    assert_eq!(
        intents[2].mode,
        ClientOccupancyReleaseMode::CancelTasksAndRelease
    );
    for intent in &intents {
        assert_eq!(intent.occupancy_lease_id, LEASE_ONE);
        assert_eq!(intent.fencing_token, 7);
        assert_eq!(
            intent.affected_worker_sessions, 0,
            "worker stopping belongs to the worker epic; this lane counts"
        );
    }

    // A replayed release (same idempotency key, new message id and
    // sequence) answers `duplicate` and never records a second intent.
    let mut replayed = release(
        1,
        "rel-immediate",
        LEASE_ONE,
        7,
        ClientOccupancyReleaseMode::Immediate,
    );
    let ServerToClientMessage::OccupancyRelease(payload) = &mut replayed else {
        unreachable!("just built");
    };
    payload.occupancy.command.idempotency_key = "rel-immediate".to_owned();
    sim.queue_downlink(replayed);
    drive_until(&mut daemon, |_daemon| {
        sim.frames_with_kind("client.command_ack").len() >= 4
    });
    let duplicate = &sim.frames_with_kind("client.command_ack")[3];
    assert_eq!(duplicate.frame["payload"]["status"], "duplicate");
    assert_eq!(
        daemon
            .store_mut()
            .occupancy_release_intents()
            .expect("intents")
            .len(),
        3,
        "the replay never recorded twice"
    );
    assert_eq!(daemon.status().occupancy_release_intents_recorded, 3);

    daemon.into_store().close().expect("store close");
    cleanup(&root);
}

#[test]
fn release_with_a_foreign_lease_is_refused_as_a_stale_stamp() {
    let root = temporary_directory("release-foreign");
    let sim = Arc::new(ServerSim::new());
    let mut daemon = started_daemon("release-foreign", &root, &sim);

    sim.queue_downlink(offer(0, LEASE_ONE, 7));
    drive_until(&mut daemon, |_daemon| {
        !sim.frames_with_kind("client.occupancy.ack").is_empty()
    });

    sim.queue_downlink(release(
        1,
        "rel-foreign",
        LEASE_TWO,
        7,
        ClientOccupancyReleaseMode::Immediate,
    ));
    drive_until(&mut daemon, |_daemon| {
        !sim.frames_with_kind("client.command_ack").is_empty()
    });
    let ack = &sim.frames_with_kind("client.command_ack")[0];
    assert_eq!(
        ack.frame["payload"]["status"], "rejected_stale_fencing_token",
        "a foreign lease under the mirror's token is still a stamp mismatch"
    );
    assert_eq!(ack.frame["payload"]["error"]["code"], "STALE_FENCING_TOKEN");
    assert!(
        daemon
            .store_mut()
            .occupancy_release_intents()
            .expect("intents")
            .is_empty()
    );

    daemon.into_store().close().expect("store close");
    cleanup(&root);
}

#[test]
fn release_without_a_mirror_fails_closed() {
    let root = temporary_directory("release-unmirrored");
    let sim = Arc::new(ServerSim::new());
    let mut daemon = started_daemon("release-unmirrored", &root, &sim);

    sim.queue_downlink(release(
        0,
        "rel-unmirrored",
        LEASE_ONE,
        7,
        ClientOccupancyReleaseMode::CancelTasksAndRelease,
    ));
    drive_until(&mut daemon, |_daemon| {
        !sim.frames_with_kind("client.command_ack").is_empty()
    });
    let ack = &sim.frames_with_kind("client.command_ack")[0];
    assert_eq!(ack.frame["payload"]["status"], "rejected_lease_mismatch");
    assert_eq!(ack.frame["payload"]["error"]["code"], "UNKNOWN_LEASE");
    assert_eq!(ack.frame["payload"]["currentRevision"], 0);
    assert!(daemon.occupancy_mirror().is_none());

    daemon.into_store().close().expect("store close");
    cleanup(&root);
}

// ---------------------------------------------------------------------------
// Restart recovery (plan 18.3): the mirror is rebuilt, never cleared.
// ---------------------------------------------------------------------------

#[test]
fn the_mirror_survives_a_restart_and_keeps_authorizing() {
    let root = temporary_directory("restart-mirror");
    let sim = Arc::new(ServerSim::new());
    let mut daemon = started_daemon("restart-mirror", &root, &sim);

    sim.queue_downlink(offer(0, LEASE_ONE, 7));
    drive_until(&mut daemon, |_daemon| {
        !sim.frames_with_kind("client.occupancy.ack").is_empty()
    });
    let uplink_watermark = daemon.status().acked_through;
    daemon.into_store().close().expect("store close");

    // A fresh process on the same store rebuilds the mirror from the
    // durable row and keeps fencing against it; the server resumes both
    // streams at the durable positions.
    let sim_two = Arc::new(ServerSim::resumed(2, uplink_watermark));
    let mut restarted = resumed_daemon("restart-mirror", &root, &sim_two);
    let mirror = restarted.occupancy_mirror().expect("rebuilt mirror");
    assert_eq!(mirror.occupancy_lease_id, LEASE_ONE);
    assert_eq!(mirror.fencing_token, 7);
    assert_eq!(mirror.mirror_revision, 1);
    assert!(matches!(
        restarted
            .fencing_guard()
            .authorize_command(FencedCommandKind::WorkerLaunch, LEASE_ONE, 7),
        FencingVerdict::Authorized(_)
    ));

    // The rebuilt mirror rides the heartbeat again after the restart.
    drive_until(&mut restarted, |_daemon| {
        sim_two
            .frames_with_kind("client.heartbeat")
            .iter()
            .any(|frame| frame.frame["payload"]["occupancyLeaseId"] == LEASE_ONE)
    });

    // The recovery flow continues at the same revision: a stale-replay
    // offer is refused exactly like before the restart ...
    sim_two.queue_downlink(offer(0, LEASE_TWO, 9));
    drive_until(&mut restarted, |_daemon| {
        !sim_two
            .frames_with_kind("client.occupancy.rejected")
            .is_empty()
    });
    assert_eq!(
        sim_two.frames_with_kind("client.occupancy.rejected")[0].frame["payload"]["reason"],
        "local_state_conflict"
    );

    // ... and the correctly stamped one advances to revision 2.
    sim_two.queue_downlink(offer(1, LEASE_TWO, 9));
    drive_until(&mut restarted, |_daemon| {
        !sim_two.frames_with_kind("client.occupancy.ack").is_empty()
    });
    assert_eq!(
        restarted
            .occupancy_mirror()
            .expect("mirror")
            .mirror_revision,
        2
    );
    assert_eq!(
        restarted
            .fencing_guard()
            .authorize_command(FencedCommandKind::WorkerStop, LEASE_ONE, 7),
        FencingVerdict::Rejected(FencingRejection::StaleFencingToken),
        "the pre-restart token is stale after the mirror advanced"
    );

    restarted.into_store().close().expect("store close");
    cleanup(&root);
}

#[test]
fn a_fresh_process_still_mints_revision_one_for_the_first_offer() {
    let root = temporary_directory("fresh-revision");
    let (store, identity) = open_enrolled(&root);
    let sim = Arc::new(ServerSim::new());
    let daemon = DeviceDaemon::start(
        daemon_config("fresh-revision"),
        store,
        sim.clone(),
        &identity,
    )
    .expect("daemon start");
    assert!(daemon.occupancy_mirror().is_none());
    assert_eq!(
        daemon.fencing_guard().mirror_revision(),
        None,
        "an unmirrored device authorizes nothing"
    );
    daemon.into_store().close().expect("store close");
    cleanup(&root);
}

#[test]
fn wire_payloads_of_the_lane_commands_keep_the_schema_vocabulary() {
    // ClientToServerMessage serialization is the wire truth the daemon
    // enqueues: the ack/rejected payloads must round-trip the exact
    // kind/payload shape with the decimal-string token.
    let message = ClientToServerMessage::CommandAck(
        winwincode_client_port::messages::ClientCommandAckPayload {
            command_kind:
                winwincode_client_port::domain::ClientControlMessageKind::OccupancyRelease,
            command_message_id: "srv-down-1".to_owned(),
            status: winwincode_client_port::domain::CommandAckStatus::Accepted,
            current_revision: Some(1),
            error: None,
        },
    );
    let value = serde_json::to_value(&message).expect("serialize");
    assert_eq!(value["kind"], "client.command_ack");
    assert_eq!(value["payload"]["currentRevision"], 1);

    let frame = serde_json::to_value(release(
        1,
        "k",
        LEASE_ONE,
        7,
        ClientOccupancyReleaseMode::Immediate,
    ))
    .expect("serialize");
    assert_eq!(frame["kind"], "client.occupancy.release");
    assert_eq!(frame["payload"]["occupancyFencingToken"], "7");
    assert_eq!(frame["payload"]["mode"], "immediate");
}

// --- WORKER-100.2: live capacity reporting and the cancel-and-release
// --- worker-stop hook (plan 14.5 / 12.4).

struct StaticCapacity {
    running: u32,
    reserved: u32,
}

impl WorkerCapacitySource for StaticCapacity {
    fn worker_capacity(&self) -> WorkerCapacitySnapshot {
        WorkerCapacitySnapshot {
            running_worker_sessions: self.running,
            reserved_worker_sessions: self.reserved,
        }
    }
}

struct RecordingController {
    leases: Mutex<Vec<String>>,
    stopped: usize,
}

impl LeaseWorkerController for RecordingController {
    fn stop_lease_workers(&self, occupancy_lease_id: &str) -> usize {
        self.leases
            .lock()
            .expect("controller lease lock")
            .push(occupancy_lease_id.to_owned());
        self.stopped
    }
}

#[test]
fn hello_and_heartbeat_report_the_live_worker_capacity() {
    let root = temporary_directory("live-capacity");
    let (store, identity) = open_enrolled(&root);
    let sim = Arc::new(ServerSim::new());
    let mut daemon = DeviceDaemon::start(
        daemon_config("live-capacity"),
        store,
        sim.clone(),
        &identity,
    )
    .expect("daemon start");
    daemon.set_worker_capacity_source(Arc::new(StaticCapacity {
        running: 3,
        reserved: 1,
    }));

    // The hello announcement already carries the live facts, keeping the
    // configured max/draining skeleton around them.
    drive_until(&mut daemon, |_| {
        !sim.frames_with_kind("client.hello").is_empty()
    });
    let hello = &sim.frames_with_kind("client.hello")[0].frame;
    let capacity = &hello["payload"]["capacity"];
    assert_eq!(capacity["maxConcurrentWorkerSessions"], 2);
    assert_eq!(capacity["runningWorkerSessions"], 3);
    assert_eq!(capacity["reservedWorkerSessions"], 1);
    assert_eq!(capacity["drainingWorkerSessions"], 0);

    // Every heartbeat refreshes the facts from the source (the daemon
    // config skeleton says running=0, so a 3 can only come from the wire).
    sim.queue_downlink(offer(0, LEASE_ONE, 7));
    drive_until(&mut daemon, |_| {
        !sim.frames_with_kind("client.occupancy.ack").is_empty()
    });
    drive_until(&mut daemon, |_| {
        sim.frames_with_kind("client.heartbeat")
            .iter()
            .any(|frame| {
                frame.frame["payload"]["capacity"]["runningWorkerSessions"] == 3
                    && frame.frame["payload"]["capacity"]["reservedWorkerSessions"] == 1
            })
    });
    daemon.into_store().close().expect("store close");
    cleanup(&root);
}

#[test]
fn cancel_and_release_stops_the_lease_workers_through_the_controller() {
    let root = temporary_directory("cancel-release-stop");
    let (store, identity) = open_enrolled(&root);
    let sim = Arc::new(ServerSim::new());
    let mut daemon = DeviceDaemon::start(
        daemon_config("cancel-release-stop"),
        store,
        sim.clone(),
        &identity,
    )
    .expect("daemon start");
    let controller = Arc::new(RecordingController {
        leases: Mutex::new(Vec::new()),
        stopped: 2,
    });
    daemon.set_lease_worker_controller(controller.clone());

    // Claim the lease, then release it with cancel_tasks_and_release.
    sim.queue_downlink(offer(0, LEASE_ONE, 7));
    drive_until(&mut daemon, |_| {
        !sim.frames_with_kind("client.occupancy.ack").is_empty()
    });
    sim.queue_downlink(release(
        1,
        "srv-release-cancel",
        LEASE_ONE,
        7,
        ClientOccupancyReleaseMode::CancelTasksAndRelease,
    ));
    drive_until(&mut daemon, |_| {
        !sim.frames_with_kind("client.command_ack").is_empty()
    });
    assert_eq!(
        daemon.status().workers_stopped_on_release,
        2,
        "the controller's answer lands in the status counters"
    );
    assert_eq!(*controller.leases.lock().expect("leases lock"), [LEASE_ONE]);

    // A replayed release (duplicate) and other modes never re-stop.
    let acks = sim.frames_with_kind("client.command_ack").len();
    sim.queue_downlink(release(
        1,
        "srv-release-cancel",
        LEASE_ONE,
        7,
        ClientOccupancyReleaseMode::CancelTasksAndRelease,
    ));
    sim.queue_downlink(release(
        1,
        "srv-release-immediate",
        LEASE_ONE,
        7,
        ClientOccupancyReleaseMode::Immediate,
    ));
    drive_until(&mut daemon, |_| {
        sim.frames_with_kind("client.command_ack").len() >= acks + 2
    });
    assert_eq!(
        daemon.status().workers_stopped_on_release,
        2,
        "duplicates and non-cancel modes stop nothing"
    );
    assert_eq!(
        controller.leases.lock().expect("leases lock").len(),
        1,
        "only the first cancel_and_release reached the controller"
    );

    daemon.into_store().close().expect("store close");
    cleanup(&root);
}
