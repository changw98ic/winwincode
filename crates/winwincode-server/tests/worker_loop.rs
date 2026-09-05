// SPDX-License-Identifier: Apache-2.0

//! The worker loop vertical (WORKER-200.2): the full worker chain over a
//! real server and a real Device Client daemon — no fake responder on the
//! device side. The daemon enrolls over the real HTTP exchange, occupies
//! the device (claim → offer → ack → `occupied`), and then answers the
//! `client.worker.launch` command the `POST /api/v1/sessions` flow enqueues:
//! the fencing stamp passes, the launch material (the one-time worker
//! credential the `201` launch response delivered exactly once) is matched
//! against the grant digest, the wired `SessionSupervisor` writes the 0600
//! credential/config files and spawns a stub worker process (field-checking
//! script: it verifies every managed-session contract field before idling
//! and exiting zero on SIGTERM — the real `--managed-session` parser is
//! cross-validated against supervisor-written configs by the device-client
//! suite), and the `client.worker.launch_ack` consumes the grant exactly
//! once. The vertical then pins the surrounding behaviors: the heartbeat
//! running count following the registry, non-holder rejection, the
//! idempotent replay of an accepted acknowledgement, the supervisor stop
//! path, `cancel_and_release` stopping the lease workers, and a stale
//! launch after a force fence being refused by the daemon's mirror.
//!
//! Assertions land on both durable sides throughout: the server's launch
//! grant ledger and registry projection, and the device store's worker
//! process registry, occupancy mirror, release intents, and exchange
//! cursors.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant as StdInstant;

use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use winwincode_api::generated::{OrganizationScope, OrganizationScopeKind, Scope};
use winwincode_client_port::domain::ClientArchitecture;
use winwincode_client_port::domain::ClientCapacityReport;
use winwincode_client_port::domain::ClientOccupancyReleaseMode;
use winwincode_client_port::domain::ClientPlatformTarget;
use winwincode_client_port::domain::WorkerLaunchAckStatus;
use winwincode_client_port::domain::WorkerLaunchGrant;
use winwincode_client_port::domain::WorkerLaunchGrantState as WireGrantState;
use winwincode_client_port::messages::CLIENT_CONTROL_PORT_SCHEMA_VERSION;
use winwincode_client_port::messages::ClientToServerEnvelope;
use winwincode_client_port::messages::ClientToServerMessage;
use winwincode_client_port::messages::ClientWorkerLaunchAckPayload;
use winwincode_client_port::messages::CommandContext;
use winwincode_client_port::messages::OccupancyCommandContext;
use winwincode_client_port::messages::ServerToClientEnvelope;
use winwincode_client_port::messages::ServerToClientMessage;
use winwincode_client_port::messages::ServerWorkerLaunchPayload;
use winwincode_control_plane::ClientOccupancyService;
use winwincode_control_plane::ClientRegistryService;
use winwincode_control_plane::ConnectCodeService;
use winwincode_control_plane::WorkerLaunchGrantService;
use winwincode_device_client::{
    DaemonConfig, DeviceDaemon, DeviceIdentitySeed, DeviceStore, HttpExchangeTransport,
    SessionSupervisor, SupervisorConfig, TickOutcome, WORKER_STATE_EXITED, WORKER_STATE_RUNNING,
    WorkerLaunchDirectories, WorkerLaunchMaterialSource, ensure_device_identity,
    load_device_identity,
};
use winwincode_domain::Instant;
use winwincode_domain::OrganizationId;
use winwincode_server::{
    ApiError, AuthSessionBootstrap, AuthSessionConfig, AuthenticatedPrincipal,
    ClientExchangeApplication, ClientExchangeConfig, ClientExchangePort,
    ClientOccupancyApplication, ClientOccupancyConfig, ControlPlaneApiPort, EventSubscription,
    RequestAuthenticator, ServerConfig, ServerTls, SqliteAuthSessionManager, UserAccountService,
    start_server_with_remote_worker,
};
use winwincode_storage::{
    AccessGrantIssuance, ClientDownlinkAppend, ConnectCodeConsume, ConnectCodePublication,
    GrantTrustMode, OccupancyLeaseState, RepositoryAccessGrantIssuance, RepositoryAvailability,
    RepositoryBindingProjection, RepositoryDirtyState, RepositoryGrantPermissions, SqliteStorage,
    WorkerLaunchGrantState,
};

const BOOTSTRAP_PROOF: &str = "worker-loop-test-bootstrap";
const ORIGIN: &str = "https://client.example";
const OWNER_PASSWORD: &str = "initial-owner-password";
const SCHEMA_VERSION: &str = "winwincode/v1";
const VALID_UNTIL: &str = "2100-01-01T00:00:00.000Z";
const EXCHANGE_ENDPOINT_PATH: &str = "/internal/v1/client/exchange";
/// The enrollment acceptance pins this server-requested heartbeat cadence,
/// so the real daemon completes launches and drains in milliseconds.
const SERVER_HEARTBEAT_MS: u32 = 200;
const DRIVE_DEADLINE: Duration = Duration::from_secs(30);
/// The recovery window of the sweep application: short enough that the
/// force-release deadline passes within the test.
const RECOVERY_WINDOW: Duration = Duration::from_millis(50);
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
static NEXT_CODE_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_BINDING: AtomicU64 = AtomicU64::new(1);
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

fn fresh_code_id() -> String {
    format!("cct_{:026}", NEXT_CODE_ID.fetch_add(1, Ordering::Relaxed))
}

fn fresh_binding_id() -> String {
    format!("rbd_{:026}", NEXT_BINDING.fetch_add(1, Ordering::Relaxed))
}

#[derive(Default)]
struct NoopApi;

impl ControlPlaneApiPort for NoopApi {
    fn command(&self, _: &AuthenticatedPrincipal, _: Value) -> Result<Value, ApiError> {
        Err(ApiError::new(
            501,
            "NOT_IMPLEMENTED",
            "unused in the worker loop test",
        ))
    }

    fn query(&self, _: &AuthenticatedPrincipal, _: Value) -> Result<Value, ApiError> {
        Err(ApiError::new(
            501,
            "NOT_IMPLEMENTED",
            "unused in the worker loop test",
        ))
    }

    fn subscribe(
        &self,
        _: &AuthenticatedPrincipal,
        first_frame: Value,
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
        frame: Value,
    ) -> Result<Vec<Value>, ApiError> {
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
        BTreeSet::from([ORIGIN.to_owned()]),
        data_directory.to_path_buf(),
        Duration::from_secs(2),
    )
    .expect("valid config")
}

fn open_auth(directory: &Path) -> Arc<SqliteAuthSessionManager> {
    let scopes = vec![Scope::OrganizationScope(OrganizationScope {
        kind: OrganizationScopeKind::Organization,
        organization_id: OrganizationId("org_00000000000000000000000001".to_owned()),
    })];
    let accounts = Arc::new(
        UserAccountService::open(directory.join("auth-sessions")).expect("account service"),
    );
    Arc::new(
        SqliteAuthSessionManager::open(
            directory.join("auth-sessions"),
            vec![AuthSessionBootstrap::new(BOOTSTRAP_PROOF).expect("proof")],
            scopes,
            AuthSessionConfig::default(),
            accounts,
            None,
        )
        .expect("auth session manager"),
    )
}

async fn start_server(data_directory: &Path) -> winwincode_server::RunningServer {
    let exchange: Arc<dyn ClientExchangePort> = Arc::new(
        ClientExchangeApplication::open(
            data_directory,
            &ClientExchangeConfig {
                heartbeat_interval_ms: SERVER_HEARTBEAT_MS,
                ..ClientExchangeConfig::default()
            },
        )
        .expect("valid client exchange application"),
    );
    let sessions = open_auth(data_directory);
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
    .expect("start server with client surface")
}

// ---- HTTP helpers ----------------------------------------------------------

async fn http_request(address: std::net::SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(address).await.expect("connect server");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read response");
    String::from_utf8(response).expect("HTTP response")
}

async fn post_exchange(
    address: std::net::SocketAddr,
    body: &str,
    credential: Option<&str>,
) -> (String, Option<Value>) {
    let mut stream = TcpStream::connect(address).await.expect("connect server");
    let authorization = credential
        .map(|value| format!("Authorization: Bearer {value}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "POST {EXCHANGE_ENDPOINT_PATH} HTTP/1.1\r\nHost: control.example\r\n{authorization}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read response");
    let response = String::from_utf8(response).expect("HTTP response");
    let status = response.lines().next().expect("status line").to_owned();
    let body = response.split_once("\r\n\r\n").map(|(_, body)| body);
    let parsed = body.and_then(|body| serde_json::from_str(body).ok());
    (status, parsed)
}

fn bearer_post(path: &str, body: &str, proof: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: control.example\r\nOrigin: {ORIGIN}\r\nAuthorization: Bearer {proof}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn plain_post(path: &str, body: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: control.example\r\nOrigin: {ORIGIN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn cookie_post(path: &str, body: &str, cookie: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: control.example\r\nOrigin: {ORIGIN}\r\nCookie: wwc_session={cookie}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn cookie_delete(path: &str, body: &str, cookie: &str) -> String {
    format!(
        "DELETE {path} HTTP/1.1\r\nHost: control.example\r\nOrigin: {ORIGIN}\r\nCookie: wwc_session={cookie}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn status_of(response: &str) -> String {
    response
        .lines()
        .next()
        .expect("status line")
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .to_owned()
}

fn response_body(response: &str) -> Value {
    let (_, body) = response
        .split_once("\r\n\r\n")
        .expect("HTTP response has a body boundary");
    serde_json::from_str(body).expect("HTTP response contains JSON")
}

fn wire_code(response: &str) -> String {
    response_body(response)["error"]["code"]
        .as_str()
        .expect("error code")
        .to_owned()
}

fn session_cookie_from_response(response: &str) -> String {
    let set_cookie = response
        .lines()
        .find_map(|line| line.strip_prefix("set-cookie: "))
        .expect("session Set-Cookie header");
    let pair = set_cookie.split(';').next().expect("cookie pair");
    pair.strip_prefix("wwc_session=")
        .expect("session cookie name")
        .to_owned()
}

fn login_body(username: &str, password: &str) -> String {
    json!({
        "schemaVersion": SCHEMA_VERSION,
        "username": username,
        "password": password,
    })
    .to_string()
}

async fn initialize_and_login_owner(address: std::net::SocketAddr) -> (String, String) {
    let response = http_request(
        address,
        &bearer_post(
            "/api/v1/auth/session",
            &login_body("owner", OWNER_PASSWORD),
            BOOTSTRAP_PROOF,
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 201"), "{response}");
    login(address, "owner", OWNER_PASSWORD).await
}

async fn login(address: std::net::SocketAddr, username: &str, password: &str) -> (String, String) {
    let response = http_request(
        address,
        &plain_post("/api/v1/auth/session", &login_body(username, password)),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 201"), "{response}");
    let user_id = response_body(&response)["actor"]["id"]
        .as_str()
        .expect("actor id")
        .to_owned();
    (session_cookie_from_response(&response), user_id)
}

async fn create_and_login_member(
    address: std::net::SocketAddr,
    owner_cookie: &str,
    username: &str,
) -> (String, String) {
    let create = json!({
        "schemaVersion": SCHEMA_VERSION,
        "username": username,
        "role": "member",
    })
    .to_string();
    let response = http_request(
        address,
        &cookie_post("/api/v1/users", &create, owner_cookie),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 201"), "{response}");
    let temporary = response_body(&response)["temporaryPassword"]
        .as_str()
        .expect("temporary password")
        .to_owned();
    login(address, username, &temporary).await
}

// ---- staging bodies --------------------------------------------------------

fn occupancy_body(client_id: &str) -> String {
    json!({
        "schemaVersion": SCHEMA_VERSION,
        "clientId": client_id,
    })
    .to_string()
}

fn confirmed_cancel_body(client_id: &str) -> String {
    json!({
        "schemaVersion": SCHEMA_VERSION,
        "clientId": client_id,
        "mode": "cancel_and_release",
        "confirm": true,
    })
    .to_string()
}

fn launch_body(client_id: &str, binding_id: &str) -> String {
    json!({
        "schemaVersion": SCHEMA_VERSION,
        "clientId": client_id,
        "repositoryBindingId": binding_id,
    })
    .to_string()
}

// ---- device daemon helpers -------------------------------------------------

fn seed() -> DeviceIdentitySeed {
    DeviceIdentitySeed {
        display_name: "Cheng's MacBook".to_owned(),
        platform: "darwin".to_owned(),
        architecture: "arm64".to_owned(),
        client_version: "0.1.0-alpha.1".to_owned(),
    }
}

fn capacity(running: u32) -> ClientCapacityReport {
    ClientCapacityReport {
        max_concurrent_worker_sessions: 4,
        running_worker_sessions: running,
        reserved_worker_sessions: 0,
        draining_worker_sessions: 0,
    }
}

fn daemon_config(endpoint: &str) -> DaemonConfig {
    DaemonConfig {
        server_profile_id: "worker-loop-server".to_owned(),
        base_url: endpoint.to_owned(),
        server_display_name: "WinWinCode Control Plane".to_owned(),
        device_display_name: "Cheng's MacBook".to_owned(),
        platform: ClientPlatformTarget::Aarch64AppleDarwin,
        architecture: ClientArchitecture::Aarch64,
        client_version: "0.1.0-alpha.1".to_owned(),
        heartbeat_interval: Duration::from_millis(u64::from(SERVER_HEARTBEAT_MS)),
        enroll_poll_interval: Duration::from_millis(5),
        max_frames_per_exchange: 8,
        initial_backoff: Duration::from_millis(5),
        max_backoff: Duration::from_millis(200),
        capacity: capacity(0),
    }
}

/// Starts one daemon session over the real std HTTP transport.
fn start_daemon(endpoint: &str, device_root: &Path, stamp: &str) -> DeviceDaemon {
    let mut store = DeviceStore::open(device_root).expect("device store should open");
    let identity = ensure_device_identity(&mut store, &seed(), stamp).expect("device identity");
    DeviceDaemon::start(
        daemon_config(endpoint),
        store,
        Arc::new(HttpExchangeTransport::new(endpoint.to_owned())),
        &identity,
    )
    .expect("daemon start")
}

/// The device credential material of the daemon (for the one hand-built
/// uplink frame the idempotency phase posts directly).
fn device_credential(device_root: &Path) -> String {
    let store = DeviceStore::open(device_root).expect("device store connection");
    let identity = load_device_identity(&store)
        .expect("identity read")
        .expect("enrolled identity");
    identity.credential().material_hex().clone()
}

/// Drives the daemon loop until `predicate` holds, sleeping only the
/// durations the loop schedules.
fn drive_until(
    daemon: &mut DeviceDaemon,
    what: &str,
    mut predicate: impl FnMut(&mut DeviceDaemon) -> bool,
) {
    let deadline = StdInstant::now() + DRIVE_DEADLINE;
    while StdInstant::now() < deadline {
        if predicate(daemon) {
            return;
        }
        match daemon.tick(StdInstant::now()) {
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

/// The canonical wall-clock instant of now, offset by whole seconds, in the
/// fixed 24-character shape the durable lexicographic comparisons rely on.
fn canonical_now(offset_seconds: i64) -> Instant {
    let value = time::OffsetDateTime::now_utc() + time::Duration::seconds(offset_seconds);
    let millis = value.nanosecond() / 1_000_000;
    Instant(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        value.year(),
        u8::from(value.month()),
        value.day(),
        value.hour(),
        value.minute(),
        value.second(),
        millis
    ))
}

// ---- the device-side launch material ----------------------------------------

/// The local bridge the vertical plays: it receives the one-time worker
/// credential exactly as the `201` launch response delivered it and owns the
/// local worker directories under the device root.
struct LaunchResponseMaterial {
    credentials: Mutex<HashMap<String, String>>,
    worker_root: PathBuf,
}

impl LaunchResponseMaterial {
    fn new(worker_root: PathBuf) -> Self {
        Self {
            credentials: Mutex::new(HashMap::new()),
            worker_root,
        }
    }
}

impl WorkerLaunchMaterialSource for LaunchResponseMaterial {
    fn worker_credential(&self, credential_digest: &str) -> Option<String> {
        self.credentials
            .lock()
            .expect("credential lock")
            .get(credential_digest)
            .cloned()
    }

    fn launch_directories(&self, worker_session_id: &str) -> Option<WorkerLaunchDirectories> {
        let root = self.worker_root.join(worker_session_id);
        Some(WorkerLaunchDirectories {
            source_directory: root.join("source"),
            data_directory: root.join("data"),
            worker_root: root,
        })
    }
}

/// The stub worker binary: a field-checking script. It verifies every
/// managed-session contract field before signalling readiness, idles, and
/// exits zero on SIGTERM — process-level behavior stays fully controlled
/// while the config parsing itself is cross-validated against the real
/// `winwincode-worker` reader by the device-client suite.
const STUB_WORKER_BODY: &str = r#"config="$2"
for field in clientNodeId clientInstanceId occupancyLeaseId occupancyFencingToken \
             repositoryBindingId workerSessionId workerId workerInstanceId \
             sourceDirectory dataDirectory serverOrigin workerCredentialPath \
             productSessionId stageRunId; do
  grep -q "\"$field\"" "$config" || exit 9
done
trap 'exit 0' TERM
echo ready > "$(dirname "$config")/worker-ready"
while :; do sleep 0.1; done"#;

fn write_stub_worker(path: &Path) {
    fs::write(path, format!("#!/bin/sh\n{STUB_WORKER_BODY}\n")).expect("stub worker writes");
    let mut permissions = fs::metadata(path).expect("stub metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("stub chmod");
}

fn wait_for_ready_marker(data_directory: &Path, worker_session_id: &str) {
    let marker = data_directory
        .join(worker_session_id)
        .join("data")
        .join("worker-ready");
    let deadline = StdInstant::now() + Duration::from_secs(30);
    while !marker.exists() {
        assert!(
            StdInstant::now() < deadline,
            "worker {worker_session_id} never signalled readiness"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn file_mode(path: &Path) -> u32 {
    fs::metadata(path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .permissions()
        .mode()
        & 0o777
}

// ---- durable-state readers --------------------------------------------------

fn node_snapshot(data_directory: &Path, node_id: &str) -> winwincode_storage::ClientNodeRecord {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut registry = ClientRegistryService::new(&mut storage);
    registry
        .snapshot(node_id)
        .expect("registry read")
        .unwrap_or_else(|| panic!("node {node_id} must exist in the registry"))
}

fn active_lease(
    data_directory: &Path,
    node_id: &str,
) -> Option<winwincode_storage::OccupancyLeaseRecord> {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut occupancy = ClientOccupancyService::new(&mut storage);
    occupancy
        .active_lease_for_node(node_id)
        .expect("active lease lookup")
}

fn count_released_leases(data_directory: &Path, reason: &str) -> i64 {
    let connection = rusqlite::Connection::open(data_directory.join("control-plane.sqlite3"))
        .expect("open database");
    connection
        .query_row(
            "SELECT COUNT(*) FROM client_occupancy_leases WHERE release_reason = ?1",
            [reason],
            |row| row.get::<_, i64>(0),
        )
        .expect("release reason count")
}

fn launch_grant(
    data_directory: &Path,
    grant_id: &str,
) -> winwincode_storage::WorkerLaunchGrantRecord {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut grants = WorkerLaunchGrantService::new(&mut storage);
    grants
        .snapshot(grant_id)
        .expect("snapshot")
        .expect("grant exists")
}

fn audit_actions(data_directory: &Path, grant_id: &str) -> Vec<String> {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut grants = WorkerLaunchGrantService::new(&mut storage);
    grants
        .audit_trail(grant_id)
        .expect("audit trail")
        .into_iter()
        .map(|entry| entry.action.as_str().to_owned())
        .collect()
}

fn next_client_sequence(data_directory: &Path, node: &str) -> u64 {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut registry = ClientRegistryService::new(&mut storage);
    registry
        .exchange_cursors(node)
        .expect("cursors")
        .expect("cursors exist")
        .client_to_server_ack_sequence
        + 1
}

fn downlink_ack_cursor(data_directory: &Path, node: &str) -> u64 {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let cursors = {
        let mut registry = ClientRegistryService::new(&mut storage);
        registry
            .exchange_cursors(node)
            .expect("cursors")
            .expect("cursors exist")
            .server_to_client_ack_sequence
    };
    let high_water = storage
        .client_downlink_outbox()
        .expect("outbox")
        .high_water(node)
        .expect("high water");
    cursors.max(high_water)
}

/// Publishes one connect code digest the way the Device Client would.
fn publish_connect_code(data_directory: &Path, node: &str, instance: &str, code: &str) -> String {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut connect = ConnectCodeService::new(&mut storage);
    let now = Instant("2026-09-04T12:00:00.000Z".to_owned());
    let digest = format!("sha256:{:x}", Sha256::digest(code.as_bytes()));
    let publication = ConnectCodePublication::try_new(
        fresh_code_id(),
        digest,
        node,
        instance,
        1,
        Instant(VALID_UNTIL.to_owned()),
        5,
    )
    .expect("valid publication");
    connect.publish(&publication, &now).expect("publish code");
    code.to_owned()
}

/// Consumes one published code directly, staging the active `use` grant a
/// connected user would hold.
fn consume_code_as(data_directory: &Path, node: &str, code: &str, user_id: &str) {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut connect = ConnectCodeService::new(&mut storage);
    let now = Instant("2026-09-04T12:00:01.000Z".to_owned());
    let digest = format!("sha256:{:x}", Sha256::digest(code.as_bytes()));
    let connect_code = connect
        .code_snapshot_by_digest(&digest)
        .expect("code lookup")
        .expect("published code");
    let consume = ConnectCodeConsume::try_new(
        connect_code.connect_code_id.clone(),
        digest,
        connect_code.generation,
    )
    .expect("consume command");
    let issuance = AccessGrantIssuance::try_new(
        fresh_code_id().replacen("cct_", "cag_", 1),
        node,
        user_id,
        user_id,
        GrantTrustMode::Trusted,
        None,
    )
    .expect("issuance");
    connect
        .consume_and_grant(&consume, &issuance, &now)
        .expect("atomic consume and grant");
}

/// Stages one visible repository binding with an active `use` grant for the
/// user; returns the binding id.
fn stage_visible_binding(data_directory: &Path, node: &str, user_id: &str) -> String {
    let binding_id = fresh_binding_id();
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut ledger = storage.repository_binding_ledger().expect("ledger");
    let projection = RepositoryBindingProjection::try_new(
        binding_id.clone(),
        node,
        "winwincode",
        Some("main".to_owned()),
        Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
        RepositoryDirtyState::Clean,
        RepositoryAvailability::Available,
        format!("sha256:{:x}", Sha256::digest(binding_id.as_bytes())),
    )
    .expect("projection");
    ledger
        .upsert(
            &projection,
            None,
            0,
            &Instant("2026-09-04T12:00:02.000Z".to_owned()),
        )
        .expect("upsert");
    let issuance = RepositoryAccessGrantIssuance::try_new(
        fresh_binding_id().replacen("rbd_", "rag_", 1),
        &binding_id,
        user_id,
        user_id,
    )
    .expect("repo grant issuance");
    ledger
        .create_grant(
            &issuance,
            RepositoryGrantPermissions::Use,
            &Instant("2026-09-04T12:00:03.000Z".to_owned()),
        )
        .expect("repo grant");
    binding_id
}

/// Enqueues one Server → Client frame directly into the durable downlink
/// outbox at the next free stream position — the exact delivery surface the
/// real launch flow writes through, used here to stage a stale launch the
/// device must refuse.
fn enqueue_downlink_frame(
    data_directory: &Path,
    node: &str,
    instance: &str,
    message: ServerToClientMessage,
) {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let cursors = {
        let mut registry = ClientRegistryService::new(&mut storage);
        registry
            .exchange_cursors(node)
            .expect("cursors")
            .expect("cursors exist")
    };
    let mut downlink = storage.client_downlink_outbox().expect("outbox");
    let high_water = downlink.high_water(node).expect("high water");
    let sequence = cursors.server_to_client_ack_sequence.max(high_water) + 1;
    let envelope = ServerToClientEnvelope {
        schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
        message_id: format!("msg_stale_launch_{sequence}"),
        client_node_id: node.to_owned(),
        client_instance_id: instance.to_owned(),
        sequence,
        occurred_at: canonical_now(0).0,
        message,
    };
    let frame = serde_json::to_string(&envelope).expect("stale frame encodes");
    downlink
        .append(
            &ClientDownlinkAppend::try_new(node.to_owned(), envelope.message_id, sequence, frame)
                .expect("append command"),
            &canonical_now(0),
        )
        .expect("stale frame appends");
}

/// Polls the durable downlink outbox until the signed launch frame of the
/// in-flight `POST /api/v1/sessions` flow appears and answers the grant id
/// it carries. The daemon is parked while the frame is captured.
fn wait_for_launch_grant(data_directory: &Path, node: &str) -> String {
    let deadline = StdInstant::now() + Duration::from_secs(20);
    loop {
        let mut storage = SqliteStorage::open(data_directory).expect("storage");
        let ack = {
            let mut registry = ClientRegistryService::new(&mut storage);
            registry
                .exchange_cursors(node)
                .expect("cursors")
                .expect("cursors exist")
                .server_to_client_ack_sequence
        };
        let downlink = storage.client_downlink_outbox().expect("outbox");
        let pending = downlink.deliverable(node, ack, 100).expect("outbox read");
        for stored in pending {
            let Ok(value) = serde_json::from_str::<Value>(&stored.frame) else {
                continue;
            };
            if value["kind"] == json!("client.worker.launch") {
                return value["payload"]["launchGrant"]["workerLaunchGrantId"]
                    .as_str()
                    .expect("launch grant id")
                    .to_owned();
            }
        }
        assert!(
            StdInstant::now() < deadline,
            "the signed launch frame never reached the durable outbox"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn the_real_daemon_runs_the_full_worker_loop_over_http() {
    let data_directory = test_directory("worker-loop-server");
    let device_root = test_directory("worker-loop-device");
    let worker_root = device_root.join("workers");
    let running = start_server(&data_directory).await;
    let address = running.local_address();
    let endpoint = format!("http://{address}{EXCHANGE_ENDPOINT_PATH}");

    // ---- Phase 0: users, enrollment, capacity, worker lane, occupancy -----
    let (owner_cookie, owner_id) = initialize_and_login_owner(address).await;
    let (member_cookie, _member_id) =
        create_and_login_member(address, &owner_cookie, "member").await;

    let mut daemon = start_daemon(&endpoint, &device_root, "2026-09-04T00:00:00.000Z");
    drive_until(&mut daemon, "the enrollment adoption", |daemon| {
        daemon.is_enrolled()
    });
    let node_id = daemon.client_node_id().to_owned();

    // The frozen-v1 exchange lands reported capacity through the
    // instance-taking hello of the NEXT launch: relaunch once.
    daemon
        .into_store()
        .close()
        .expect("close before the capacity relaunch");
    let mut daemon = start_daemon(&endpoint, &device_root, "2026-09-04T00:30:00.000Z");
    let device_instance = daemon.client_instance_id().to_owned();
    drive_until(
        &mut daemon,
        "the takeover hello to land the capacity",
        |daemon| {
            settled(daemon)
                && node_snapshot(&data_directory, &node_id).max_concurrent_worker_sessions == 4
        },
    );

    // Wire the worker lane: the local supervisor over a second connection to
    // the same durable device store, every daemon hook attached.
    let stub_binary = worker_root.join("stub-winwincode-worker");
    fs::create_dir_all(&worker_root).expect("worker root creates");
    write_stub_worker(&stub_binary);
    let supervisor = SessionSupervisor::new(
        SupervisorConfig {
            client_node_id: node_id.clone(),
            client_instance_id: device_instance.clone(),
            // The exchange endpoint the tests serve is plain HTTP; the
            // managed-session contract enforces the https origin form (the
            // stub never connects to it).
            server_origin: format!("https://127.0.0.1:{}", address.port()),
            model_route: None,
            worker_binary_path: Some(stub_binary.clone()),
            max_concurrent_worker_sessions: 4,
            stop_grace_period: Duration::from_secs(5),
        },
        DeviceStore::open(&device_root).expect("supervisor store connection"),
    )
    .expect("supervisor builds");
    let material = Arc::new(LaunchResponseMaterial::new(worker_root.clone()));
    daemon.set_worker_supervisor(supervisor.clone());
    daemon.set_worker_launch_material_source(material.clone());
    daemon.set_worker_capacity_source(Arc::new(supervisor.clone()));
    daemon.set_lease_worker_controller(Arc::new(supervisor.clone()));
    let daemon_credential = device_credential(&device_root);

    consume_code_as(
        &data_directory,
        &node_id,
        &publish_connect_code(
            &data_directory,
            &node_id,
            daemon.client_instance_id(),
            "50585058",
        ),
        &owner_id,
    );
    let public_client_id = node_snapshot(&data_directory, &node_id).public_client_id;
    let binding_id = stage_visible_binding(&data_directory, &node_id, &owner_id);

    // ---- Phase 1: occupancy (claim → offer → ack → occupied) ---------------
    let claim_request = cookie_post(
        "/api/v1/clients/occupancy",
        &occupancy_body(&public_client_id),
        &owner_cookie,
    );
    let claim = tokio::spawn(async move { http_request(address, &claim_request).await });
    drive_until(&mut daemon, "the offer to settle occupied", |daemon| {
        settled(daemon)
            && active_lease(&data_directory, &node_id)
                .is_some_and(|lease| lease.state == OccupancyLeaseState::Occupied)
    });
    let response = claim.await.expect("claim task");
    assert_eq!(status_of(&response), "201", "{response}");
    let lease_one_id = response_body(&response)["occupancyLeaseId"]
        .as_str()
        .expect("lease id")
        .to_owned();
    let token_one = response_body(&response)["fencingToken"]
        .as_u64()
        .expect("fencing token");
    let mirror = daemon
        .store_mut()
        .occupancy_mirror()
        .expect("mirror read")
        .expect("the daemon persisted the occupancy mirror");
    assert_eq!(mirror.occupancy_lease_id, lease_one_id);
    assert_eq!(mirror.fencing_token, token_one);

    // ---- Phase 2: the non-holder cannot launch -----------------------------
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/sessions",
            &launch_body(&public_client_id, &binding_id),
            &member_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "403", "{response}");
    assert_eq!(wire_code(&response), "NOT_HOLDER");

    // ---- Phase 3: the holder launches; the daemon consumes the grant -------
    // The launch flow's 201 answers only after the device consumed the
    // grant, so the POST runs concurrently while the vertical drives the
    // daemon: the launch command is signed (durable downlink frame), the
    // daemon fences it, spawns through the supervisor, and the launch ack
    // consumes the grant exactly once — then the 201 body delivers the
    // one-time worker credential material.
    let launch_request = cookie_post(
        "/api/v1/sessions",
        &launch_body(&public_client_id, &binding_id),
        &owner_cookie,
    );
    let launch_task = tokio::spawn(async move { http_request(address, &launch_request).await });
    // The signed launch frame is durable before the flow answers; capture
    // its grant id from the outbox while the daemon is still parked.
    let grant_one_id = wait_for_launch_grant(&data_directory, &node_id);
    drive_until(
        &mut daemon,
        "the grant to be consumed by the launch ack",
        |daemon| {
            settled(daemon)
                && launch_grant(&data_directory, &grant_one_id).state
                    == WorkerLaunchGrantState::Consumed
        },
    );
    let response = launch_task.await.expect("launch task");
    assert_eq!(status_of(&response), "201", "{response}");
    let body = response_body(&response);
    assert_eq!(
        body["workerLaunchGrantId"].as_str().expect("grant id"),
        grant_one_id,
        "the 201 names the consumed grant"
    );
    let worker_session_one = body["workerSessionId"]
        .as_str()
        .expect("session id")
        .to_owned();
    let worker_instance_one = body["workerInstanceId"]
        .as_str()
        .expect("instance id")
        .to_owned();
    let grant_one = launch_grant(&data_directory, &grant_one_id);
    assert_eq!(grant_one.state, WorkerLaunchGrantState::Consumed);
    assert!(grant_one.consumed_at.is_some());
    assert_eq!(worker_session_one, grant_one.worker_session_id);
    assert_eq!(worker_instance_one, grant_one.worker_instance_id);
    // The raw 32-byte credential material crosses this response exactly once
    // and is digest-bound to the durable grant.
    let material_one = body["workerCredential"].as_str().expect("credential");
    let digest_one = body["credentialDigest"].as_str().expect("digest");
    assert_eq!(material_one.len(), 64);
    let secret: Vec<u8> = (0..material_one.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&material_one[index..index + 2], 16).expect("hex"))
        .collect();
    assert_eq!(
        digest_one,
        &format!("sha256:{:x}", Sha256::digest(&secret)),
        "the response material binds the grant digest"
    );
    assert_eq!(digest_one, grant_one.credential_digest);

    // The deferred delivery: the local bridge receives the launch-response
    // material and hands it to the daemon, which writes it into the
    // worker's private 0600 credential file.
    let delivered = daemon
        .receive_worker_credential(digest_one, material_one)
        .expect("deferred delivery");
    assert!(delivered, "the handled launch's digest is known");
    assert_eq!(
        audit_actions(&data_directory, &grant_one_id),
        vec!["issued".to_owned(), "consumed".to_owned()],
    );
    assert_eq!(daemon.status().worker_launches_accepted, 1);
    assert_eq!(daemon.status().worker_launches_rejected, 0);
    assert_eq!(
        daemon.status().unhandled_downlink_commands,
        0,
        "every downlink command of this vertical is handled by a lane"
    );

    // The device-side registry carries the grant identities and the stub
    // process is really alive.
    let record = supervisor
        .worker_process(&worker_session_one)
        .expect("registry read")
        .expect("the launch registered the worker session");
    assert_eq!(record.state, WORKER_STATE_RUNNING);
    assert_eq!(record.worker_id, grant_one.worker_id);
    assert_eq!(record.worker_instance_id, worker_instance_one);
    assert_eq!(record.launch_grant_id, grant_one_id);
    assert_eq!(record.occupancy_lease_id, lease_one_id);
    assert_eq!(record.repository_binding_id, binding_id);
    assert_eq!(
        supervisor
            .count_worker_processes_in_state(WORKER_STATE_RUNNING)
            .expect("running count"),
        1
    );
    let worker_data_one = worker_root.join(&worker_session_one).join("data");
    wait_for_ready_marker(&worker_root, &worker_session_one);

    // The private files are mode 0600 and the credential file carries
    // exactly the launch-response material.
    assert_eq!(
        file_mode(&worker_data_one.join("managed-session.json")),
        0o600
    );
    assert_eq!(file_mode(&worker_data_one.join("worker-credential")), 0o600);
    assert_eq!(
        fs::read_to_string(worker_data_one.join("worker-credential")).expect("credential read"),
        material_one,
        "the one-time material crossed to the credential file"
    );
    let config: Value = serde_json::from_str(
        &fs::read_to_string(worker_data_one.join("managed-session.json")).expect("config read"),
    )
    .expect("config is JSON");
    assert_eq!(config["clientNodeId"], json!(node_id));
    assert_eq!(config["clientInstanceId"], json!(device_instance));
    assert_eq!(config["occupancyLeaseId"], json!(lease_one_id));
    assert_eq!(
        config["occupancyFencingToken"],
        json!(token_one.to_string())
    );
    assert_eq!(config["repositoryBindingId"], json!(binding_id));
    assert_eq!(config["workerSessionId"], json!(worker_session_one));
    assert_eq!(config["workerId"], json!(grant_one.worker_id));
    assert_eq!(config["workerInstanceId"], json!(worker_instance_one));
    assert_eq!(
        config["workerCredentialPath"],
        json!(worker_data_one.join("worker-credential").to_str().unwrap())
    );
    assert_eq!(config["productSessionId"], json!(body["productSessionId"]));
    assert_eq!(config["stageRunId"], json!(body["stageRunId"]));

    // The heartbeat running count follows the registry.
    drive_until(
        &mut daemon,
        "the heartbeat to report the running worker",
        |daemon| {
            daemon.status().heartbeats_enqueued >= 1
                && node_snapshot(&data_directory, &node_id).reported_running_worker_sessions == 1
        },
    );

    // ---- Phase 4: a replayed accepted ack is an idempotent no-op -----------
    let revision_before = grant_one.revision;
    let duplicate_ack = ClientToServerEnvelope {
        schema_version: SCHEMA_VERSION.to_owned(),
        message_id: format!(
            "msg_duplicate_ack_{}",
            next_client_sequence(&data_directory, &node_id)
        ),
        client_node_id: node_id.clone(),
        client_instance_id: device_instance.clone(),
        sequence: next_client_sequence(&data_directory, &node_id),
        occurred_at: "2026-09-04T12:05:00.000Z".to_owned(),
        message: ClientToServerMessage::WorkerLaunchAck(Box::new(ClientWorkerLaunchAckPayload {
            occupancy: OccupancyCommandContext {
                command: CommandContext {
                    expected_revision: mirror.mirror_revision,
                    idempotency_key: format!("worker-launch-ack-{grant_one_id}"),
                },
                occupancy_lease_id: lease_one_id.clone(),
                occupancy_fencing_token: token_one,
            },
            worker_launch_grant_id: grant_one_id.clone(),
            worker_session_id: worker_session_one.clone(),
            worker_id: grant_one.worker_id.clone(),
            worker_instance_id: worker_instance_one.clone(),
            status: WorkerLaunchAckStatus::Accepted,
            error: None,
        })),
    };
    let (status, _) = post_exchange(
        address,
        &json!({
            "schemaVersion": SCHEMA_VERSION,
            "frames": [serde_json::to_value(&duplicate_ack).expect("ack frame")],
            "ackSequence": downlink_ack_cursor(&data_directory, &node_id),
        })
        .to_string(),
        Some(&daemon_credential),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    let grant_after = launch_grant(&data_directory, &grant_one_id);
    assert_eq!(grant_after.state, WorkerLaunchGrantState::Consumed);
    assert_eq!(
        grant_after.revision, revision_before,
        "no second consumption"
    );
    assert_eq!(
        audit_actions(&data_directory, &grant_one_id),
        vec!["issued".to_owned(), "consumed".to_owned()],
    );

    // ---- Phase 5: the supervisor stop path and the running count -----------
    let stopped = supervisor
        .stop(&worker_session_one, true)
        .expect("graceful stop");
    assert_eq!(stopped.state, WORKER_STATE_EXITED);
    assert_eq!(stopped.exit_code, Some(0));
    drive_until(
        &mut daemon,
        "the heartbeat to report zero running workers",
        |daemon| {
            node_snapshot(&data_directory, &node_id).reported_running_worker_sessions == 0
                && settled(daemon)
        },
    );

    // ---- Phase 6: the second launch and cancel_and_release -----------------
    let launch_request = cookie_post(
        "/api/v1/sessions",
        &launch_body(&public_client_id, &binding_id),
        &owner_cookie,
    );
    let launch_task = tokio::spawn(async move { http_request(address, &launch_request).await });
    let grant_two_id = wait_for_launch_grant(&data_directory, &node_id);
    drive_until(&mut daemon, "the second grant to be consumed", |daemon| {
        settled(daemon)
            && launch_grant(&data_directory, &grant_two_id).state
                == WorkerLaunchGrantState::Consumed
    });
    let response = launch_task.await.expect("launch task");
    assert_eq!(status_of(&response), "201", "{response}");
    let body = response_body(&response);
    assert_eq!(
        body["workerLaunchGrantId"].as_str().expect("grant id"),
        grant_two_id,
    );
    let worker_session_two = body["workerSessionId"]
        .as_str()
        .expect("session id")
        .to_owned();
    // The second launch's deferred material lands in its own private file.
    let material_two = body["workerCredential"].as_str().expect("credential");
    let digest_two = body["credentialDigest"].as_str().expect("digest");
    assert_ne!(digest_two, grant_one.credential_digest);
    let delivered = daemon
        .receive_worker_credential(digest_two, material_two)
        .expect("deferred delivery");
    assert!(delivered);
    let worker_data_two = worker_root.join(&worker_session_two).join("data");
    assert_eq!(
        fs::read_to_string(worker_data_two.join("worker-credential"))
            .expect("second credential read"),
        material_two,
    );
    assert_eq!(daemon.status().worker_launches_accepted, 2);
    let record_two = supervisor
        .worker_process(&worker_session_two)
        .expect("registry read")
        .expect("the second launch registered its worker session");
    assert_eq!(record_two.state, WORKER_STATE_RUNNING);
    wait_for_ready_marker(&worker_root, &worker_session_two);
    drive_until(
        &mut daemon,
        "the heartbeat to report the second running worker",
        |_daemon| node_snapshot(&data_directory, &node_id).reported_running_worker_sessions == 1,
    );

    // cancel_and_release requires the explicit confirmation first.
    let response = http_request(
        address,
        &cookie_delete(
            "/api/v1/clients/occupancy",
            &json!({
                "schemaVersion": SCHEMA_VERSION,
                "clientId": public_client_id,
                "mode": "cancel_and_release",
            })
            .to_string(),
            &owner_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "400", "{response}");
    assert_eq!(wire_code(&response), "CONFIRMATION_REQUIRED");

    let response = http_request(
        address,
        &cookie_delete(
            "/api/v1/clients/occupancy",
            &confirmed_cancel_body(&public_client_id),
            &owner_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "200", "{response}");
    assert_eq!(response_body(&response)["occupancy"], json!("draining"));

    // The daemon records the durable intent, the lease controller stops the
    // supervised worker, and the zeroed running count completes the drain.
    drive_until(
        &mut daemon,
        "the cancel_and_release to stop the worker and drain the lease",
        |daemon| active_lease(&data_directory, &node_id).is_none() && settled(daemon),
    );
    assert_eq!(
        daemon.status().workers_stopped_on_release,
        1,
        "the lease controller stopped the supervised worker"
    );
    assert_eq!(
        daemon
            .store_mut()
            .occupancy_release_intents()
            .expect("intents read")
            .len(),
        1
    );
    let intents = daemon
        .store_mut()
        .occupancy_release_intents()
        .expect("intents read");
    assert_eq!(
        intents[0].mode,
        ClientOccupancyReleaseMode::CancelTasksAndRelease
    );
    assert_eq!(intents[0].occupancy_lease_id, lease_one_id);
    assert_eq!(
        intents[0].affected_worker_sessions, 2,
        "both worker sessions of the lease are affected (the exited first \
         boot and the live second launch)"
    );
    let record_two = supervisor
        .worker_process(&worker_session_two)
        .expect("registry read")
        .expect("row");
    assert_eq!(record_two.state, WORKER_STATE_EXITED);
    assert_eq!(record_two.exit_code, Some(0));
    drive_until(
        &mut daemon,
        "the heartbeat to report the stopped worker",
        |_daemon| node_snapshot(&data_directory, &node_id).reported_running_worker_sessions == 0,
    );
    assert_eq!(count_released_leases(&data_directory, "drain_completed"), 1);

    // ---- Phase 7: a stale launch after a force fence is refused ------------
    // Claim once more, go silent, and let the offline sweep push the lease
    // into recovery; the overdue force-release fences the device with a
    // strictly higher token.
    let claim_request = cookie_post(
        "/api/v1/clients/occupancy",
        &occupancy_body(&public_client_id),
        &owner_cookie,
    );
    let claim = tokio::spawn(async move { http_request(address, &claim_request).await });
    drive_until(
        &mut daemon,
        "the recovery-scenario lease to settle occupied",
        |daemon| {
            settled(daemon)
                && active_lease(&data_directory, &node_id)
                    .is_some_and(|lease| lease.state == OccupancyLeaseState::Occupied)
        },
    );
    let response = claim.await.expect("claim task");
    assert_eq!(status_of(&response), "201", "{response}");
    let lease_three_id = response_body(&response)["occupancyLeaseId"]
        .as_str()
        .expect("lease id")
        .to_owned();

    let sweep_application = ClientOccupancyApplication::open(
        &data_directory,
        &ClientOccupancyConfig {
            recovery_window: RECOVERY_WINDOW,
            ..ClientOccupancyConfig::default()
        },
    )
    .expect("valid sweep application");
    let outcome = sweep_application
        .run_offline_sweep(&canonical_now(1), &canonical_now(0))
        .expect("sweep");
    assert!(
        outcome.swept_nodes.contains(&node_id),
        "the silent device must be swept: {outcome:?}"
    );
    assert_eq!(
        active_lease(&data_directory, &node_id)
            .expect("recovery lease")
            .state,
        OccupancyLeaseState::RecoveryPending
    );
    std::thread::sleep(RECOVERY_WINDOW + Duration::from_millis(250));
    assert!(
        sweep_application
            .recovery_overdue(&public_client_id)
            .expect("overdue query"),
        "the elapsed window reads as overdue"
    );
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/occupancy/force-release",
            &occupancy_body(&public_client_id),
            &owner_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "200", "{response}");
    let fence_token = response_body(&response)["forceFenceToken"]
        .as_u64()
        .expect("fence token");
    drive_until(
        &mut daemon,
        "the force fence to overwrite the mirror",
        |daemon| settled(daemon) && daemon.status().occupancy_force_fences_applied >= 1,
    );
    let mirror = daemon
        .store_mut()
        .occupancy_mirror()
        .expect("mirror read")
        .expect("the fence overwrote the mirror");
    assert_eq!(mirror.occupancy_lease_id, lease_three_id);
    assert_eq!(mirror.fencing_token, fence_token);

    // The stale launch replays the FIRST grant's stamp: its lease/token pair
    // died with the force fence, so the daemon refuses it and spawns
    // nothing. The frame rides the real durable downlink outbox.
    let grant_one_replay = launch_grant(&data_directory, &grant_one_id);
    enqueue_downlink_frame(
        &data_directory,
        &node_id,
        &device_instance,
        ServerToClientMessage::WorkerLaunch(ServerWorkerLaunchPayload {
            occupancy: OccupancyCommandContext {
                command: CommandContext {
                    expected_revision: mirror.mirror_revision,
                    idempotency_key: "stale_launch_replay".to_owned(),
                },
                occupancy_lease_id: grant_one_replay.occupancy_lease_id.clone(),
                occupancy_fencing_token: grant_one_replay.occupancy_fencing_token,
            },
            launch_grant: WorkerLaunchGrant {
                worker_launch_grant_id: grant_one_replay.worker_launch_grant_id.clone(),
                client_node_id: grant_one_replay.client_node_id.clone(),
                client_instance_id: grant_one_replay.client_instance_id.clone(),
                occupancy_lease_id: grant_one_replay.occupancy_lease_id.clone(),
                occupancy_fencing_token: grant_one_replay.occupancy_fencing_token,
                repository_binding_id: grant_one_replay.repository_binding_id.clone(),
                product_session_id: grant_one_replay
                    .product_session_id
                    .clone()
                    .unwrap_or_default(),
                stage_run_id: grant_one_replay.stage_run_id.clone().unwrap_or_default(),
                worker_session_id: grant_one_replay.worker_session_id.clone(),
                worker_id: grant_one_replay.worker_id.clone(),
                worker_instance_id: grant_one_replay.worker_instance_id.clone(),
                credential_digest: grant_one_replay.credential_digest.clone(),
                expires_at: grant_one_replay.expires_at.0.clone(),
                state: WireGrantState::Issued,
                revision: grant_one_replay.revision,
            },
        }),
    );
    drive_until(&mut daemon, "the stale launch to be refused", |daemon| {
        daemon.status().worker_launches_rejected == 1 && settled(daemon)
    });
    assert_eq!(
        daemon.status().worker_launches_accepted,
        2,
        "the stale launch spawned nothing"
    );
    assert_eq!(
        supervisor
            .count_worker_processes_in_state(WORKER_STATE_RUNNING)
            .expect("running count"),
        0
    );
    let stale_session = supervisor
        .worker_process(&grant_one_replay.worker_session_id)
        .expect("registry read")
        .expect("the first launch's session row is untouched by the replay");
    assert_eq!(stale_session.state, WORKER_STATE_EXITED);
    assert_eq!(
        stale_session.worker_instance_id, worker_instance_one,
        "the superseded grant's session was never respawned"
    );
    let mirror_after = daemon
        .store_mut()
        .occupancy_mirror()
        .expect("mirror read")
        .expect("mirror");
    assert_eq!(
        mirror_after.fencing_token, fence_token,
        "the stale launch never touched the mirror"
    );
    // The consumed grant the stale frame named stays consumed: the rejection
    // changes no durable grant state.
    assert_eq!(
        launch_grant(&data_directory, &grant_one_id).state,
        WorkerLaunchGrantState::Consumed
    );

    // ---- Bilateral closure: both sides agree at rest -----------------------
    drive_until(&mut daemon, "the stream to drain", settled);
    {
        let mut storage = SqliteStorage::open(&data_directory).expect("storage");
        let outbox = storage.client_downlink_outbox().expect("outbox");
        assert_eq!(
            outbox
                .deliverable(&node_id, 0, 100)
                .expect("outbox read")
                .len(),
            0,
            "every downlink frame was acknowledged and retained no longer"
        );
    }
    let inbox = daemon
        .store_mut()
        .inbox_cursor(daemon_config(&endpoint).server_profile_id.as_str())
        .expect("cursor read")
        .expect("inbox cursor");
    {
        let mut storage = SqliteStorage::open(&data_directory).expect("storage");
        let mut registry = ClientRegistryService::new(&mut storage);
        let cursors = registry
            .exchange_cursors(&node_id)
            .expect("cursors")
            .expect("cursors exist");
        assert_eq!(
            inbox.last_sequence, cursors.server_to_client_ack_sequence,
            "the device's durable downlink cursor equals the server's"
        );
        assert_eq!(
            daemon.outbox_snapshot().expect("snapshot").ack_sequence,
            cursors.client_to_server_ack_sequence,
            "the device's durable uplink cursor equals the server's"
        );
    }

    // Stopping the first (already terminal) worker session is the idempotent
    // no-op; the registry answers the stored row.
    supervisor
        .stop(&worker_session_one, false)
        .expect("terminal stop is a no-op");
    let terminal = supervisor
        .worker_process(&worker_session_one)
        .expect("registry read")
        .expect("row");
    assert_eq!(terminal.state, WORKER_STATE_EXITED);
    daemon.into_store().close().expect("store close");
    running.shutdown().await.expect("server shutdown");
    let _ = fs::remove_dir_all(&data_directory);
    let _ = fs::remove_dir_all(&device_root);
}
