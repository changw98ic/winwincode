// SPDX-License-Identifier: Apache-2.0

//! The occupancy loop vertical: the full CLIENT-300 occupancy chain over a
//! real server (`POST /api/v1/clients/occupancy` wired to
//! `ClientOccupancyApplication`) and a real Device Client daemon — no fake
//! responder. The daemon enrolls over the real HTTP exchange, mirrors every
//! `client.occupancy.offer` into its durable device store, answers
//! `client.occupancy.ack` with the persisted mirror revision, records
//! release intents per mode, and applies the overdue force fence that
//! invalidates the old token.
//!
//! The vertical also pins the cross-lane interoperability point: the device
//! refuses any occupancy command whose `expectedRevision` is not exactly its
//! local mirror revision (0 before the first offer), so the server must
//! stamp every downlink command with its view of the confirmed mirror
//! revision. Five consecutive occupancy cycles on one device — including
//! three releases, a recovery sweep, and a force fence — prove the stamp
//! tracking: with a stale revision view the device would answer
//! `client.occupancy.rejected` and every claim after the first would fail.
//! The daemon's `occupancy_offers_rejected` counter stays at zero across the
//! whole run.
//!
//! One frozen-v1 exchange fact shows itself in the setup: the enrollment
//! registers the node with zeroed capacity, and the reported worker-session
//! slots land through the instance-taking hello of the NEXT launch — so the
//! vertical relaunches the daemon once (a real lifecycle event) before the
//! first claim.
//!
//! Assertions land on both durable sides throughout: the server's occupancy
//! ledger and registry projection, and the device store's occupancy mirror,
//! release intents, and exchange cursors.

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
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
use winwincode_control_plane::ClientOccupancyService;
use winwincode_control_plane::ClientRegistryService;
use winwincode_control_plane::ConnectCodeService;
use winwincode_device_client::{
    DaemonConfig, DeviceDaemon, DeviceIdentitySeed, DeviceStore, FencedCommandKind,
    FencingRejection, FencingVerdict, HttpExchangeTransport, TickOutcome, ensure_device_identity,
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
    AccessGrantIssuance, ConnectCodeConsume, ConnectCodePublication, GrantTrustMode,
    OccupancyLeaseState, SqliteStorage,
};

const BOOTSTRAP_PROOF: &str = "occupancy-loop-test-bootstrap";
const ORIGIN: &str = "https://client.example";
const OWNER_PASSWORD: &str = "initial-owner-password";
const SCHEMA_VERSION: &str = "winwincode/v1";
const VALID_UNTIL: &str = "2100-01-01T00:00:00.000Z";
const EXCHANGE_ENDPOINT_PATH: &str = "/internal/v1/client/exchange";
/// The enrollment acceptance pins this server-requested heartbeat cadence,
/// so the real daemon answers offers and completes drains in milliseconds.
const SERVER_HEARTBEAT_MS: u32 = 200;
const DRIVE_DEADLINE: Duration = Duration::from_secs(20);
/// The recovery window of the sweep application: short enough that the
/// force-release deadline passes within the test without touching the wall
/// clock beyond a bounded sleep.
const RECOVERY_WINDOW: Duration = Duration::from_millis(50);

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
static NEXT_CODE_ID: AtomicU64 = AtomicU64::new(1);
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

#[derive(Default)]
struct NoopApi;

impl ControlPlaneApiPort for NoopApi {
    fn command(&self, _: &AuthenticatedPrincipal, _: Value) -> Result<Value, ApiError> {
        Err(ApiError::new(
            501,
            "NOT_IMPLEMENTED",
            "unused in the occupancy loop test",
        ))
    }

    fn query(&self, _: &AuthenticatedPrincipal, _: Value) -> Result<Value, ApiError> {
        Err(ApiError::new(
            501,
            "NOT_IMPLEMENTED",
            "unused in the occupancy loop test",
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

fn cookie_get(path: &str, cookie: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\nHost: control.example\r\nOrigin: {ORIGIN}\r\nCookie: wwc_session={cookie}\r\nConnection: close\r\n\r\n"
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

// ---- occupancy HTTP bodies -------------------------------------------------

fn occupancy_body(client_id: &str) -> String {
    json!({
        "schemaVersion": SCHEMA_VERSION,
        "clientId": client_id,
    })
    .to_string()
}

fn release_body(client_id: &str, mode: &str) -> String {
    json!({
        "schemaVersion": SCHEMA_VERSION,
        "clientId": client_id,
        "mode": mode,
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

fn daemon_config(endpoint: &str, running: u32) -> DaemonConfig {
    DaemonConfig {
        server_profile_id: "loop-server".to_owned(),
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
        capacity: capacity(running),
    }
}

/// Starts one daemon session over the real std HTTP transport: the durable
/// store is reopened and the launch identity restored (the adopted
/// enrollment and the issued credential survive every restart; only the
/// launch instance rotates).
fn start_daemon(
    endpoint: &str,
    running: u32,
    device_root: &Path,
    stamp: &str,
) -> (DeviceDaemon, DaemonConfig) {
    let config = daemon_config(endpoint, running);
    let mut store = DeviceStore::open(device_root).expect("device store should open");
    let identity = ensure_device_identity(&mut store, &seed(), stamp).expect("device identity");
    let daemon = DeviceDaemon::start(
        config.clone(),
        store,
        Arc::new(HttpExchangeTransport::new(endpoint.to_owned())),
        &identity,
    )
    .expect("daemon start");
    (daemon, config)
}

/// Drives the daemon loop until `predicate` holds, sleeping only the
/// durations the loop schedules. The predicate may read server storage: the
/// daemon and the server exchange over real HTTP while the loop runs.
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

// ---- durable-state readers -------------------------------------------------

fn node_snapshot(data_directory: &Path, node_id: &str) -> winwincode_storage::ClientNodeRecord {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut registry = ClientRegistryService::new(&mut storage);
    registry
        .snapshot(node_id)
        .expect("registry read")
        .unwrap_or_else(|| panic!("node {node_id} must exist in the registry"))
}

fn exchange_cursors(
    data_directory: &Path,
    node_id: &str,
) -> winwincode_storage::ClientExchangeCursors {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut registry = ClientRegistryService::new(&mut storage);
    registry
        .exchange_cursors(node_id)
        .expect("cursor read")
        .unwrap_or_else(|| panic!("cursors of {node_id} must exist"))
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

/// Publishes one connect code digest the way the Device Client would, and
/// returns the 8-digit code (grant staging for the occupancy claim gate; the
/// connect flow itself is owned by its own lane).
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

// ---- the vertical ----------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn the_real_device_daemon_runs_the_full_occupancy_loop_over_http() {
    let data_directory = test_directory("occupancy-loop-server");
    let device_root = test_directory("occupancy-loop-device");
    let running = start_server(&data_directory).await;
    let address = running.local_address();
    let endpoint = format!("http://{address}{EXCHANGE_ENDPOINT_PATH}");

    // ---- Phase 0: users, enrollment, hello, heartbeat ----------------------
    let (owner_cookie, owner_id) = initialize_and_login_owner(address).await;
    let (member_cookie, member_id) =
        create_and_login_member(address, &owner_cookie, "member").await;
    assert_ne!(owner_id, member_id);

    let (mut daemon, config) = start_daemon(&endpoint, 0, &device_root, "2026-09-04T00:00:00.000Z");
    drive_until(&mut daemon, "the enrollment adoption", |daemon| {
        daemon.is_enrolled()
    });
    let node_id = daemon.client_node_id().to_owned();
    assert!(
        node_id.starts_with("cnd_") && node_id.len() == 30,
        "the daemon must adopt the server-assigned cnd_ identity: {node_id}"
    );
    drive_until(
        &mut daemon,
        "the hello and the first heartbeats",
        |daemon| settled(daemon) && daemon.status().heartbeats_enqueued >= 1,
    );
    let record = node_snapshot(&data_directory, &node_id);
    assert_eq!(
        record.presence_state,
        winwincode_storage::ClientPresenceState::Online
    );
    assert!(record.last_heartbeat_at.is_some());
    assert_eq!(
        record.max_concurrent_worker_sessions, 0,
        "the frozen v1 exchange lands reported capacity through the \
         instance-taking hello, not the announcement hello of the \
         enrollment instance itself"
    );
    // Device side: the profile persisted and the acceptance frame advanced
    // the downlink cursor.
    assert!(
        daemon
            .store_mut()
            .server_profile(&config.server_profile_id)
            .expect("profile read")
            .is_some(),
        "the adoption must persist the server profile"
    );
    assert_eq!(
        daemon
            .store_mut()
            .inbox_cursor(&config.server_profile_id)
            .expect("cursor read")
            .expect("the acceptance frame advanced the inbox cursor")
            .last_sequence,
        1
    );
    assert!(
        daemon.occupancy_mirror().is_none(),
        "a fresh device holds no occupancy mirror"
    );

    // ---- First relaunch: the instance-taking hello lands the capacity ------
    // The claim gate needs the reported worker-session slots; every real
    // relaunch presents them through the takeover hello of the new launch
    // instance.
    daemon
        .into_store()
        .close()
        .expect("close before the relaunch");
    let (mut daemon, config) = start_daemon(&endpoint, 0, &device_root, "2026-09-04T00:30:00.000Z");
    drive_until(
        &mut daemon,
        "the takeover hello to land the capacity",
        |daemon| {
            settled(daemon)
                && node_snapshot(&data_directory, &node_id).max_concurrent_worker_sessions == 4
        },
    );
    assert_eq!(
        node_snapshot(&data_directory, &node_id).presence_state,
        winwincode_storage::ClientPresenceState::Online
    );

    consume_code_as(
        &data_directory,
        &node_id,
        &publish_connect_code(
            &data_directory,
            &node_id,
            daemon.client_instance_id(),
            "36903690",
        ),
        &owner_id,
    );
    let public_client_id = record.public_client_id.clone();

    // ---- Phase 1: User A claims; the daemon mirrors and acks ---------------
    let claim_request = cookie_post(
        "/api/v1/clients/occupancy",
        &occupancy_body(&public_client_id),
        &owner_cookie,
    );
    let claim = tokio::spawn(async move { http_request(address, &claim_request).await });
    drive_until(
        &mut daemon,
        "the first offer to be mirrored, acked, and settled occupied",
        |daemon| {
            settled(daemon)
                && active_lease(&data_directory, &node_id)
                    .is_some_and(|lease| lease.state == OccupancyLeaseState::Occupied)
        },
    );
    let response = claim.await.expect("claim task");
    assert_eq!(status_of(&response), "201", "{response}");
    let holder_view = response_body(&response);
    assert_eq!(holder_view["occupancy"], json!("occupied"));
    assert_eq!(holder_view["holderUserId"], json!(owner_id));
    assert_eq!(holder_view["capacityTotal"], json!(4));
    let lease_one_id = holder_view["occupancyLeaseId"]
        .as_str()
        .expect("lease id")
        .to_owned();
    let token_one = holder_view["fencingToken"].as_u64().expect("fencing token");

    let lease = active_lease(&data_directory, &node_id).expect("occupied lease");
    assert_eq!(lease.occupancy_lease_id, lease_one_id);
    assert_eq!(lease.fencing_token, token_one);
    assert_eq!(lease.holder_user_id, owner_id);
    assert!(
        lease.acknowledged_at.is_some(),
        "the device ack was recorded"
    );

    let mirror = daemon
        .store_mut()
        .occupancy_mirror()
        .expect("mirror read")
        .expect("the daemon persisted the occupancy mirror");
    assert_eq!(mirror.occupancy_lease_id, lease_one_id);
    assert_eq!(mirror.fencing_token, token_one);
    assert_eq!(
        mirror.mirror_revision, 1,
        "the first offer starts the mirror at revision 1"
    );
    assert_eq!(mirror.holder_user_id.as_deref(), Some(owner_id.as_str()));
    assert_eq!(daemon.status().occupancy_offers_acked, 1);
    assert_eq!(
        daemon.status().occupancy_offers_rejected,
        0,
        "the device must not refuse the first offer"
    );

    // ---- Phase 2: the non-holder sees nothing but the conflict -------------
    let response = http_request(
        address,
        &cookie_get(
            &format!("/api/v1/clients/{public_client_id}/occupancy"),
            &member_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "200", "{response}");
    let view = response_body(&response);
    let fields = view
        .as_object()
        .expect("projection object")
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(fields.len(), 3, "{view}");
    assert_eq!(view["occupancy"], json!("occupied-by-other"));
    let serialized = response.clone();
    assert!(!serialized.to_lowercase().contains("holder"));
    assert!(!serialized.contains("occupancyLeaseId"));
    assert!(!serialized.contains(&owner_id));

    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/occupancy",
            &occupancy_body(&public_client_id),
            &member_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "409", "{response}");
    assert_eq!(wire_code(&response), "OCCUPIED_BY_OTHER");

    // ---- Phase 3: drain release with the real device -----------------------
    // A restart with running sessions (a new launch instance) reports two
    // active worker sessions, so the holder release drains instead of
    // releasing.
    daemon
        .into_store()
        .close()
        .expect("close before the drain restart");
    let (mut daemon, _config) =
        start_daemon(&endpoint, 2, &device_root, "2026-09-04T01:00:00.000Z");
    drive_until(&mut daemon, "the running count to project", |daemon| {
        daemon.status().heartbeats_enqueued >= 1
            && node_snapshot(&data_directory, &node_id).reported_running_worker_sessions == 2
    });
    let response = http_request(
        address,
        &cookie_delete(
            "/api/v1/clients/occupancy",
            &release_body(&public_client_id, "drain"),
            &owner_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "200", "{response}");
    assert_eq!(response_body(&response)["occupancy"], json!("draining"));
    assert_eq!(
        active_lease(&data_directory, &node_id)
            .expect("draining lease")
            .state,
        OccupancyLeaseState::Draining
    );
    drive_until(&mut daemon, "the drain release intent", |daemon| {
        daemon
            .store_mut()
            .occupancy_release_intents()
            .expect("intents read")
            .len()
            == 1
    });
    // Heartbeats keep reporting running sessions: the lease stays draining.
    drive_until(&mut daemon, "one more heartbeat under load", |daemon| {
        daemon.status().heartbeats_enqueued >= 2
    });
    assert_eq!(
        active_lease(&data_directory, &node_id)
            .expect("still draining")
            .state,
        OccupancyLeaseState::Draining,
        "running sessions hold the lease in draining"
    );
    {
        let intents = daemon
            .store_mut()
            .occupancy_release_intents()
            .expect("intents read");
        assert_eq!(intents[0].occupancy_lease_id, lease_one_id);
        assert_eq!(intents[0].fencing_token, token_one);
        assert_eq!(
            intents[0].mode,
            ClientOccupancyReleaseMode::DrainThenRelease
        );
    }
    assert_eq!(
        daemon.status().occupancy_offers_rejected,
        0,
        "the release command must pass the device's revision judgement"
    );

    // Zero reported sessions complete the drain automatically.
    let (mut daemon, _config) = {
        let closed = daemon.into_store();
        closed.close().expect("close before the zero restart");
        start_daemon(&endpoint, 0, &device_root, "2026-09-04T02:00:00.000Z")
    };
    drive_until(&mut daemon, "the drain to complete", |daemon| {
        settled(daemon) && active_lease(&data_directory, &node_id).is_none()
    });
    assert_eq!(count_released_leases(&data_directory, "drain_completed"), 1);

    // ---- Phase 4: immediate release, second cycle --------------------------
    // The second offer is the interop proof: the device mirror sits at
    // revision 1, so the server must stamp the offer with its confirmed
    // view (1) — a zero stamp would be refused as a local state conflict.
    let claim_request = cookie_post(
        "/api/v1/clients/occupancy",
        &occupancy_body(&public_client_id),
        &owner_cookie,
    );
    let claim = tokio::spawn(async move { http_request(address, &claim_request).await });
    drive_until(
        &mut daemon,
        "the second offer to settle occupied",
        |daemon| {
            settled(daemon)
                && active_lease(&data_directory, &node_id)
                    .is_some_and(|lease| lease.state == OccupancyLeaseState::Occupied)
        },
    );
    let response = claim.await.expect("claim task");
    assert_eq!(status_of(&response), "201", "{response}");
    let view = response_body(&response);
    let lease_two_id = view["occupancyLeaseId"]
        .as_str()
        .expect("lease id")
        .to_owned();
    let token_two = view["fencingToken"].as_u64().expect("fencing token");
    assert_ne!(lease_two_id, lease_one_id);
    assert!(
        token_two > token_one,
        "the fencing token is strictly higher"
    );

    let mirror = daemon
        .store_mut()
        .occupancy_mirror()
        .expect("mirror read")
        .expect("mirror");
    assert_eq!(mirror.occupancy_lease_id, lease_two_id);
    assert_eq!(mirror.fencing_token, token_two);
    assert_eq!(
        mirror.mirror_revision, 2,
        "the second offer advances the mirror"
    );
    assert_eq!(daemon.status().occupancy_offers_acked, 1);
    assert_eq!(daemon.status().occupancy_offers_rejected, 0);

    let response = http_request(
        address,
        &cookie_delete(
            "/api/v1/clients/occupancy",
            &release_body(&public_client_id, "release"),
            &owner_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "200", "{response}");
    assert_eq!(response_body(&response)["occupancy"], json!("released"));
    drive_until(&mut daemon, "the immediate release intent", |daemon| {
        daemon
            .store_mut()
            .occupancy_release_intents()
            .expect("intents read")
            .len()
            == 2
    });
    {
        let intents = daemon
            .store_mut()
            .occupancy_release_intents()
            .expect("intents read");
        assert_eq!(intents[1].mode, ClientOccupancyReleaseMode::Immediate);
        assert_eq!(intents[1].fencing_token, token_two);
    }
    // The release never mutates the mirror: only an offer or a force fence
    // advances it.
    let mirror = daemon
        .store_mut()
        .occupancy_mirror()
        .expect("mirror read")
        .expect("mirror");
    assert_eq!(mirror.occupancy_lease_id, lease_two_id);
    assert_eq!(mirror.mirror_revision, 2);
    assert!(active_lease(&data_directory, &node_id).is_none());

    // ---- Phase 5: cancel-and-release with the confirmation gate ------------
    let (mut daemon, _config) = {
        let closed = daemon.into_store();
        closed.close().expect("close before the busy restart");
        start_daemon(&endpoint, 1, &device_root, "2026-09-04T03:00:00.000Z")
    };
    drive_until(
        &mut daemon,
        "the single running session to project",
        |daemon| {
            daemon.status().heartbeats_enqueued >= 1
                && node_snapshot(&data_directory, &node_id).reported_running_worker_sessions == 1
        },
    );
    let claim_request = cookie_post(
        "/api/v1/clients/occupancy",
        &occupancy_body(&public_client_id),
        &owner_cookie,
    );
    let claim = tokio::spawn(async move { http_request(address, &claim_request).await });
    drive_until(
        &mut daemon,
        "the third offer to settle occupied",
        |daemon| {
            settled(daemon)
                && active_lease(&data_directory, &node_id)
                    .is_some_and(|lease| lease.state == OccupancyLeaseState::Occupied)
        },
    );
    let response = claim.await.expect("claim task");
    assert_eq!(status_of(&response), "201", "{response}");
    let view = response_body(&response);
    let lease_three_id = view["occupancyLeaseId"]
        .as_str()
        .expect("lease id")
        .to_owned();
    let token_three = view["fencingToken"].as_u64().expect("fencing token");
    assert_eq!(
        daemon
            .store_mut()
            .occupancy_mirror()
            .expect("mirror read")
            .expect("mirror")
            .mirror_revision,
        3,
        "the third offer advances the mirror again"
    );

    let response = http_request(
        address,
        &cookie_delete(
            "/api/v1/clients/occupancy",
            &release_body(&public_client_id, "cancel_and_release"),
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
    drive_until(&mut daemon, "the cancel release intent", |daemon| {
        daemon
            .store_mut()
            .occupancy_release_intents()
            .expect("intents read")
            .len()
            == 3
    });
    assert_eq!(
        daemon
            .store_mut()
            .occupancy_release_intents()
            .expect("intents read")[2]
            .mode,
        ClientOccupancyReleaseMode::CancelTasksAndRelease
    );
    {
        let intents = daemon
            .store_mut()
            .occupancy_release_intents()
            .expect("intents read");
        assert_eq!(intents[2].occupancy_lease_id, lease_three_id);
        assert_eq!(intents[2].fencing_token, token_three);
    }
    assert_eq!(daemon.status().occupancy_offers_rejected, 0);

    let (mut daemon, _config) = {
        let closed = daemon.into_store();
        closed.close().expect("close before the final restart");
        start_daemon(&endpoint, 0, &device_root, "2026-09-04T04:00:00.000Z")
    };
    drive_until(&mut daemon, "the cancelled lease to drain", |daemon| {
        settled(daemon) && active_lease(&data_directory, &node_id).is_none()
    });
    assert_eq!(count_released_leases(&data_directory, "drain_completed"), 2);

    // ---- Phase 6: heartbeat silence, offline sweep, no preemption ----------
    let claim_request = cookie_post(
        "/api/v1/clients/occupancy",
        &occupancy_body(&public_client_id),
        &owner_cookie,
    );
    let claim = tokio::spawn(async move { http_request(address, &claim_request).await });
    drive_until(
        &mut daemon,
        "the fourth offer to settle occupied",
        |daemon| {
            settled(daemon)
                && active_lease(&data_directory, &node_id)
                    .is_some_and(|lease| lease.state == OccupancyLeaseState::Occupied)
        },
    );
    let response = claim.await.expect("claim task");
    assert_eq!(status_of(&response), "201", "{response}");
    let view = response_body(&response);
    let lease_four_id = view["occupancyLeaseId"]
        .as_str()
        .expect("lease id")
        .to_owned();
    let token_four = view["fencingToken"].as_u64().expect("fencing token");
    assert_eq!(
        daemon
            .store_mut()
            .occupancy_mirror()
            .expect("mirror read")
            .expect("mirror")
            .mirror_revision,
        4
    );

    // The device goes silent: the sweep projects it and its lease. The
    // cutoff sits one second ahead of the wall clock, so the last accepted
    // heartbeat deterministically reads as stale; the short recovery window
    // makes the force-release deadline pass without waiting beyond a bounded
    // sleep.
    let sweep_application = ClientOccupancyApplication::open(
        &data_directory,
        &ClientOccupancyConfig {
            recovery_window: RECOVERY_WINDOW,
            ..ClientOccupancyConfig::default()
        },
    )
    .expect("valid sweep application");
    let now = canonical_now(0);
    let cutoff = canonical_now(1);
    let outcome = sweep_application
        .run_offline_sweep(&cutoff, &now)
        .expect("sweep");
    assert!(
        outcome.swept_nodes.contains(&node_id),
        "the silent device must be swept: {outcome:?}"
    );
    assert_eq!(outcome.leases_pending_recovery, vec![lease_four_id.clone()]);
    assert_eq!(
        node_snapshot(&data_directory, &node_id).presence_state,
        winwincode_storage::ClientPresenceState::Offline
    );
    let pending = active_lease(&data_directory, &node_id).expect("recovery lease");
    assert_eq!(pending.state, OccupancyLeaseState::RecoveryPending);
    assert_eq!(pending.fencing_token, token_four);
    assert!(pending.recovery_deadline_at.is_some());

    // No preemption while the lease is pending recovery: B's claim is
    // rejected by the active-lease gate even though the device is offline.
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/occupancy",
            &occupancy_body(&public_client_id),
            &member_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "409", "{response}");
    assert_eq!(wire_code(&response), "OCCUPIED_BY_OTHER");

    // The device reconnects; the heartbeats project it online again while
    // the lease stays recovery pending (reconciliation belongs to the worker
    // lane) — still no preemption.
    drive_until(&mut daemon, "the reconnect to project online", |daemon| {
        settled(daemon)
            && node_snapshot(&data_directory, &node_id).presence_state
                == winwincode_storage::ClientPresenceState::Online
    });
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/occupancy",
            &occupancy_body(&public_client_id),
            &member_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "409", "{response}");
    assert_eq!(wire_code(&response), "OCCUPIED_BY_OTHER");
    assert_eq!(
        active_lease(&data_directory, &node_id)
            .expect("still pending")
            .state,
        OccupancyLeaseState::RecoveryPending
    );

    // ---- Phase 7: overdue force release fences the device ------------------
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
            &member_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "403", "{response}");
    assert_eq!(wire_code(&response), "PERMISSION_DENIED");

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
    let value = response_body(&response);
    assert_eq!(value["released"], json!(true));
    assert_eq!(value["occupancyLeaseId"], json!(lease_four_id));
    let fence_token = value["forceFenceToken"].as_u64().expect("fence token");
    assert!(
        fence_token > token_four,
        "the force fence token is strictly higher"
    );
    assert!(active_lease(&data_directory, &node_id).is_none());

    // The daemon overwrites its mirror with the fence stamp; every intent
    // under the old token is dead from that moment.
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
    assert_eq!(mirror.occupancy_lease_id, lease_four_id);
    assert_eq!(mirror.fencing_token, fence_token);
    assert_eq!(
        mirror.mirror_revision, 5,
        "the fence advances the mirror once"
    );
    let guard = daemon.fencing_guard();
    assert_eq!(
        guard.authorize_command(FencedCommandKind::WorkerLaunch, &lease_four_id, token_four),
        FencingVerdict::Rejected(FencingRejection::StaleFencingToken),
        "the superseded token can never authorize a local command again"
    );
    let FencingVerdict::Authorized(ticket) =
        guard.authorize_command(FencedCommandKind::WorkerLaunch, &lease_four_id, fence_token)
    else {
        panic!("the fenced stamp must authorize");
    };
    assert_eq!(ticket.mirror_revision, 5);
    assert_eq!(
        daemon.status().occupancy_offers_rejected,
        0,
        "the force fence itself must pass the revision judgement"
    );

    let response = http_request(
        address,
        &cookie_get(
            &format!("/api/v1/clients/{public_client_id}/occupancy"),
            &member_cookie,
        ),
    )
    .await;
    assert_eq!(response_body(&response)["occupancy"], json!("available"));

    // ---- Phase 8: the fifth cycle survives the fence -----------------------
    let claim_request = cookie_post(
        "/api/v1/clients/occupancy",
        &occupancy_body(&public_client_id),
        &owner_cookie,
    );
    let claim = tokio::spawn(async move { http_request(address, &claim_request).await });
    drive_until(
        &mut daemon,
        "the fifth offer to settle occupied",
        |daemon| {
            settled(daemon)
                && active_lease(&data_directory, &node_id)
                    .is_some_and(|lease| lease.state == OccupancyLeaseState::Occupied)
        },
    );
    let response = claim.await.expect("claim task");
    assert_eq!(status_of(&response), "201", "{response}");
    let view = response_body(&response);
    let token_five = view["fencingToken"].as_u64().expect("fencing token");
    assert!(token_five > fence_token);
    let mirror = daemon
        .store_mut()
        .occupancy_mirror()
        .expect("mirror read")
        .expect("mirror");
    assert_eq!(mirror.fencing_token, token_five);
    assert_eq!(mirror.mirror_revision, 6);
    assert_eq!(daemon.status().occupancy_offers_rejected, 0);

    let response = http_request(
        address,
        &cookie_delete(
            "/api/v1/clients/occupancy",
            &release_body(&public_client_id, "release"),
            &owner_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "200", "{response}");
    drive_until(&mut daemon, "the final release intent", |daemon| {
        daemon
            .store_mut()
            .occupancy_release_intents()
            .expect("intents read")
            .len()
            == 4
    });

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
    let server_cursors = exchange_cursors(&data_directory, &node_id);
    let inbox = daemon
        .store_mut()
        .inbox_cursor(&config.server_profile_id)
        .expect("cursor read")
        .expect("inbox cursor");
    assert_eq!(
        inbox.last_sequence, server_cursors.server_to_client_ack_sequence,
        "the device's durable downlink cursor equals the server's"
    );
    assert_eq!(
        daemon.outbox_snapshot().expect("snapshot").ack_sequence,
        server_cursors.client_to_server_ack_sequence,
        "the device's durable uplink cursor equals the server's"
    );
    assert!(active_lease(&data_directory, &node_id).is_none());
    assert_eq!(
        node_snapshot(&data_directory, &node_id).presence_state,
        winwincode_storage::ClientPresenceState::Online
    );

    daemon.into_store().close().expect("store close");
    running.shutdown().await.expect("server shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&device_root);
}
