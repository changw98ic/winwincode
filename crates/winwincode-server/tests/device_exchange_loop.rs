// SPDX-License-Identifier: Apache-2.0

//! The device exchange vertical over real HTTP: the device-client daemon
//! runs against a real server (`POST /internal/v1/client/exchange` wired to
//! `ClientExchangeApplication`) through the std HTTP transport, covering the
//! whole enrollment lifecycle — enroll with credential issuance, hello
//! takeover, heartbeat projection, a staged outage with backoff and
//! redelivery, a manufactured gap answered by `replayFromSequence`, and
//! durable recovery across a device restart and a server restart. The
//! assertions land on both sides: the server registry projection and the
//! device store.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use winwincode_api::generated::{OrganizationScope, OrganizationScopeKind, Scope};
use winwincode_client_port::domain::{
    ClientArchitecture, ClientCapacityReport, ClientLockState, ClientPlatformTarget, PresenceState,
};
use winwincode_client_port::messages::{ClientHeartbeatPayload, ClientToServerMessage};
use winwincode_control_plane::ClientRegistryService;
use winwincode_device_client::{
    DaemonConfig, DeviceDaemon, DeviceIdentitySeed, DeviceStore, ExchangeTransport,
    ExchangeTransportError, HttpExchangeTransport, TickOutcome, ensure_device_identity,
};
use winwincode_domain::OrganizationId;
use winwincode_server::{
    ApiError, AuthSessionBootstrap, AuthSessionConfig, AuthenticatedPrincipal,
    ClientExchangeApplication, ClientExchangeConfig, ClientExchangePort, ControlPlaneApiPort,
    EventSubscription, RequestAuthenticator, ServerConfig, ServerTls, SqliteAuthSessionManager,
    UserAccountService, start_server_with_remote_worker,
};
use winwincode_storage::{
    ClientExchangeCursors, ClientNodeRecord, ClientPresenceState, SqliteStorage,
};

const BOOTSTRAP_PROOF: &str = "device-exchange-loop-test-bootstrap";
/// Quiet heartbeat cadence: every wire frame in this vertical is staged by
/// the test, keeping the frame ledger deterministic.
const QUIET_HEARTBEAT: Duration = Duration::from_mins(2);
const DRIVE_DEADLINE: Duration = Duration::from_secs(20);

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
static TEST_RUN_NAMESPACE: OnceLock<String> = OnceLock::new();

fn test_directory(label: &str) -> PathBuf {
    let namespace = TEST_RUN_NAMESPACE.get_or_init(|| {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce).expect("test run namespace entropy");
        let mut encoded = String::with_capacity(nonce.len() * 2);
        for byte in nonce {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        format!("{}-{encoded}", std::process::id())
    });
    let id = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{label}-{namespace}-{id}"))
}

fn seed() -> DeviceIdentitySeed {
    DeviceIdentitySeed {
        display_name: "Cheng's MacBook".to_owned(),
        platform: "darwin".to_owned(),
        architecture: "arm64".to_owned(),
        client_version: "0.1.0-alpha.1".to_owned(),
    }
}

fn capacity() -> ClientCapacityReport {
    ClientCapacityReport {
        max_concurrent_worker_sessions: 4,
        running_worker_sessions: 0,
        reserved_worker_sessions: 0,
        draining_worker_sessions: 0,
    }
}

fn heartbeat_message() -> ClientToServerMessage {
    ClientToServerMessage::Heartbeat(ClientHeartbeatPayload {
        capacity: capacity(),
        accepting_connections: true,
        lock_state: ClientLockState::Unlocked,
        presence_state: PresenceState::Online,
        occupancy_lease_id: None,
    })
}

fn daemon_config(endpoint: &str) -> DaemonConfig {
    DaemonConfig {
        server_profile_id: "loop-server".to_owned(),
        base_url: endpoint.to_owned(),
        server_display_name: "WinWinCode Control Plane".to_owned(),
        device_display_name: "Cheng's MacBook".to_owned(),
        platform: ClientPlatformTarget::Aarch64AppleDarwin,
        architecture: ClientArchitecture::Aarch64,
        client_version: "0.1.0-alpha.1".to_owned(),
        heartbeat_interval: QUIET_HEARTBEAT,
        enroll_poll_interval: Duration::from_millis(5),
        max_frames_per_exchange: 8,
        initial_backoff: Duration::from_millis(5),
        max_backoff: Duration::from_millis(200),
        capacity: capacity(),
    }
}

#[derive(Default)]
struct NoopApi;

impl ControlPlaneApiPort for NoopApi {
    fn command(
        &self,
        _: &AuthenticatedPrincipal,
        _: serde_json::Value,
    ) -> Result<serde_json::Value, ApiError> {
        Err(ApiError::new(
            501,
            "NOT_IMPLEMENTED",
            "unused in the device exchange loop test",
        ))
    }

    fn query(
        &self,
        _: &AuthenticatedPrincipal,
        _: serde_json::Value,
    ) -> Result<serde_json::Value, ApiError> {
        Err(ApiError::new(
            501,
            "NOT_IMPLEMENTED",
            "unused in the device exchange loop test",
        ))
    }

    fn subscribe(
        &self,
        _: &AuthenticatedPrincipal,
        first_frame: serde_json::Value,
    ) -> Result<EventSubscription, ApiError> {
        let (_, receiver) = mpsc::channel(1);
        Ok(EventSubscription {
            initial_frames: vec![first_frame],
            events: receiver,
        })
    }

    fn event_control(
        &self,
        _: &AuthenticatedPrincipal,
        frame: serde_json::Value,
    ) -> Result<Vec<serde_json::Value>, ApiError> {
        Ok(vec![frame])
    }

    fn shutdown(&self) -> Result<(), ApiError> {
        Ok(())
    }
}

fn server_config(data_directory: &Path) -> ServerConfig {
    ServerConfig::new(
        "127.0.0.1:0".parse().expect("loopback address"),
        "http://control.example",
        ServerTls::Disabled,
        BTreeSet::from(["https://client.example".to_owned()]),
        data_directory.to_path_buf(),
        Duration::from_secs(2),
    )
    .expect("valid config")
}

fn auth_sessions() -> Arc<SqliteAuthSessionManager> {
    let scopes = vec![Scope::OrganizationScope(OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: OrganizationId("org_00000000000000000000000001".to_owned()),
    })];
    let accounts = Arc::new(
        UserAccountService::open(test_directory("device-loop-auth")).expect("account service"),
    );
    Arc::new(
        SqliteAuthSessionManager::open(
            test_directory("device-loop-auth"),
            vec![AuthSessionBootstrap::new(BOOTSTRAP_PROOF).expect("proof")],
            scopes,
            AuthSessionConfig::default(),
            Arc::clone(&accounts),
            None,
        )
        .expect("auth session manager"),
    )
}

async fn start_with_client_exchange(data_directory: &Path) -> winwincode_server::RunningServer {
    let exchange: Arc<dyn ClientExchangePort> = Arc::new(
        ClientExchangeApplication::open(data_directory, &ClientExchangeConfig::default())
            .expect("valid client exchange application"),
    );
    let sessions = auth_sessions();
    let authenticator: Arc<dyn RequestAuthenticator> = sessions.clone();
    start_server_with_remote_worker(
        server_config(data_directory),
        sessions,
        authenticator,
        Arc::new(NoopApi),
        None,
        None,
        Some(exchange),
    )
    .await
    .expect("start server with client exchange")
}

fn exchange_endpoint(address: std::net::SocketAddr) -> String {
    format!("http://{address}/internal/v1/client/exchange")
}

/// The device-side transport for this vertical: the real std HTTP transport,
/// plus two staged faults the loop test controls — a connection-refused
/// outage (frames stay durable and back off) and a one-shot dropped frame
/// that manufactures a server-side gap.
struct LoopTransport {
    http: Arc<HttpExchangeTransport>,
    outage_remaining: AtomicUsize,
    drop_sequence: AtomicU64,
}

impl LoopTransport {
    fn new(address: std::net::SocketAddr) -> Arc<Self> {
        Arc::new(Self {
            http: Arc::new(HttpExchangeTransport::new(exchange_endpoint(address))),
            outage_remaining: AtomicUsize::new(0),
            drop_sequence: AtomicU64::new(0),
        })
    }
}

impl ExchangeTransport for LoopTransport {
    fn exchange(
        &self,
        credential: Option<&str>,
        request_bytes: &[u8],
    ) -> Result<Vec<u8>, ExchangeTransportError> {
        if self.outage_remaining.load(Ordering::SeqCst) > 0 {
            self.outage_remaining.fetch_sub(1, Ordering::SeqCst);
            return Err(ExchangeTransportError::new(
                "staged outage: connection refused",
            ));
        }
        let mut body = request_bytes.to_vec();
        let drop = self.drop_sequence.swap(0, Ordering::SeqCst);
        if drop > 0 {
            let mut request: serde_json::Value = serde_json::from_slice(&body)
                .map_err(|error| ExchangeTransportError::new(format!("staged drop: {error}")))?;
            if let Some(frames) = request
                .get_mut("frames")
                .and_then(serde_json::Value::as_array_mut)
            {
                frames.retain(|frame| {
                    frame.get("sequence").and_then(serde_json::Value::as_u64) != Some(drop)
                });
            }
            body = serde_json::to_vec(&request)
                .map_err(|error| ExchangeTransportError::new(format!("staged drop: {error}")))?;
        }
        self.http.exchange(credential, &body)
    }
}

/// Drives the daemon loop until `predicate` holds, sleeping only the
/// durations the loop schedules.
fn drive_until(
    daemon: &mut DeviceDaemon,
    what: &str,
    mut predicate: impl FnMut(&mut DeviceDaemon) -> bool,
) {
    let deadline = Instant::now() + DRIVE_DEADLINE;
    while Instant::now() < deadline {
        if predicate(daemon) {
            return;
        }
        match daemon.tick(Instant::now()) {
            Ok(
                TickOutcome::Waiting { ready_in }
                | TickOutcome::Retrying {
                    after: ready_in, ..
                },
            ) => {
                std::thread::sleep(ready_in.min(Duration::from_millis(20)));
            }
            Ok(TickOutcome::Exchanged { .. }) => std::thread::sleep(Duration::from_millis(2)),
            Err(error) => panic!("the daemon failed during {what}: {error:?}"),
        }
    }
    panic!("timed out waiting for {what}");
}

fn settled(daemon: &mut DeviceDaemon) -> bool {
    let snapshot = daemon.outbox_snapshot().expect("outbox snapshot");
    snapshot.frames.is_empty() && snapshot.ack_sequence == snapshot.highest_sequence
}

fn node_snapshot(data_directory: &Path, node_id: &str) -> ClientNodeRecord {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut registry = ClientRegistryService::new(&mut storage);
    registry
        .snapshot(node_id)
        .expect("registry read")
        .unwrap_or_else(|| panic!("node {node_id} must exist in the registry"))
}

fn cursors(data_directory: &Path, node_id: &str) -> ClientExchangeCursors {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut registry = ClientRegistryService::new(&mut storage);
    registry
        .exchange_cursors(node_id)
        .expect("cursor read")
        .unwrap_or_else(|| panic!("cursors of {node_id} must exist"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn the_device_daemon_runs_the_full_exchange_lifecycle_over_http() {
    let data_directory = test_directory("device-loop-server");
    let device_root = test_directory("device-loop-device");
    let running = start_with_client_exchange(&data_directory).await;
    let address = running.local_address();

    // ---- Device first boot -------------------------------------------------
    let mut store = DeviceStore::open(&device_root).expect("device store should open");
    let first_identity = ensure_device_identity(&mut store, &seed(), "2026-09-04T00:00:00.000Z")
        .expect("device identity should load");
    assert!(
        first_identity.current_instance_id().starts_with("cix_")
            && first_identity.current_instance_id().len() == 30,
        "the launch instance must be a canonical cix_ identity"
    );
    assert_eq!(first_identity.identity().client_node_id(), "");

    let endpoint = exchange_endpoint(address);
    let transport = LoopTransport::new(address);
    let config = daemon_config(&endpoint);
    let mut daemon = DeviceDaemon::start(
        config.clone(),
        store,
        Arc::clone(&transport) as Arc<dyn ExchangeTransport>,
        &first_identity,
    )
    .expect("daemon start");

    // ---- Phase 1: enroll over real HTTP ------------------------------------
    drive_until(&mut daemon, "the enrollment adoption", |daemon| {
        daemon.is_enrolled()
    });
    let node_id = daemon.client_node_id().to_owned();
    assert!(
        node_id.starts_with("cnd_") && node_id.len() == 30,
        "the daemon must adopt the server-assigned cnd_ identity: {node_id}"
    );

    // Server side: the node was created with the issued credential and the
    // enrolling instance.
    let record = node_snapshot(&data_directory, &node_id);
    assert_eq!(
        record.presence_state,
        ClientPresenceState::PendingEnrollment
    );
    assert_eq!(
        record.current_instance_id.as_deref(),
        Some(first_identity.current_instance_id())
    );
    let issued_digest = record
        .device_credential_digest
        .clone()
        .expect("the enrollment issues a persisted credential digest");
    let adopted_public_client_id = record.public_client_id.clone();
    // Device side: the enrollment persisted the server profile, the downlink
    // cursor accepted the acceptance frame, and the assigned stream was
    // credited with the settled enroll sequence.
    let profile = daemon
        .store_mut()
        .server_profile(&config.server_profile_id)
        .expect("profile read")
        .expect("the adoption must persist the server profile");
    assert_eq!(profile.base_url, endpoint);
    let inbox_cursor = daemon
        .store_mut()
        .inbox_cursor(&config.server_profile_id)
        .expect("cursor read")
        .expect("the acceptance frame advanced the inbox cursor");
    assert_eq!(inbox_cursor.last_sequence, 1);
    assert_eq!(
        cursors(&data_directory, &node_id).client_to_server_ack_sequence,
        1,
        "the assigned stream starts credited with the enroll sequence"
    );

    // ---- Phase 2: hello takeover and heartbeat projection ------------------
    // The hello is announced automatically after the adoption; it must
    // settle first (it moves the node online), then two staged heartbeats
    // drive the projection.
    drive_until(&mut daemon, "the announcement hello to settle", |daemon| {
        daemon.is_enrolled() && daemon.status().frames_sent >= 2
    });
    daemon
        .enqueue(heartbeat_message())
        .expect("enqueue heartbeat");
    daemon
        .enqueue(heartbeat_message())
        .expect("enqueue heartbeat");
    drive_until(&mut daemon, "the heartbeats to settle", |daemon| {
        daemon.status().acked_through >= 4
    });
    let record = node_snapshot(&data_directory, &node_id);
    assert_eq!(record.presence_state, ClientPresenceState::Online);
    assert!(
        record.last_heartbeat_at.is_some(),
        "the heartbeat projection must be recorded"
    );
    assert_eq!(
        cursors(&data_directory, &node_id).client_to_server_ack_sequence,
        4
    );
    assert!(settled(&mut daemon), "the stream must be fully exchanged");

    // ---- Phase 3: staged outage, backoff, and durable redelivery -----------
    transport.outage_remaining.store(3, Ordering::SeqCst);
    daemon
        .enqueue(heartbeat_message())
        .expect("enqueue heartbeat");
    daemon
        .enqueue(heartbeat_message())
        .expect("enqueue heartbeat");
    drive_until(&mut daemon, "the outage to recover", |daemon| {
        settled(daemon) && daemon.status().consecutive_failures == 0
    });
    let status = daemon.status().clone();
    assert!(
        status.last_error.is_some(),
        "the staged outage must be visible: {status:?}"
    );
    assert_eq!(status.replays, 0, "an outage is a redelivery, not a gap");
    assert_eq!(
        cursors(&data_directory, &node_id).client_to_server_ack_sequence,
        6,
        "the durable frames survived the outage and settled once"
    );

    // ---- Phase 4: manufactured gap and replayFromSequence replay -----------
    for _ in 0..3 {
        daemon
            .enqueue(heartbeat_message())
            .expect("enqueue heartbeat");
    }
    let snapshot = daemon.outbox_snapshot().expect("outbox snapshot");
    let staged_sequences = snapshot
        .frames
        .iter()
        .map(|frame| frame.sequence)
        .collect::<Vec<_>>();
    assert_eq!(staged_sequences, [7, 8, 9]);
    // The middle frame is lost in transit: the server cursor sits at 7 while
    // sequence 9 arrives, so the response carries replayFromSequence = 8.
    transport.drop_sequence.store(8, Ordering::SeqCst);
    drive_until(&mut daemon, "the manufactured gap", |daemon| {
        daemon.status().replays >= 1
    });
    drive_until(&mut daemon, "the gap replay to settle", |daemon| {
        settled(daemon) && daemon.status().acked_through >= 9
    });
    let status = daemon.status().clone();
    assert_eq!(status.replays, 1);
    assert_eq!(status.acked_through, 9);
    assert_eq!(
        cursors(&data_directory, &node_id).client_to_server_ack_sequence,
        9
    );
    let record = node_snapshot(&data_directory, &node_id);
    assert!(
        record.last_heartbeat_at.is_some(),
        "the replayed heartbeats execute their projection exactly once"
    );

    // ---- Phase 5: device restart from durable state -------------------------
    // Three frames stay pending (never exchanged): the crash shape.
    for _ in 0..3 {
        daemon
            .enqueue(heartbeat_message())
            .expect("enqueue heartbeat");
    }
    assert_eq!(
        daemon
            .outbox_snapshot()
            .expect("snapshot")
            .frames
            .iter()
            .map(|frame| frame.sequence)
            .collect::<Vec<_>>(),
        [10, 11, 12]
    );
    daemon.into_store().close().expect("crash close");

    let mut store = DeviceStore::open(&device_root).expect("restarted device store");
    let second_identity = ensure_device_identity(&mut store, &seed(), "2026-09-04T01:00:00.000Z")
        .expect("restarted identity");
    // Identity 不变: the adopted identity and the issued credential survive;
    // only the launch instance rotates.
    assert_eq!(
        second_identity.identity().device_id(),
        first_identity.identity().device_id()
    );
    assert_eq!(second_identity.identity().client_node_id(), node_id);
    assert_eq!(
        second_identity.identity().public_client_id(),
        adopted_public_client_id,
        "the server-issued publicClientId survives the restart"
    );
    assert_eq!(
        adopted_public_client_id.len(),
        10,
        "the issued publicClientId is the canonical 10-digit form"
    );
    assert_eq!(
        second_identity.credential().digest(),
        issued_digest,
        "the issued Device Credential survives the restart"
    );
    assert_ne!(
        second_identity.current_instance_id(),
        first_identity.current_instance_id()
    );

    let mut daemon = DeviceDaemon::start(
        config.clone(),
        store,
        Arc::clone(&transport) as Arc<dyn ExchangeTransport>,
        &second_identity,
    )
    .expect("restarted daemon");
    assert!(
        daemon.is_enrolled(),
        "the enrolled phase must restore from the durable identity before any exchange"
    );
    assert_eq!(
        daemon.client_node_id(),
        node_id,
        "the restarted daemon exchanges under the assigned node id"
    );
    // Outbox 不丢帧 + cursor 连续: the pending frames and the announcement
    // hello settle contiguously through the instance takeover.
    drive_until(&mut daemon, "the restarted daemon to settle", |daemon| {
        settled(daemon) && daemon.status().acked_through >= 13
    });
    assert_eq!(daemon.status().acked_through, 13);
    assert_eq!(
        cursors(&data_directory, &node_id).client_to_server_ack_sequence,
        13
    );
    let record = node_snapshot(&data_directory, &node_id);
    assert_eq!(
        record.current_instance_id.as_deref(),
        Some(second_identity.current_instance_id()),
        "the announcement hello took the launch instance over"
    );

    // ---- Phase 6: server restart over the same durable state ---------------
    running.shutdown().await.expect("server shutdown");
    let restarted_server = start_with_client_exchange(&data_directory).await;
    transport
        .http
        .set_endpoint(exchange_endpoint(restarted_server.local_address()));

    daemon
        .enqueue(heartbeat_message())
        .expect("enqueue heartbeat");
    daemon
        .enqueue(heartbeat_message())
        .expect("enqueue heartbeat");
    drive_until(
        &mut daemon,
        "the exchanges after the server restart to settle",
        |daemon| settled(daemon) && daemon.status().acked_through >= 15,
    );
    let status = daemon.status().clone();
    // The fresh session started at the device restart without any replay;
    // the server restart must not add one (both cursors are durable).
    assert_eq!(status.replays, 0, "a server restart replays nothing");
    assert_eq!(status.consecutive_failures, 0);
    assert_eq!(status.acked_through, 15);
    let server_cursors = cursors(&data_directory, &node_id);
    assert_eq!(server_cursors.client_to_server_ack_sequence, 15);
    assert_eq!(
        server_cursors.server_to_client_ack_sequence, 1,
        "the downlink cursor is durable across the server restart"
    );
    let record = node_snapshot(&data_directory, &node_id);
    assert_eq!(record.presence_state, ClientPresenceState::Online);
    assert!(record.last_heartbeat_at.is_some());

    // Both sides agree at rest: the daemon's durable cursor equals the
    // registry projection cursor, and the outbox retains nothing.
    let snapshot = daemon.outbox_snapshot().expect("final snapshot");
    assert!(snapshot.frames.is_empty());
    assert_eq!(snapshot.ack_sequence, snapshot.highest_sequence);
    assert_eq!(
        snapshot.ack_sequence,
        server_cursors.client_to_server_ack_sequence
    );

    daemon.into_store().close().expect("store close");
    restarted_server.shutdown().await.expect("server shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&device_root);
}
