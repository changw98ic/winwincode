// SPDX-License-Identifier: Apache-2.0

//! The user-facing Worker launch flow over real HTTP: the
//! `POST /api/v1/sessions` route wired to `ClientSessionsApplication` and the
//! real client exchange, covering the full chain (grant issued, the
//! `client.worker.launch` frame delivered with every `C + L` field, the fake
//! device answering `client.worker.launch_ack`, the grant consumed exactly
//! once), the repeated-ack idempotency, the rejected acknowledgement keeping
//! the grant `issued` with the audited reason, the occupancy preconditions
//! (`NOT_HOLDER`, `OCCUPANCY_REQUIRED`), the binding visibility gate, the
//! durable capacity view, and the expiry path of an unanswered grant.

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

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
use winwincode_client_port::domain::ClientControlError;
use winwincode_client_port::domain::ClientControlErrorCode;
use winwincode_client_port::domain::ClientLockState;
use winwincode_client_port::domain::ClientPlatformTarget;
use winwincode_client_port::domain::ClientWorkerStopReason;
use winwincode_client_port::domain::PresenceState;
use winwincode_client_port::domain::WorkerLaunchAckStatus;
use winwincode_client_port::exchange::DEFAULT_MAX_FRAME_BYTES;
use winwincode_client_port::exchange::FrameCodec;
use winwincode_client_port::messages::CLIENT_CONTROL_PORT_SCHEMA_VERSION;
use winwincode_client_port::messages::ClientEnrollPayload;
use winwincode_client_port::messages::ClientHelloPayload;
use winwincode_client_port::messages::ClientToServerEnvelope;
use winwincode_client_port::messages::ClientToServerMessage;
use winwincode_client_port::messages::ClientWorkerLaunchAckPayload;
use winwincode_client_port::messages::CommandContext;
use winwincode_client_port::messages::OccupancyCommandContext;
use winwincode_client_port::messages::ServerToClientEnvelope;
use winwincode_control_plane::ClientOccupancyService;
use winwincode_control_plane::ClientRegistryService;
use winwincode_control_plane::ConnectCodeService;
use winwincode_control_plane::WorkerLaunchGrantService;
use winwincode_domain::Instant;
use winwincode_server::{
    ApiError, AuthSessionBootstrap, AuthSessionConfig, AuthenticatedPrincipal,
    ClientExchangeApplication, ClientExchangeConfig, ClientExchangePort, ClientSessionsApplication,
    ClientSessionsConfig, ClientSessionsErrorKind, ControlPlaneApiPort, EventSubscription,
    RequestAuthenticator, ServerConfig, ServerTls, SqliteAuthSessionManager, UserAccountService,
    start_server_with_remote_worker, worker_stop_message,
};
use winwincode_storage::{
    AccessGrantIssuance, ConnectCodeConsume, ConnectCodePublication, GrantTrustMode,
    LaunchAckSettlement, OccupancyClaim, OccupancyLeaseState, RepositoryAccessGrantIssuance,
    RepositoryAvailability, RepositoryBindingProjection, RepositoryDirtyState,
    RepositoryGrantPermissions, SqliteStorage, WorkerLaunchGrantState,
};

const BOOTSTRAP_PROOF: &str = "client-sessions-test-bootstrap";
const ORIGIN: &str = "https://client.example";
const OWNER_PASSWORD: &str = "initial-owner-password";
/// Placeholder client node id a fresh device sends before the server assigns
/// the canonical `cnd_` identity.
const FRESH_NODE: &str = "device-local-pending";
/// Enrollment instance; a following hello from the takeover instance carries
/// the reported capacity the flow needs.
const ENROLL_INSTANCE: &str = "cix_A1A1A1A1A1A1A1A1A1A1A1A1A1";
const DEVICE_INSTANCE: &str = "cix_B2B2B2B2B2B2B2B2B2B2B2B2B2";
const SCHEMA_VERSION: &str = "winwincode/v1";
const VALID_UNTIL: &str = "2100-01-01T00:00:00.000Z";

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
            "unused in session tests",
        ))
    }

    fn query(&self, _: &AuthenticatedPrincipal, _: Value) -> Result<Value, ApiError> {
        Err(ApiError::new(
            501,
            "NOT_IMPLEMENTED",
            "unused in session tests",
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
        organization_id: winwincode_domain::OrganizationId(
            "org_00000000000000000000000001".to_owned(),
        ),
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

async fn start_server(
    data_directory: &Path,
    auth_directory: &Path,
) -> winwincode_server::RunningServer {
    let exchange: Arc<dyn ClientExchangePort> = Arc::new(
        ClientExchangeApplication::open(data_directory, &ClientExchangeConfig::default())
            .expect("valid client exchange application"),
    );
    let sessions = open_auth(auth_directory);
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

fn cookie_post(path: &str, body: &str, cookie: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: control.example\r\nOrigin: {ORIGIN}\r\nCookie: wwc_session={cookie}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
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

/// Initializes the Owner with a password, then signs in; returns
/// (cookie, userId).
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

/// Creates one member account and signs in; returns (cookie, userId).
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

// ---- exchange protocol helpers (device side) ------------------------------

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
        "POST /internal/v1/client/exchange HTTP/1.1\r\nHost: control.example\r\n{authorization}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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

fn frame(node: &str, instance: &str, sequence: u64, message: ClientToServerMessage) -> Value {
    serde_json::to_value(ClientToServerEnvelope {
        schema_version: SCHEMA_VERSION.to_owned(),
        message_id: format!("msg_{sequence:020}"),
        client_node_id: node.to_owned(),
        client_instance_id: instance.to_owned(),
        sequence,
        occurred_at: "2026-09-04T12:00:00.000Z".to_owned(),
        message,
    })
    .expect("frame value")
}

fn exchange_request(frames: &[Value], ack_sequence: u64) -> String {
    json!({
        "schemaVersion": SCHEMA_VERSION,
        "frames": frames,
        "ackSequence": ack_sequence,
    })
    .to_string()
}

fn capacity(running: u32) -> ClientCapacityReport {
    ClientCapacityReport {
        max_concurrent_worker_sessions: 4,
        running_worker_sessions: running,
        reserved_worker_sessions: 0,
        draining_worker_sessions: 0,
    }
}

/// Enrolls a fresh device over the exchange; returns (nodeId, publicClientId,
/// credential). The node stays `pending_enrollment`.
async fn enroll_device(address: std::net::SocketAddr) -> (String, String, String) {
    let enroll = ClientToServerMessage::Enroll(Box::new(ClientEnrollPayload {
        command: CommandContext {
            expected_revision: 0,
            idempotency_key: fresh_code_id(),
        },
        display_name: "Cheng's MacBook".to_owned(),
        platform: ClientPlatformTarget::Aarch64AppleDarwin,
        architecture: ClientArchitecture::Aarch64,
        client_version: "0.1.0-alpha.1".to_owned(),
    }));
    let (status, body) = post_exchange(
        address,
        &exchange_request(&[frame(FRESH_NODE, ENROLL_INSTANCE, 1, enroll)], 0),
        None,
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    let body = body.expect("enrollment response body");
    let enrollment = body.get("enrollment").expect("enrollment issuance");
    let node = enrollment["clientNodeId"]
        .as_str()
        .expect("node id")
        .to_owned();
    let public_client_id = enrollment["publicClientId"]
        .as_str()
        .expect("public client id")
        .to_owned();
    let credential = enrollment["deviceCredential"]
        .as_str()
        .expect("credential")
        .to_owned();
    (node, public_client_id, credential)
}

/// Walks the announcement hello so the node projects `online`.
async fn walk_hello(
    address: std::net::SocketAddr,
    data_directory: &Path,
    node: &str,
    instance: &str,
    credential: &str,
) {
    let hello = ClientToServerMessage::Hello(ClientHelloPayload {
        client_version: "0.1.0-alpha.1".to_owned(),
        capacity: capacity(0),
        accepting_connections: true,
        lock_state: ClientLockState::Unlocked,
        presence_state: PresenceState::Online,
    });
    let sequence = next_client_sequence(data_directory, node);
    let (status, body) = post_exchange(
        address,
        &exchange_request(
            &[frame(node, instance, sequence, hello)],
            downlink_ack_cursor(data_directory, node),
        ),
        Some(credential),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert_eq!(
        body.expect("hello response body")["ackSequence"],
        json!(sequence)
    );
}

/// Enrolls and walks the takeover hello, then sends one real heartbeat: the
/// node is `online` reporting four worker-session slots under the device
/// instance.
async fn enroll_online_device(
    address: std::net::SocketAddr,
    data_directory: &Path,
) -> (String, String, String) {
    let (node, public_client_id, credential) = enroll_device(address).await;
    walk_hello(address, data_directory, &node, DEVICE_INSTANCE, &credential).await;
    send_heartbeat(address, data_directory, &node, &credential, 0).await;
    (node, public_client_id, credential)
}

/// Publishes one connect code digest the way the Device Client would, and
/// returns the 8-digit code.
fn publish_connect_code(data_directory: &Path, node: &str, code: &str) -> String {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut connect = ConnectCodeService::new(&mut storage);
    let now = Instant("2026-09-04T12:00:00.000Z".to_owned());
    let digest = format!("sha256:{:x}", Sha256::digest(code.as_bytes()));
    let publication = ConnectCodePublication::try_new(
        fresh_code_id(),
        digest,
        node,
        DEVICE_INSTANCE,
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

/// Stages one repository binding without any repository access grant: it
/// exists on the client but is invisible to every user.
fn stage_invisible_binding(data_directory: &Path, node: &str) -> String {
    let binding_id = fresh_binding_id();
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut ledger = storage.repository_binding_ledger().expect("ledger");
    let projection = RepositoryBindingProjection::try_new(
        binding_id.clone(),
        node,
        "unshared",
        None,
        None,
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
    binding_id
}

/// Claims occupancy directly and walks the lease to `occupied` through the
/// shared service (no HTTP detour needed for staging).
fn stage_occupied_lease(data_directory: &Path, node: &str, holder_user_id: &str) {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut occupancy = ClientOccupancyService::new(&mut storage);
    let claim = OccupancyClaim::try_new(
        fresh_code_id().replacen("cct_", "ocl_", 1),
        node,
        holder_user_id,
        fresh_code_id().replacen("cct_", "req_", 1),
    )
    .expect("claim");
    let lease = occupancy
        .atomic_claim(&claim, &Instant("2026-09-04T12:01:00.000Z".to_owned()))
        .expect("claim");
    let occupied = occupancy
        .record_acknowledgement(
            &lease.occupancy_lease_id,
            lease.fencing_token,
            None,
            &Instant("2026-09-04T12:01:01.000Z".to_owned()),
        )
        .expect("ack");
    assert_eq!(occupied.state, OccupancyLeaseState::Occupied);
}

/// Rewrites one registry column of a node directly (test staging for device
/// local facts no public API mutates).
fn set_node_column(data_directory: &Path, node: &str, assignment: &str) {
    let connection = rusqlite::Connection::open(data_directory.join("control-plane.sqlite3"))
        .expect("open database");
    connection
        .execute(
            &format!("UPDATE client_nodes SET {assignment} WHERE client_node_id = ?1"),
            [node],
        )
        .expect("node column update");
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

/// Sends one device heartbeat reporting `running` active worker sessions.
async fn send_heartbeat(
    address: std::net::SocketAddr,
    data_directory: &Path,
    node: &str,
    credential: &str,
    running: u32,
) {
    let heartbeat = ClientToServerMessage::Heartbeat(
        winwincode_client_port::messages::ClientHeartbeatPayload {
            capacity: capacity(running),
            accepting_connections: true,
            lock_state: ClientLockState::Unlocked,
            presence_state: PresenceState::Online,
            occupancy_lease_id: None,
        },
    );
    let sequence = next_client_sequence(data_directory, node);
    let (status, _) = post_exchange(
        address,
        &exchange_request(
            &[frame(node, DEVICE_INSTANCE, sequence, heartbeat)],
            downlink_ack_cursor(data_directory, node),
        ),
        Some(credential),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
}

/// How the fake device answers a `client.worker.launch` command.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LaunchVerdict {
    /// Accept: the device persists the launch intent and spawns.
    Accept,
    /// Reject with the stale-fencing-token status (an unused token verdict).
    Reject,
}

/// The fake device: polls the durable downlink outbox like the real daemon
/// polls its inbox, acknowledges every frame, answers every
/// `client.occupancy.offer` with `client.occupancy.ack`, and answers every
/// `client.worker.launch` with a `client.worker.launch_ack` echoing the
/// grant identities and the exact occupancy stamp.
#[allow(clippy::too_many_lines)]
fn spawn_device_responder(
    data_directory: PathBuf,
    address: std::net::SocketAddr,
    node: String,
    credential: String,
    mut inbox_ack: u64,
    mut next_sequence: u64,
    verdict: LaunchVerdict,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let pending = {
                let Ok(mut storage) = SqliteStorage::open(&data_directory) else {
                    continue;
                };
                let Ok(outbox) = storage.client_downlink_outbox() else {
                    continue;
                };
                match outbox.deliverable(&node, inbox_ack, 8) {
                    Ok(frames) => frames,
                    Err(_) => continue,
                }
            };
            for stored in pending {
                let Ok(frame_value) = serde_json::from_str::<Value>(&stored.frame) else {
                    continue;
                };
                inbox_ack = stored.sequence;
                let kind = frame_value["kind"].as_str().unwrap_or_default().to_owned();
                let payload = frame_value["payload"].clone();
                let message = match kind.as_str() {
                    "client.occupancy.offer" => {
                        let lease_id = payload["occupancyLeaseId"]
                            .as_str()
                            .expect("occupancy lease id")
                            .to_owned();
                        let token: u64 = payload["occupancyFencingToken"]
                            .as_str()
                            .expect("wire fencing token is a decimal string")
                            .parse()
                            .expect("fencing token number");
                        ClientToServerMessage::OccupancyAck(
                            winwincode_client_port::messages::ClientOccupancyAckPayload {
                                occupancy: OccupancyCommandContext {
                                    command: CommandContext {
                                        expected_revision: 0,
                                        idempotency_key: format!("idem_ack_{lease_id}"),
                                    },
                                    occupancy_lease_id: lease_id,
                                    occupancy_fencing_token: token,
                                },
                            },
                        )
                    }
                    "client.worker.launch" => {
                        let grant = &payload["launchGrant"];
                        let (status, error) = match verdict {
                            LaunchVerdict::Accept => (WorkerLaunchAckStatus::Accepted, None),
                            LaunchVerdict::Reject => (
                                WorkerLaunchAckStatus::RejectedLeaseMismatch,
                                Some(ClientControlError {
                                    code: ClientControlErrorCode::WrongState,
                                    message: "local state refuses the launch".to_owned(),
                                    retryable: false,
                                }),
                            ),
                        };
                        ClientToServerMessage::WorkerLaunchAck(Box::new(
                            ClientWorkerLaunchAckPayload {
                                occupancy: OccupancyCommandContext {
                                    command: CommandContext {
                                        expected_revision: 0,
                                        idempotency_key: format!(
                                            "idem_launch_ack_{}",
                                            grant["workerLaunchGrantId"]
                                                .as_str()
                                                .expect("grant id")
                                        ),
                                    },
                                    occupancy_lease_id: payload["occupancyLeaseId"]
                                        .as_str()
                                        .expect("occupancy lease id")
                                        .to_owned(),
                                    occupancy_fencing_token: payload["occupancyFencingToken"]
                                        .as_str()
                                        .expect("wire fencing token is a decimal string")
                                        .parse()
                                        .expect("fencing token number"),
                                },
                                worker_launch_grant_id: grant["workerLaunchGrantId"]
                                    .as_str()
                                    .expect("grant id")
                                    .to_owned(),
                                worker_session_id: grant["workerSessionId"]
                                    .as_str()
                                    .expect("worker session id")
                                    .to_owned(),
                                worker_id: grant["workerId"]
                                    .as_str()
                                    .expect("worker id")
                                    .to_owned(),
                                worker_instance_id: grant["workerInstanceId"]
                                    .as_str()
                                    .expect("worker instance id")
                                    .to_owned(),
                                status,
                                error,
                            },
                        ))
                    }
                    _ => continue,
                };
                let request = exchange_request(
                    &[frame(&node, DEVICE_INSTANCE, next_sequence, message)],
                    stored.sequence,
                );
                next_sequence += 1;
                let (status, _) = post_exchange(address, &request, Some(&credential)).await;
                assert!(status.starts_with("HTTP/1.1 200"), "{status}");
            }
        }
    })
}

// ---- launch flow helpers ---------------------------------------------------

fn launch_body(client_id: &str, binding_id: &str) -> String {
    json!({
        "schemaVersion": SCHEMA_VERSION,
        "clientId": client_id,
        "repositoryBindingId": binding_id,
    })
    .to_string()
}

/// Reads the durable launch grant audit actions of one grant, oldest first.
fn audit_actions(data_directory: &Path, grant_id: &str) -> Vec<(String, Option<String>)> {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut grants = WorkerLaunchGrantService::new(&mut storage);
    grants
        .audit_trail(grant_id)
        .expect("audit trail")
        .into_iter()
        .map(|entry| (entry.action.as_str().to_owned(), entry.reason))
        .collect()
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

/// Polls `predicate` until it holds or the budget elapses (the fake responder
/// settles its acks through real exchange round trips; awaiting keeps the
/// single-threaded test runtime responsive).
async fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    predicate()
}

// ---- tests ----------------------------------------------------------------

#[tokio::test]
async fn launch_routes_require_a_signed_in_session() {
    let data_directory = test_directory("sessions-auth");
    let auth_directory = test_directory("sessions-auth-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();

    let response = http_request(
        address,
        &plain_post(
            "/api/v1/sessions",
            &launch_body("927351842", "rbd_00000000000000000000000001"),
        ),
    )
    .await;
    assert_eq!(status_of(&response), "401", "{response}");

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn full_launch_chain_issues_consumes_and_is_idempotent_under_replays() {
    let data_directory = test_directory("sessions-happy");
    let auth_directory = test_directory("sessions-happy-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (cookie, user_id) = initialize_and_login_owner(address).await;

    let (node, public_client_id, credential) = enroll_online_device(address, &data_directory).await;
    let code = publish_connect_code(&data_directory, &node, "68421975");
    consume_code_as(&data_directory, &node, &code, &user_id);
    let binding_id = stage_visible_binding(&data_directory, &node, &user_id);
    stage_occupied_lease(&data_directory, &node, &user_id);

    let responder = spawn_device_responder(
        data_directory.clone(),
        address,
        node.clone(),
        credential.clone(),
        downlink_ack_cursor(&data_directory, &node),
        next_client_sequence(&data_directory, &node),
        LaunchVerdict::Accept,
    );

    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/sessions",
            &launch_body(&public_client_id, &binding_id),
            &cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "201", "{response}");
    let body = response_body(&response);
    assert_eq!(body["schemaVersion"], json!(SCHEMA_VERSION));
    assert_eq!(body["clientId"], json!(public_client_id));
    assert_eq!(body["repositoryBindingId"], json!(binding_id));
    assert!(
        body["workerLaunchGrantId"]
            .as_str()
            .expect("grant id")
            .starts_with("wlg_"),
        "{body}"
    );
    assert!(
        body["workerSessionId"]
            .as_str()
            .expect("session id")
            .starts_with("ws_"),
        "{body}"
    );
    assert!(
        body["workerId"]
            .as_str()
            .expect("worker id")
            .starts_with("wkr_"),
        "{body}"
    );
    assert!(
        body["workerInstanceId"]
            .as_str()
            .expect("worker instance id")
            .starts_with("winst_"),
        "{body}"
    );
    // The raw 32-byte credential material crosses this response exactly once;
    // durable state stores only its digest.
    let material = body["workerCredential"]
        .as_str()
        .expect("credential material");
    assert_eq!(material.len(), 64, "{body}");
    let digest = body["credentialDigest"]
        .as_str()
        .expect("credential digest");
    assert_eq!(digest.len(), 71);
    assert!(digest.starts_with("sha256:"));
    let secret: Vec<u8> = (0..material.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&material[index..index + 2], 16).expect("hex"))
        .collect();
    assert_eq!(
        digest,
        format!("sha256:{:x}", Sha256::digest(&secret)),
        "the stored digest binds the response material"
    );

    // Durable facts: the grant is consumed exactly once with its audit pair,
    // and the acked launch frame is retained no longer.
    let grant_id = body["workerLaunchGrantId"]
        .as_str()
        .expect("grant id")
        .to_owned();
    let consumed = wait_until(|| {
        launch_grant(&data_directory, &grant_id).state == WorkerLaunchGrantState::Consumed
    })
    .await;
    assert!(consumed, "the device ack consumed the grant");
    let grant = launch_grant(&data_directory, &grant_id);
    assert_eq!(grant.state, WorkerLaunchGrantState::Consumed);
    assert!(grant.consumed_at.is_some());
    assert_eq!(grant.holder_user_id, user_id);
    {
        let mut storage = SqliteStorage::open(&data_directory).expect("storage");
        let outbox = storage.client_downlink_outbox().expect("outbox");
        assert_eq!(
            outbox.deliverable(&node, 0, 100).expect("retained").len(),
            0,
            "the acked launch frame is retained no longer"
        );
    }

    // A replayed accepted acknowledgement is an idempotent no-op: the grant
    // keeps its revision and the audit trail keeps exactly two entries.
    let settlement = LaunchAckSettlement::try_new(
        &grant.worker_launch_grant_id,
        &grant.occupancy_lease_id,
        grant.occupancy_fencing_token,
        &grant.worker_session_id,
        &grant.worker_id,
        &grant.worker_instance_id,
        true,
        None,
    )
    .expect("replay settlement");
    {
        let mut storage = SqliteStorage::open(&data_directory).expect("storage");
        let mut grants = WorkerLaunchGrantService::new(&mut storage);
        let outcome = grants
            .settle_launch_ack(&settlement, &Instant("2026-09-04T12:05:00.000Z".to_owned()))
            .expect("replay settle");
        assert_eq!(
            outcome,
            winwincode_storage::LaunchAckOutcome::AlreadyConsumed
        );
    }
    let replayed = launch_grant(&data_directory, &grant_id);
    assert_eq!(
        replayed.revision, grant.revision,
        "the replay never rewrote"
    );
    let trail = audit_actions(&data_directory, &grant_id);
    assert_eq!(
        trail,
        vec![("issued".to_owned(), None), ("consumed".to_owned(), None),]
    );

    responder.abort();
    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn occupancy_preconditions_binding_visibility_and_capacity_gate_the_launch() {
    let data_directory = test_directory("sessions-gates");
    let auth_directory = test_directory("sessions-gates-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (owner_cookie, owner_id) = initialize_and_login_owner(address).await;
    let (member_cookie, _member_id) =
        create_and_login_member(address, &owner_cookie, "member").await;

    let (node, public_client_id, credential) = enroll_online_device(address, &data_directory).await;
    let code = publish_connect_code(&data_directory, &node, "11112222");
    consume_code_as(&data_directory, &node, &code, &owner_id);
    let binding_id = stage_visible_binding(&data_directory, &node, &owner_id);
    let invisible_binding = stage_invisible_binding(&data_directory, &node);

    // No occupancy at all: the launch requires the holder's occupied lease.
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/sessions",
            &launch_body(&public_client_id, &binding_id),
            &owner_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "409", "{response}");
    assert_eq!(wire_code(&response), "OCCUPANCY_REQUIRED");

    // The owner occupies the device; the member is not the holder.
    let responder = spawn_device_responder(
        data_directory.clone(),
        address,
        node.clone(),
        credential,
        downlink_ack_cursor(&data_directory, &node),
        next_client_sequence(&data_directory, &node),
        LaunchVerdict::Accept,
    );
    let claim = json!({
        "schemaVersion": SCHEMA_VERSION,
        "clientId": public_client_id,
    })
    .to_string();
    let response = http_request(
        address,
        &cookie_post("/api/v1/clients/occupancy", &claim, &owner_cookie),
    )
    .await;
    assert_eq!(status_of(&response), "201", "{response}");
    responder.abort();

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

    // A binding without an active repository grant is invisible to the
    // holder, even while they occupy the client.
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/sessions",
            &launch_body(&public_client_id, &invisible_binding),
            &owner_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "403", "{response}");
    assert_eq!(wire_code(&response), "BINDING_NOT_VISIBLE");

    // An unknown binding reads the same way and never confirms existence.
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/sessions",
            &launch_body(&public_client_id, "rbd_99999999999999999999999999"),
            &owner_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "403", "{response}");
    assert_eq!(wire_code(&response), "BINDING_NOT_VISIBLE");

    // Capacity zero: the durable reservation view has no free slot.
    set_node_column(&data_directory, &node, "max_concurrent_worker_sessions = 0");
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/sessions",
            &launch_body(&public_client_id, &binding_id),
            &owner_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "409", "{response}");
    assert_eq!(wire_code(&response), "CAPACITY_EXHAUSTED");

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[tokio::test]
async fn an_unconfirmed_occupancy_refuses_the_launch() {
    let data_directory = test_directory("sessions-reserving");
    let auth_directory = test_directory("sessions-reserving-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (cookie, user_id) = initialize_and_login_owner(address).await;

    let (node, public_client_id, _credential) =
        enroll_online_device(address, &data_directory).await;
    let code = publish_connect_code(&data_directory, &node, "22223333");
    consume_code_as(&data_directory, &node, &code, &user_id);
    let binding_id = stage_visible_binding(&data_directory, &node, &user_id);
    // The offer was created but the device never confirmed it: the lease
    // stays `reserving`, which does not authorize launches.
    {
        let mut storage = SqliteStorage::open(&data_directory).expect("storage");
        let mut occupancy = ClientOccupancyService::new(&mut storage);
        let claim = OccupancyClaim::try_new(
            fresh_code_id().replacen("cct_", "ocl_", 1),
            &node,
            &user_id,
            fresh_code_id().replacen("cct_", "req_", 1),
        )
        .expect("claim");
        let lease = occupancy
            .atomic_claim(&claim, &Instant("2026-09-04T12:01:00.000Z".to_owned()))
            .expect("claim");
        assert_eq!(lease.state, OccupancyLeaseState::Reserving);
    }

    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/sessions",
            &launch_body(&public_client_id, &binding_id),
            &cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "409", "{response}");
    assert_eq!(wire_code(&response), "OCCUPANCY_REQUIRED");

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[tokio::test]
async fn a_rejected_launch_ack_keeps_the_grant_issued_and_audits_the_reason() {
    let data_directory = test_directory("sessions-reject");
    let auth_directory = test_directory("sessions-reject-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (cookie, user_id) = initialize_and_login_owner(address).await;

    let (node, public_client_id, credential) = enroll_online_device(address, &data_directory).await;
    let code = publish_connect_code(&data_directory, &node, "33334444");
    consume_code_as(&data_directory, &node, &code, &user_id);
    let binding_id = stage_visible_binding(&data_directory, &node, &user_id);
    stage_occupied_lease(&data_directory, &node, &user_id);

    let responder = spawn_device_responder(
        data_directory.clone(),
        address,
        node.clone(),
        credential,
        downlink_ack_cursor(&data_directory, &node),
        next_client_sequence(&data_directory, &node),
        LaunchVerdict::Reject,
    );

    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/sessions",
            &launch_body(&public_client_id, &binding_id),
            &cookie,
        ),
    )
    .await;
    responder.abort();
    assert_eq!(status_of(&response), "409", "{response}");
    assert_eq!(wire_code(&response), "LAUNCH_REJECTED");

    let grant_id = {
        let connection = rusqlite::Connection::open(data_directory.join("control-plane.sqlite3"))
            .expect("open database");
        connection
            .query_row(
                "SELECT worker_launch_grant_id FROM worker_launch_grants
                 ORDER BY created_at DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("newest grant")
    };
    // The route failed fast on the settled rejection (not on the timeout)
    // because the audit trail carries the verdict the state machine keeps
    // implicit: the grant stayed `issued`.
    let settled = wait_until(|| {
        audit_actions(&data_directory, &grant_id)
            .iter()
            .any(|(action, _)| action == "launch_rejected")
    })
    .await;
    assert!(settled, "the rejection reason landed in the audit trail");
    let grant = launch_grant(&data_directory, &grant_id);
    assert_eq!(grant.state, WorkerLaunchGrantState::Issued, "{grant:?}");
    assert_eq!(grant.revision, 1, "a rejection never rewrites the grant");
    let trail = audit_actions(&data_directory, &grant_id);
    let rejected = trail
        .iter()
        .find(|(action, _)| action == "launch_rejected")
        .expect("launch_rejected audit");
    assert!(
        rejected
            .1
            .as_deref()
            .expect("rejection reason")
            .starts_with("rejected_lease_mismatch"),
        "{rejected:?}"
    );

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn an_unanswered_grant_times_out_keeps_the_frame_and_expires() {
    let data_directory = test_directory("sessions-expiry");
    let auth_directory = test_directory("sessions-expiry-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (_cookie, user_id) = initialize_and_login_owner(address).await;

    let (node, public_client_id, _credential) =
        enroll_online_device(address, &data_directory).await;
    let code = publish_connect_code(&data_directory, &node, "44445555");
    consume_code_as(&data_directory, &node, &code, &user_id);
    let binding_id = stage_visible_binding(&data_directory, &node, &user_id);
    stage_occupied_lease(&data_directory, &node, &user_id);

    // No responder: the bounded wait fails, the grant stays `issued`, and
    // the launch frame stays retained in the durable outbox with every
    // `C + L` field.
    let application = ClientSessionsApplication::open(
        &data_directory,
        &ClientSessionsConfig {
            launch_wait: Duration::from_millis(300),
            poll_interval: Duration::from_millis(25),
            grant_ttl: Duration::from_millis(400),
        },
    )
    .expect("valid application");
    let request = json!({
        "schemaVersion": SCHEMA_VERSION,
        "clientId": public_client_id,
        "repositoryBindingId": binding_id,
    });
    let error = application
        .launch(&user_id, &request)
        .await
        .expect_err("the bounded wait must fail");
    assert_eq!(
        error.kind(),
        ClientSessionsErrorKind::LaunchAckTimeout,
        "{error}"
    );

    let grant_id = {
        let connection = rusqlite::Connection::open(data_directory.join("control-plane.sqlite3"))
            .expect("open database");
        connection
            .query_row(
                "SELECT worker_launch_grant_id FROM worker_launch_grants
                 ORDER BY created_at DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("newest grant")
    };
    let grant = launch_grant(&data_directory, &grant_id);
    assert_eq!(grant.state, WorkerLaunchGrantState::Issued);
    {
        let mut storage = SqliteStorage::open(&data_directory).expect("storage");
        let outbox = storage.client_downlink_outbox().expect("outbox");
        let frames = outbox.deliverable(&node, 0, 8).expect("retained frames");
        let launch_frame = frames
            .iter()
            .find(|stored| {
                serde_json::from_str::<Value>(&stored.frame)
                    .is_ok_and(|value| value["kind"] == json!("client.worker.launch"))
            })
            .expect("launch frame retained");
        let value: Value = serde_json::from_str(&launch_frame.frame).expect("frame json");
        let payload = &value["payload"];
        // The `C + L` command fields.
        assert_eq!(payload["occupancyLeaseId"], json!(grant.occupancy_lease_id));
        assert_eq!(
            payload["occupancyFencingToken"],
            json!(grant.occupancy_fencing_token.to_string()),
            "the wire fencing token is a decimal string"
        );
        assert_eq!(
            payload["idempotencyKey"],
            json!(format!("idem_launch_{}", grant.worker_launch_grant_id))
        );
        assert!(payload["expectedRevision"].is_u64());
        // The full grant binding.
        let launch_grant_value = &payload["launchGrant"];
        assert_eq!(
            launch_grant_value["workerLaunchGrantId"],
            json!(grant.worker_launch_grant_id)
        );
        assert_eq!(launch_grant_value["clientNodeId"], json!(node));
        assert_eq!(
            launch_grant_value["clientInstanceId"],
            json!(grant.client_instance_id)
        );
        assert_eq!(launch_grant_value["repositoryBindingId"], json!(binding_id));
        assert_eq!(
            launch_grant_value["workerSessionId"],
            json!(grant.worker_session_id)
        );
        assert_eq!(launch_grant_value["workerId"], json!(grant.worker_id));
        assert_eq!(
            launch_grant_value["workerInstanceId"],
            json!(grant.worker_instance_id)
        );
        assert_eq!(
            launch_grant_value["credentialDigest"],
            json!(grant.credential_digest)
        );
        assert_eq!(launch_grant_value["expiresAt"], json!(grant.expires_at.0));
        assert_eq!(launch_grant_value["state"], json!("issued"));
        assert!(launch_grant_value["productSessionId"].is_string());
        assert!(launch_grant_value["stageRunId"].is_string());
        // The frame never carries the raw credential material.
        assert!(
            !value.to_string().contains("workerCredential"),
            "the raw material never goes downlink"
        );
    }

    // A late accepted acknowledgement after the expiry deadline is refused
    // and changes nothing; the sweep then terminates the grant. The late
    // instants derive from the grant's own expiry (a far-future year), so
    // the test never depends on the wall clock.
    let late = Instant(format!("2099{}", &grant.expires_at.0[4..]));
    let settlement = LaunchAckSettlement::try_new(
        &grant.worker_launch_grant_id,
        &grant.occupancy_lease_id,
        grant.occupancy_fencing_token,
        &grant.worker_session_id,
        &grant.worker_id,
        &grant.worker_instance_id,
        true,
        None,
    )
    .expect("late settlement");
    {
        let mut storage = SqliteStorage::open(&data_directory).expect("storage");
        let mut grants = WorkerLaunchGrantService::new(&mut storage);
        let error = grants
            .settle_launch_ack(&settlement, &late)
            .expect_err("an expired grant must refuse consumption");
        assert_eq!(
            error.kind(),
            winwincode_control_plane::WorkerLaunchGrantServiceErrorKind::GrantExpired
        );
        let expired = grants.expire(&late).expect("expire");
        assert_eq!(expired, vec![grant.worker_launch_grant_id.clone()]);
    }
    let expired_grant = launch_grant(&data_directory, &grant_id);
    assert_eq!(expired_grant.state, WorkerLaunchGrantState::Expired);
    let trail = audit_actions(&data_directory, &grant_id);
    assert_eq!(
        trail.last().expect("audit").0,
        "expired",
        "the expiry landed in the audit trail"
    );

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[test]
fn the_worker_stop_frame_round_trips_through_the_wire_codec() {
    let message = worker_stop_message(
        OccupancyCommandContext {
            command: CommandContext {
                expected_revision: 3,
                idempotency_key: "idem_stop_wlg_1".to_owned(),
            },
            occupancy_lease_id: "ocl_AAAAAAAAAAAAAAAAAAAAAAAA1".to_owned(),
            occupancy_fencing_token: 7,
        },
        "ws_AAAAAAAAAAAAAAAAAAAAAAAAA1",
        "wkr_AAAAAAAAAAAAAAAAAAAAAAAAA1",
        ClientWorkerStopReason::GrantRevoked,
    );
    let envelope = ServerToClientEnvelope {
        schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
        message_id: "msg_AAAAAAAAAAAAAAAAAAAAAAAA1".to_owned(),
        client_node_id: "cnd_AAAAAAAAAAAAAAAAAAAAAAAA1".to_owned(),
        client_instance_id: "cix_AAAAAAAAAAAAAAAAAAAAAAAA1".to_owned(),
        sequence: 1,
        occurred_at: "2026-09-04T12:00:00.000Z".to_owned(),
        message,
    };
    let codec = FrameCodec::new(DEFAULT_MAX_FRAME_BYTES);
    let encoded = codec.encode_envelope(&envelope).expect("encode");
    let decoded: Value = serde_json::from_slice(&encoded.frame).expect("wire frame is JSON");
    assert_eq!(decoded["kind"], json!("client.worker.stop"));
    assert_eq!(
        decoded["payload"]["workerSessionId"],
        json!("ws_AAAAAAAAAAAAAAAAAAAAAAAAA1")
    );
    assert_eq!(
        decoded["payload"]["workerId"],
        json!("wkr_AAAAAAAAAAAAAAAAAAAAAAAAA1")
    );
    assert_eq!(decoded["payload"]["reason"], json!("grant_revoked"));
    assert_eq!(
        decoded["payload"]["occupancyLeaseId"],
        json!("ocl_AAAAAAAAAAAAAAAAAAAAAAAA1")
    );
    assert_eq!(
        decoded["payload"]["occupancyFencingToken"],
        json!("7"),
        "the wire fencing token is a decimal string"
    );
    assert_eq!(
        decoded["payload"]["idempotencyKey"],
        json!("idem_stop_wlg_1")
    );
    assert_eq!(decoded["payload"]["expectedRevision"], json!(3));
}
