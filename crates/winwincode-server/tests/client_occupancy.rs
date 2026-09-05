// SPDX-License-Identifier: Apache-2.0

//! The user-facing Client occupancy flow over real HTTP: the four
//! `/api/v1/clients/occupancy` routes wired to `ClientOccupancyApplication`
//! and the real client exchange, covering the full claim chain driven by a
//! fake device that answers `client.occupancy.offer` with
//! `client.occupancy.ack` over the exchange protocol, the exactly-one-winner
//! concurrent claim gate, the unanswered-offer rollback, the non-holder
//! privacy projection, the three release modes with their downlink frames,
//! the offline sweep into `recovery_pending` and its preemption block, the
//! token-stable recovery resume, and the Owner-only overdue force release.

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
use winwincode_client_port::domain::ClientLockState;
use winwincode_client_port::domain::ClientPlatformTarget;
use winwincode_client_port::domain::PresenceState;
use winwincode_client_port::messages::ClientEnrollPayload;
use winwincode_client_port::messages::ClientHeartbeatPayload;
use winwincode_client_port::messages::ClientHelloPayload;
use winwincode_client_port::messages::ClientOccupancyAckPayload;
use winwincode_client_port::messages::ClientToServerEnvelope;
use winwincode_client_port::messages::ClientToServerMessage;
use winwincode_client_port::messages::CommandContext;
use winwincode_client_port::messages::OccupancyCommandContext;
use winwincode_control_plane::ClientOccupancyService;
use winwincode_control_plane::ClientRegistryService;
use winwincode_control_plane::ConnectCodeService;
use winwincode_domain::Instant;
use winwincode_server::{
    ApiError, AuthSessionBootstrap, AuthSessionConfig, AuthenticatedPrincipal,
    ClientExchangeApplication, ClientExchangeConfig, ClientExchangePort,
    ClientOccupancyApplication, ClientOccupancyConfig, ClientOccupancyErrorKind,
    ControlPlaneApiPort, EventSubscription, RequestAuthenticator, ServerConfig, ServerTls,
    SqliteAuthSessionManager, UserAccountService, start_server_with_remote_worker,
};
use winwincode_storage::{
    AccessGrantIssuance, ConnectCodeConsume, ConnectCodePublication, GrantTrustMode,
    OccupancyLeaseState, OccupancyReconcileTarget, SqliteStorage,
};

const BOOTSTRAP_PROOF: &str = "client-occupancy-test-bootstrap";
const ORIGIN: &str = "https://client.example";
const OWNER_PASSWORD: &str = "initial-owner-password";
/// Placeholder client node id a fresh device sends before the server assigns
/// the canonical `cnd_` identity.
const FRESH_NODE: &str = "device-local-pending";
/// Enrollment instance; a following hello from the takeover instance carries
/// the reported capacity the claim gate needs.
const ENROLL_INSTANCE: &str = "cix_A1A1A1A1A1A1A1A1A1A1A1A1A1";
const DEVICE_INSTANCE: &str = "cix_B2B2B2B2B2B2B2B2B2B2B2B2B2";
const SCHEMA_VERSION: &str = "winwincode/v1";
const VALID_UNTIL: &str = "2100-01-01T00:00:00.000Z";

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
            "unused in client occupancy tests",
        ))
    }

    fn query(&self, _: &AuthenticatedPrincipal, _: Value) -> Result<Value, ApiError> {
        Err(ApiError::new(
            501,
            "NOT_IMPLEMENTED",
            "unused in client occupancy tests",
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
/// credential, next client-to-server sequence). The node stays
/// `pending_enrollment`.
async fn enroll_device(address: std::net::SocketAddr) -> (String, String, String, u64) {
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
    (node, public_client_id, credential, 2)
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
/// instance, with a heartbeat instant the offline sweep can age out.
async fn enroll_online_device(
    address: std::net::SocketAddr,
    data_directory: &Path,
) -> (String, String, String) {
    let (node, public_client_id, credential, _next) = enroll_device(address).await;
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

/// Counts terminal occupancy leases by their release reason (the audit
/// history behind the availability projection).
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
    let heartbeat = ClientToServerMessage::Heartbeat(ClientHeartbeatPayload {
        capacity: capacity(running),
        accepting_connections: true,
        lock_state: ClientLockState::Unlocked,
        presence_state: PresenceState::Online,
        occupancy_lease_id: None,
    });
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

/// The fake device: polls the durable downlink outbox like the real daemon
/// polls its inbox, acknowledges every frame, and answers every
/// `client.occupancy.offer` with a `client.occupancy.ack` echoing the exact
/// lease and fencing token.
fn spawn_offer_responder(
    data_directory: PathBuf,
    address: std::net::SocketAddr,
    node: String,
    credential: String,
    mut inbox_ack: u64,
    mut next_sequence: u64,
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
                if frame_value["kind"] != json!("client.occupancy.offer") {
                    continue;
                }
                let payload = &frame_value["payload"];
                let lease_id = payload["occupancyLeaseId"]
                    .as_str()
                    .expect("occupancy lease id")
                    .to_owned();
                let token: u64 = payload["occupancyFencingToken"]
                    .as_str()
                    .expect("wire fencing token is a decimal string")
                    .parse()
                    .expect("fencing token number");
                let ack_frame = ClientToServerMessage::OccupancyAck(ClientOccupancyAckPayload {
                    occupancy: OccupancyCommandContext {
                        command: CommandContext {
                            expected_revision: 0,
                            idempotency_key: format!("idem_ack_{lease_id}"),
                        },
                        occupancy_lease_id: lease_id,
                        occupancy_fencing_token: token,
                    },
                });
                let request = exchange_request(
                    &[frame(&node, DEVICE_INSTANCE, next_sequence, ack_frame)],
                    stored.sequence,
                );
                next_sequence += 1;
                let (status, _) = post_exchange(address, &request, Some(&credential)).await;
                assert!(status.starts_with("HTTP/1.1 200"), "{status}");
            }
        }
    })
}

// ---- occupancy HTTP helpers ------------------------------------------------

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

async fn claim_occupied(address: std::net::SocketAddr, client_id: &str, cookie: &str) {
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/occupancy",
            &occupancy_body(client_id),
            cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "201", "{response}");
}

/// Reads the active occupancy lease of one node directly from storage.
fn active_lease(
    data_directory: &Path,
    node: &str,
) -> Option<winwincode_storage::OccupancyLeaseRecord> {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut occupancy = ClientOccupancyService::new(&mut storage);
    occupancy
        .active_lease_for_node(node)
        .expect("active lease lookup")
}

/// Polls `predicate` until it holds or the budget elapses (the fake responder
/// settles its ack through a real exchange round trip; awaiting keeps the
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
async fn occupancy_routes_require_a_signed_in_session() {
    let data_directory = test_directory("occupancy-auth");
    let auth_directory = test_directory("occupancy-auth-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();

    let response = http_request(
        address,
        &plain_post("/api/v1/clients/occupancy", &occupancy_body("927351842")),
    )
    .await;
    assert_eq!(status_of(&response), "401");
    assert_eq!(wire_code(&response), "AUTHENTICATION_REQUIRED");

    let response = http_request(
        address,
        &cookie_delete(
            "/api/v1/clients/occupancy",
            &release_body("927351842", "release"),
            "not-a-session",
        ),
    )
    .await;
    assert_eq!(status_of(&response), "401");

    let response = http_request(
        address,
        &plain_post(
            "/api/v1/clients/occupancy/force-release",
            &occupancy_body("927351842"),
        ),
    )
    .await;
    assert_eq!(status_of(&response), "401");

    let response = http_request(
        address,
        &cookie_get("/api/v1/clients/927351842/occupancy", "not-a-session"),
    )
    .await;
    assert_eq!(status_of(&response), "401");

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[tokio::test]
async fn claim_completes_through_offer_ack_and_projects_occupied() {
    let data_directory = test_directory("occupancy-happy");
    let auth_directory = test_directory("occupancy-happy-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (cookie, user_id) = initialize_and_login_owner(address).await;

    let (node, public_client_id, credential) = enroll_online_device(address, &data_directory).await;
    let code = publish_connect_code(&data_directory, &node, "68421975");
    consume_code_as(&data_directory, &node, &code, &user_id);
    let _responder = spawn_offer_responder(
        data_directory.clone(),
        address,
        node.clone(),
        credential,
        downlink_ack_cursor(&data_directory, &node),
        next_client_sequence(&data_directory, &node),
    );

    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/occupancy",
            &occupancy_body(&public_client_id),
            &cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "201", "{response}");
    let value = response_body(&response);
    assert_eq!(value["schemaVersion"], json!(SCHEMA_VERSION));
    assert_eq!(value["clientId"], json!(public_client_id));
    assert_eq!(value["occupancy"], json!("occupied"));
    assert_eq!(value["holderUserId"], json!(user_id));
    assert!(
        value["occupancyLeaseId"]
            .as_str()
            .expect("lease id")
            .starts_with("ocl_")
    );
    let fencing_token = value["fencingToken"].as_u64().expect("fencing token");
    assert!(fencing_token >= 1);
    assert_eq!(value["capacityTotal"], json!(4));

    // Durable facts: the lease is occupied with the ACK recorded, and the
    // acked offer frame is retained no longer.
    let settled = wait_until(|| {
        active_lease(&data_directory, &node)
            .is_some_and(|lease| lease.state == OccupancyLeaseState::Occupied)
    })
    .await;
    assert!(settled, "the device ack promoted the lease to occupied");
    let lease = active_lease(&data_directory, &node).expect("occupied lease");
    assert_eq!(lease.fencing_token, fencing_token);
    assert_eq!(lease.holder_user_id, user_id);
    assert!(lease.acknowledged_at.is_some());
    {
        let mut storage = SqliteStorage::open(&data_directory).expect("storage");
        let outbox = storage.client_downlink_outbox().expect("outbox");
        assert_eq!(
            outbox.deliverable(&node, 0, 100).expect("retained").len(),
            0,
            "the acked offer frame is retained no longer"
        );
    }

    // The holder reads the full projection.
    let response = http_request(
        address,
        &cookie_get(
            &format!("/api/v1/clients/{public_client_id}/occupancy"),
            &cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "200", "{response}");
    let view = response_body(&response);
    assert_eq!(view["occupancy"], json!("occupied"));
    assert_eq!(view["holderUserId"], json!(user_id));
    assert_eq!(view["fencingToken"], json!(fencing_token));

    // An idempotent re-claim returns the same occupation without a second
    // lease.
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/occupancy",
            &occupancy_body(&public_client_id),
            &cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "201", "{response}");
    assert_eq!(
        response_body(&response)["occupancyLeaseId"],
        value["occupancyLeaseId"]
    );

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn concurrent_claims_exactly_one_wins_and_missing_grants_are_denied() {
    let data_directory = test_directory("occupancy-race");
    let auth_directory = test_directory("occupancy-race-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (owner_cookie, owner_id) = initialize_and_login_owner(address).await;
    let (member_cookie, member_id) =
        create_and_login_member(address, &owner_cookie, "member").await;
    assert_ne!(owner_id, member_id);

    let (node, public_client_id, credential) = enroll_online_device(address, &data_directory).await;

    // A user without an active use grant fails the grant condition.
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/occupancy",
            &occupancy_body(&public_client_id),
            &member_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "403", "{response}");
    assert_eq!(wire_code(&response), "ACCESS_DENIED");

    consume_code_as(
        &data_directory,
        &node,
        &publish_connect_code(&data_directory, &node, "11112222"),
        &owner_id,
    );
    consume_code_as(
        &data_directory,
        &node,
        &publish_connect_code(&data_directory, &node, "22223333"),
        &member_id,
    );
    let responder = spawn_offer_responder(
        data_directory.clone(),
        address,
        node.clone(),
        credential,
        downlink_ack_cursor(&data_directory, &node),
        next_client_sequence(&data_directory, &node),
    );

    let owner_request = cookie_post(
        "/api/v1/clients/occupancy",
        &occupancy_body(&public_client_id),
        &owner_cookie,
    );
    let member_request = cookie_post(
        "/api/v1/clients/occupancy",
        &occupancy_body(&public_client_id),
        &member_cookie,
    );
    let (owner_response, member_response) = tokio::join!(
        http_request(address, &owner_request),
        http_request(address, &member_request)
    );
    let owner_status = status_of(&owner_response);
    let member_status = status_of(&member_response);
    assert_ne!(
        owner_status, member_status,
        "{owner_response} {member_response}"
    );
    let (winner_body, loser_code) = if owner_status == "201" {
        assert_eq!(member_status, "409", "{member_response}");
        assert_eq!(wire_code(&member_response), "OCCUPIED_BY_OTHER");
        (response_body(&owner_response), "OCCUPIED_BY_OTHER")
    } else {
        assert_eq!(owner_status, "409", "{owner_response}");
        assert_eq!(wire_code(&owner_response), "OCCUPIED_BY_OTHER");
        (response_body(&member_response), "OCCUPIED_BY_OTHER")
    };
    let holder = winner_body["holderUserId"].as_str().expect("holder");
    assert!(
        holder == owner_id || holder == member_id,
        "exactly one granted user holds the lease"
    );

    // Exactly one active lease exists and it belongs to the winner.
    let lease = active_lease(&data_directory, &node).expect("the winning lease");
    assert_eq!(lease.state, OccupancyLeaseState::Occupied);
    assert_eq!(lease.holder_user_id, holder);
    let _ = loser_code;

    responder.abort();
    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[tokio::test]
async fn unanswered_offer_times_out_rolls_back_and_feeds_the_rate_limit() {
    let data_directory = test_directory("occupancy-timeout");
    let auth_directory = test_directory("occupancy-timeout-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (_cookie, user_id) = initialize_and_login_owner(address).await;

    let (node, public_client_id, _credential) =
        enroll_online_device(address, &data_directory).await;
    consume_code_as(
        &data_directory,
        &node,
        &publish_connect_code(&data_directory, &node, "55556666"),
        &user_id,
    );
    // No responder: the durable offer stays unanswered until the deadline.
    let application = ClientOccupancyApplication::open(
        &data_directory,
        &ClientOccupancyConfig {
            offer_wait: Duration::from_millis(300),
            poll_interval: Duration::from_millis(25),
            ..ClientOccupancyConfig::default()
        },
    )
    .expect("valid application");
    let request = json!({
        "schemaVersion": SCHEMA_VERSION,
        "clientId": public_client_id,
    });

    for _ in 0..5 {
        let error = application
            .claim(&user_id, &request)
            .await
            .expect_err("the bounded wait must fail");
        assert_eq!(
            error.kind(),
            ClientOccupancyErrorKind::OccupancyAckTimeout,
            "{error}"
        );
        assert!(
            active_lease(&data_directory, &node).is_none(),
            "the rolled-back lease is terminal"
        );
    }
    // Every rolled-back offer ended as `released` with the `ack_timeout`
    // reason: the terminal rows are the audit history behind the projection.
    assert_eq!(count_released_leases(&data_directory, "ack_timeout"), 5);

    // Five failures reached the fixed-window threshold: the next claim is
    // throttled before any lease work happens.
    let throttled = application
        .claim(&user_id, &request)
        .await
        .expect_err("the throttled attempt must fail");
    assert_eq!(
        throttled.kind(),
        ClientOccupancyErrorKind::RateLimited,
        "{throttled}"
    );

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[tokio::test]
async fn non_holder_gets_only_the_occupied_by_other_privacy_projection() {
    let data_directory = test_directory("occupancy-privacy");
    let auth_directory = test_directory("occupancy-privacy-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (owner_cookie, owner_id) = initialize_and_login_owner(address).await;
    let (member_cookie, _member_id) =
        create_and_login_member(address, &owner_cookie, "member").await;

    let (node, public_client_id, credential) = enroll_online_device(address, &data_directory).await;
    consume_code_as(
        &data_directory,
        &node,
        &publish_connect_code(&data_directory, &node, "77778888"),
        &owner_id,
    );
    let responder = spawn_offer_responder(
        data_directory.clone(),
        address,
        node.clone(),
        credential,
        downlink_ack_cursor(&data_directory, &node),
        next_client_sequence(&data_directory, &node),
    );
    claim_occupied(address, &public_client_id, &owner_cookie).await;
    responder.abort();

    // The non-holder sees only the privacy projection: exactly three fields,
    // no holder identity, no lease identity, no capacity.
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
    assert!(fields.contains(&"schemaVersion"));
    assert!(fields.contains(&"clientId"));
    assert_eq!(view["occupancy"], json!("occupied-by-other"));
    let serialized = response_body(&response).to_string();
    assert!(!serialized.to_lowercase().contains("holder"));
    assert!(!serialized.contains("occupancyLeaseId"));
    assert!(!serialized.contains(&owner_id));

    // The holder sees the full projection of the same client.
    let response = http_request(
        address,
        &cookie_get(
            &format!("/api/v1/clients/{public_client_id}/occupancy"),
            &owner_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "200", "{response}");
    let full = response_body(&response);
    assert_eq!(full["holderUserId"], json!(owner_id));
    assert!(full["occupancyLeaseId"].is_string());
    assert_eq!(full["capacityTotal"], json!(4));

    // After the release the projection reads available again.
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
    let response = http_request(
        address,
        &cookie_get(
            &format!("/api/v1/clients/{public_client_id}/occupancy"),
            &member_cookie,
        ),
    )
    .await;
    assert_eq!(response_body(&response)["occupancy"], json!("available"));

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn release_modes_map_onto_the_service_semantics_and_downlink_frames() {
    let data_directory = test_directory("occupancy-release");
    let auth_directory = test_directory("occupancy-release-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (cookie, user_id) = initialize_and_login_owner(address).await;

    let (node, public_client_id, credential) = enroll_online_device(address, &data_directory).await;
    consume_code_as(
        &data_directory,
        &node,
        &publish_connect_code(&data_directory, &node, "12121212"),
        &user_id,
    );

    // Drain: active worker sessions move the lease to `draining` and the
    // release command carries the drain mode downlink.
    let responder = spawn_offer_responder(
        data_directory.clone(),
        address,
        node.clone(),
        credential.clone(),
        downlink_ack_cursor(&data_directory, &node),
        next_client_sequence(&data_directory, &node),
    );
    claim_occupied(address, &public_client_id, &cookie).await;
    responder.abort();
    let ack_before_release = downlink_ack_cursor(&data_directory, &node);
    set_node_column(
        &data_directory,
        &node,
        "reported_running_worker_sessions = 2",
    );
    let response = http_request(
        address,
        &cookie_delete(
            "/api/v1/clients/occupancy",
            &release_body(&public_client_id, "drain"),
            &cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "200", "{response}");
    assert_eq!(response_body(&response)["occupancy"], json!("draining"));
    let lease = active_lease(&data_directory, &node).expect("draining lease");
    assert_eq!(lease.state, OccupancyLeaseState::Draining);
    {
        let mut storage = SqliteStorage::open(&data_directory).expect("storage");
        let outbox = storage.client_downlink_outbox().expect("outbox");
        let frames = outbox
            .deliverable(&node, ack_before_release, 8)
            .expect("retained release frame");
        let release_frame = frames
            .iter()
            .find(|stored| {
                serde_json::from_str::<Value>(&stored.frame)
                    .is_ok_and(|value| value["kind"] == json!("client.occupancy.release"))
            })
            .expect("release frame retained");
        let value: Value = serde_json::from_str(&release_frame.frame).expect("frame json");
        assert_eq!(value["payload"]["mode"], json!("drain_then_release"));
        assert_eq!(
            value["payload"]["occupancyLeaseId"],
            json!(lease.occupancy_lease_id)
        );
    }

    // The automatic drain judgement: a heartbeat reporting zero running
    // sessions completes the drain and releases the lease.
    send_heartbeat(address, &data_directory, &node, &credential, 0).await;
    let drained = wait_until(|| active_lease(&data_directory, &node).is_none()).await;
    assert!(drained, "the draining lease released after the heartbeat");

    // Immediate release: no active worker session releases at once.
    let responder = spawn_offer_responder(
        data_directory.clone(),
        address,
        node.clone(),
        credential.clone(),
        downlink_ack_cursor(&data_directory, &node),
        next_client_sequence(&data_directory, &node),
    );
    claim_occupied(address, &public_client_id, &cookie).await;
    responder.abort();
    set_node_column(
        &data_directory,
        &node,
        "reported_running_worker_sessions = 0",
    );
    let response = http_request(
        address,
        &cookie_delete(
            "/api/v1/clients/occupancy",
            &release_body(&public_client_id, "release"),
            &cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "200", "{response}");
    assert_eq!(response_body(&response)["occupancy"], json!("released"));
    assert!(active_lease(&data_directory, &node).is_none());

    // Cancel-and-release requires the explicit confirmation flag, then moves
    // a busy lease to draining with the cancel mode downlink.
    let responder = spawn_offer_responder(
        data_directory.clone(),
        address,
        node.clone(),
        credential.clone(),
        downlink_ack_cursor(&data_directory, &node),
        next_client_sequence(&data_directory, &node),
    );
    claim_occupied(address, &public_client_id, &cookie).await;
    responder.abort();
    let ack_before_cancel = downlink_ack_cursor(&data_directory, &node);
    set_node_column(
        &data_directory,
        &node,
        "reported_running_worker_sessions = 1",
    );
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
            &cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "400", "{response}");
    assert_eq!(wire_code(&response), "CONFIRMATION_REQUIRED");
    let response = http_request(
        address,
        &cookie_delete(
            "/api/v1/clients/occupancy",
            &json!({
                "schemaVersion": SCHEMA_VERSION,
                "clientId": public_client_id,
                "mode": "cancel_and_release",
                "confirm": true,
            })
            .to_string(),
            &cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "200", "{response}");
    assert_eq!(response_body(&response)["occupancy"], json!("draining"));
    {
        let mut storage = SqliteStorage::open(&data_directory).expect("storage");
        let outbox = storage.client_downlink_outbox().expect("outbox");
        let frames = outbox
            .deliverable(&node, ack_before_cancel, 8)
            .expect("retained release frame");
        let release_frame = frames
            .iter()
            .find(|stored| {
                serde_json::from_str::<Value>(&stored.frame)
                    .is_ok_and(|value| value["kind"] == json!("client.occupancy.release"))
            })
            .expect("cancel release frame retained");
        let value: Value = serde_json::from_str(&release_frame.frame).expect("frame json");
        assert_eq!(value["payload"]["mode"], json!("cancel_tasks_and_release"));
    }

    // A release while the offer is still unanswered withdraws the claim.
    set_node_column(
        &data_directory,
        &node,
        "reported_running_worker_sessions = 0",
    );
    // The cancelled lease is still draining; the device reporting zero
    // running sessions completes it before the fresh claim.
    send_heartbeat(address, &data_directory, &node, &credential, 0).await;
    let drained = wait_until(|| active_lease(&data_directory, &node).is_none()).await;
    assert!(
        drained,
        "the cancelled lease drained before the fresh claim"
    );
    let application = ClientOccupancyApplication::open(
        &data_directory,
        &ClientOccupancyConfig {
            offer_wait: Duration::from_secs(30),
            poll_interval: Duration::from_millis(20),
            ..ClientOccupancyConfig::default()
        },
    )
    .expect("valid application");
    let claim_request = json!({
        "schemaVersion": SCHEMA_VERSION,
        "clientId": public_client_id,
    });
    let claim_user = user_id.clone();
    let claim_task =
        tokio::spawn(async move { application.claim(&claim_user, &claim_request).await });
    let withdrawn = wait_until(|| {
        active_lease(&data_directory, &node)
            .is_some_and(|lease| lease.state == OccupancyLeaseState::Reserving)
    })
    .await;
    assert!(withdrawn, "the fresh claim sits in reserving");
    let response = http_request(
        address,
        &cookie_delete(
            "/api/v1/clients/occupancy",
            &release_body(&public_client_id, "release"),
            &cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "200", "{response}");
    assert_eq!(response_body(&response)["occupancy"], json!("released"));
    let claim_outcome = claim_task.await.expect("claim task");
    let error = claim_outcome.expect_err("the withdrawn claim must fail");
    assert_eq!(error.kind(), ClientOccupancyErrorKind::OccupancyRejected);
    // The withdrawn lease ended with the `claim_withdrawn` reason: the
    // terminal row is the audit history behind the now-available projection.
    assert!(active_lease(&data_directory, &node).is_none());
    assert_eq!(count_released_leases(&data_directory, "claim_withdrawn"), 1);

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn offline_sweep_marks_recovery_blocks_preemption_and_resumes_with_same_token() {
    let data_directory = test_directory("occupancy-recovery");
    let auth_directory = test_directory("occupancy-recovery-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (owner_cookie, owner_id) = initialize_and_login_owner(address).await;
    let (member_cookie, member_id) =
        create_and_login_member(address, &owner_cookie, "member").await;

    let (node, public_client_id, credential) = enroll_online_device(address, &data_directory).await;
    consume_code_as(
        &data_directory,
        &node,
        &publish_connect_code(&data_directory, &node, "23232323"),
        &owner_id,
    );
    consume_code_as(
        &data_directory,
        &node,
        &publish_connect_code(&data_directory, &node, "34343434"),
        &member_id,
    );
    let responder = spawn_offer_responder(
        data_directory.clone(),
        address,
        node.clone(),
        credential.clone(),
        downlink_ack_cursor(&data_directory, &node),
        next_client_sequence(&data_directory, &node),
    );
    claim_occupied(address, &public_client_id, &owner_cookie).await;
    responder.abort();
    let occupied = active_lease(&data_directory, &node).expect("occupied lease");
    let original_token = occupied.fencing_token;

    // The device goes silent; the offline sweep projects it and its lease.
    // The deadline base sits far in the future so the recovery window is
    // deterministically still open regardless of the wall clock.
    let application =
        ClientOccupancyApplication::open(&data_directory, &ClientOccupancyConfig::default())
            .expect("valid application");
    let sweep_cutoff = Instant("2027-01-01T00:00:00.000Z".to_owned());
    let outcome = application
        .run_offline_sweep(
            &sweep_cutoff,
            &Instant("2100-01-01T00:00:00.000Z".to_owned()),
        )
        .expect("sweep");
    assert!(outcome.swept_nodes.contains(&node), "{outcome:?}");
    assert_eq!(
        outcome.leases_pending_recovery,
        vec![occupied.occupancy_lease_id.clone()]
    );
    let pending = active_lease(&data_directory, &node).expect("recovery lease");
    assert_eq!(pending.state, OccupancyLeaseState::RecoveryPending);
    assert_eq!(pending.fencing_token, original_token);
    assert!(pending.recovery_deadline_at.is_some());
    assert!(
        !application
            .recovery_overdue(&public_client_id)
            .expect("overdue query"),
        "the recovery window is still open"
    );

    // No preemption while the lease is pending recovery: the member's claim
    // is rejected both while the device is offline and after it reconnects
    // (reconciliation has not run yet).
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
    walk_hello(
        address,
        &data_directory,
        &node,
        DEVICE_INSTANCE,
        &credential,
    )
    .await;
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

    // The recovery resume reuses the original fencing token unchanged: no new
    // occupancy happened. (The `client.worker.reconcile` exchange settlement
    // belongs to the worker lane; the resume is applied through the shared
    // service here.)
    {
        let mut storage = SqliteStorage::open(&data_directory).expect("storage");
        let mut occupancy = ClientOccupancyService::new(&mut storage);
        let resumed = occupancy
            .reconcile_resume(
                &occupied.occupancy_lease_id,
                OccupancyReconcileTarget::ResumeOccupied,
                None,
                &Instant("2026-09-04T12:05:00.000Z".to_owned()),
            )
            .expect("reconcile resume");
        assert_eq!(resumed.state, OccupancyLeaseState::Occupied);
        assert_eq!(resumed.fencing_token, original_token);
    }
    let response = http_request(
        address,
        &cookie_get(
            &format!("/api/v1/clients/{public_client_id}/occupancy"),
            &owner_cookie,
        ),
    )
    .await;
    let view = response_body(&response);
    assert_eq!(view["occupancy"], json!("occupied"));
    assert_eq!(view["fencingToken"], json!(original_token));
    assert!(view["recoveryDeadlineAt"].is_null());

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn force_release_is_owner_only_and_enforces_the_recovery_deadline() {
    let data_directory = test_directory("occupancy-force");
    let auth_directory = test_directory("occupancy-force-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (owner_cookie, owner_id) = initialize_and_login_owner(address).await;
    let (member_cookie, member_id) =
        create_and_login_member(address, &owner_cookie, "member").await;

    let (node, public_client_id, credential) = enroll_online_device(address, &data_directory).await;
    consume_code_as(
        &data_directory,
        &node,
        &publish_connect_code(&data_directory, &node, "45454545"),
        &member_id,
    );
    let responder = spawn_offer_responder(
        data_directory.clone(),
        address,
        node.clone(),
        credential,
        downlink_ack_cursor(&data_directory, &node),
        next_client_sequence(&data_directory, &node),
    );
    claim_occupied(address, &public_client_id, &member_cookie).await;
    responder.abort();
    let occupied = active_lease(&data_directory, &node).expect("occupied lease");

    // The Owner sweep marks the dropped device; the window is still open, so
    // the force release is refused and nothing is released automatically.
    let application =
        ClientOccupancyApplication::open(&data_directory, &ClientOccupancyConfig::default())
            .expect("valid application");
    let outcome = application
        .run_offline_sweep(
            &Instant("2027-01-01T00:00:00.000Z".to_owned()),
            &Instant("2100-01-01T00:00:00.000Z".to_owned()),
        )
        .expect("sweep");
    assert_eq!(
        outcome.leases_pending_recovery,
        vec![occupied.occupancy_lease_id.clone()]
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
    assert_eq!(status_of(&response), "409", "{response}");
    assert_eq!(wire_code(&response), "OCCUPANCY_RECOVERY_PENDING");
    let pending = active_lease(&data_directory, &node).expect("still recovery pending");
    assert_eq!(pending.state, OccupancyLeaseState::RecoveryPending);

    // Past the deadline the Owner cleanup releases the lease and mints the
    // strictly higher force-fence token that goes downlink.
    let ack_before_force = downlink_ack_cursor(&data_directory, &node);
    {
        // Backdate the deadline the way an elapsed window would leave it
        // (a recovery replay keeps the original deadline, so the raw column
        // update stages the elapsed window deterministically).
        let connection = rusqlite::Connection::open(data_directory.join("control-plane.sqlite3"))
            .expect("open database");
        connection
            .execute(
                "UPDATE client_occupancy_leases SET recovery_deadline_at = ?1
                 WHERE occupancy_lease_id = ?2",
                rusqlite::params!["2020-01-01T00:00:00.000Z", occupied.occupancy_lease_id,],
            )
            .expect("deadline backdate");
    }
    assert!(
        application
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
    let value = response_body(&response);
    assert_eq!(value["released"], json!(true));
    assert_eq!(
        value["occupancyLeaseId"],
        json!(occupied.occupancy_lease_id)
    );
    let fence_token = value["forceFenceToken"].as_u64().expect("fence token");
    assert!(
        fence_token > occupied.fencing_token,
        "the force fence token is strictly higher"
    );
    assert!(active_lease(&data_directory, &node).is_none());
    {
        let mut storage = SqliteStorage::open(&data_directory).expect("storage");
        let outbox = storage.client_downlink_outbox().expect("outbox");
        let frames = outbox
            .deliverable(&node, ack_before_force, 8)
            .expect("retained fence frame");
        let fence_frame = frames
            .iter()
            .find(|stored| {
                serde_json::from_str::<Value>(&stored.frame)
                    .is_ok_and(|value| value["kind"] == json!("client.occupancy.force_fence"))
            })
            .expect("force fence frame retained");
        let frame_value: Value = serde_json::from_str(&fence_frame.frame).expect("frame json");
        assert_eq!(
            frame_value["payload"]["occupancyFencingToken"],
            json!(fence_token.to_string())
        );
        assert_eq!(
            frame_value["payload"]["supersededLeaseId"],
            json!(occupied.occupancy_lease_id)
        );
        assert_eq!(
            frame_value["payload"]["reason"],
            json!("recovery_deadline_exceeded")
        );
    }

    // The member sees the released client as available again — the occupancy
    // was cleaned up, never handed over.
    let response = http_request(
        address,
        &cookie_get(
            &format!("/api/v1/clients/{public_client_id}/occupancy"),
            &member_cookie,
        ),
    )
    .await;
    assert_eq!(response_body(&response)["occupancy"], json!("available"));
    let _ = owner_id;

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}
