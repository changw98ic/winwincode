// SPDX-License-Identifier: Apache-2.0

//! The user-facing Client candidate flows over real HTTP (GIT-100.7): the
//! dual-authorized `GET /api/v1/clients/{clientId}/candidates` list and the
//! three `POST /api/v1/clients/candidates/{branch,apply,discard}` routes
//! wired to the real client exchange, covering the retained-frame projection
//! driven by a fake device that reports `client.candidate.retained` over the
//! exchange protocol and answers every `client.candidate.apply` command with
//! a `client.candidate.apply_result` receipt, the full branch chain
//! (HTTP → durable downlink → device settlement → `201` + ledger
//! `branch_created`), the apply chain in its `applied` and `base_stale`
//! outcomes with the repeated apply returning the original receipt, the
//! fencing refusal of an apply ticket superseded by a higher token, the
//! terminal discard, the unauthenticated `401`s, and the dual-authorization
//! `403`s.

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
use winwincode_client_port::domain::ApplyResult;
use winwincode_client_port::domain::ApplyStrategy;
use winwincode_client_port::domain::ClientArchitecture;
use winwincode_client_port::domain::ClientCapacityReport;
use winwincode_client_port::domain::ClientLockState;
use winwincode_client_port::domain::ClientPlatformTarget;
use winwincode_client_port::domain::LocalCandidateState;
use winwincode_client_port::domain::PresenceState;
use winwincode_client_port::messages::ClientCandidateApplyResultPayload;
use winwincode_client_port::messages::ClientCandidateRetainedPayload;
use winwincode_client_port::messages::ClientEnrollPayload;
use winwincode_client_port::messages::ClientHelloPayload;
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
    ClientExchangeApplication, ClientExchangeConfig, ClientExchangePort, ControlPlaneApiPort,
    EventSubscription, RequestAuthenticator, ServerConfig, ServerTls, SqliteAuthSessionManager,
    UserAccountService, start_server_with_remote_worker,
};
use winwincode_storage::{
    AccessGrantIssuance, ConnectCodeConsume, ConnectCodePublication, GrantTrustMode,
    OccupancyClaim, OccupancyLeaseState, RepositoryAccessGrantIssuance, RepositoryAvailability,
    RepositoryBindingProjection, RepositoryDirtyState, RepositoryGrantPermissions, SqliteStorage,
};

const BOOTSTRAP_PROOF: &str = "client-candidate-test-bootstrap";
const ORIGIN: &str = "https://client.example";
const OWNER_PASSWORD: &str = "initial-owner-password";
/// Placeholder client node id a fresh device sends before the server assigns
/// the canonical `cnd_` identity.
const FRESH_NODE: &str = "device-local-pending";
const ENROLL_INSTANCE: &str = "cix_A1A1A1A1A1A1A1A1A1A1A1A1A1";
const DEVICE_INSTANCE: &str = "cix_B2B2B2B2B2B2B2B2B2B2B2B2B2";
const SCHEMA_VERSION: &str = "winwincode/v1";
const VALID_UNTIL: &str = "2100-01-01T00:00:00.000Z";
const T0: &str = "2026-09-04T12:00:00.000Z";
/// The commit the fake device reports for a successful apply.
const RESULTING_COMMIT: &str = "1234567890abcdef1234567890abcdef12345678";

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
static NEXT_CODE_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_RECEIPT: AtomicU64 = AtomicU64::new(1);
static NEXT_GRANT: AtomicU64 = AtomicU64::new(1);
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
    format!("rbd_{:026}", NEXT_CODE_ID.fetch_add(1, Ordering::Relaxed))
}

/// One canonical ledger receipt id: `lcr_`/`lar_` + 26 Crockford digits.
fn fresh_receipt_id(prefix: &str) -> String {
    format!(
        "{prefix}{:026}",
        NEXT_RECEIPT.fetch_add(1, Ordering::Relaxed)
    )
}

fn instant(value: &str) -> Instant {
    Instant(value.to_owned())
}

#[derive(Default)]
struct NoopApi;

impl ControlPlaneApiPort for NoopApi {
    fn command(&self, _: &AuthenticatedPrincipal, _: Value) -> Result<Value, ApiError> {
        Err(ApiError::new(
            501,
            "NOT_IMPLEMENTED",
            "unused in client candidate tests",
        ))
    }

    fn query(&self, _: &AuthenticatedPrincipal, _: Value) -> Result<Value, ApiError> {
        Err(ApiError::new(
            501,
            "NOT_IMPLEMENTED",
            "unused in client candidate tests",
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
        occurred_at: T0.to_owned(),
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

/// Enrolls and walks the takeover hello, then sends one heartbeat: the node
/// is `online` under the device instance.
async fn enroll_online_device(
    address: std::net::SocketAddr,
    data_directory: &Path,
) -> (String, String, String) {
    let (node, public_client_id, credential) = enroll_device(address).await;
    walk_hello(address, data_directory, &node, DEVICE_INSTANCE, &credential).await;
    (node, public_client_id, credential)
}

/// Publishes one connect code digest the way the Device Client would, and
/// returns the 8-digit code.
fn publish_connect_code(data_directory: &Path, node: &str, code: &str) -> String {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut connect = ConnectCodeService::new(&mut storage);
    let digest = format!("sha256:{:x}", Sha256::digest(code.as_bytes()));
    let publication = ConnectCodePublication::try_new(
        fresh_code_id(),
        digest,
        node,
        DEVICE_INSTANCE,
        1,
        instant(VALID_UNTIL),
        5,
    )
    .expect("valid publication");
    connect
        .publish(&publication, &instant(T0))
        .expect("publish code");
    code.to_owned()
}

/// Consumes one published code directly, staging the active `use` grant a
/// connected user would hold.
fn consume_code_as(data_directory: &Path, node: &str, code: &str, user_id: &str) {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut connect = ConnectCodeService::new(&mut storage);
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
        .consume_and_grant(&consume, &issuance, &instant("2026-09-04T12:00:01.000Z"))
        .expect("atomic consume and grant");
}

/// Stages one repository binding on the node plus the repository access
/// grant that makes it visible to `user_id` (plan 13.4 dual authorization).
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
        .upsert(&projection, None, 0, &instant(T0))
        .expect("upsert");
    let issuance = RepositoryAccessGrantIssuance::try_new(
        format!("rag_{:026}", NEXT_GRANT.fetch_add(1, Ordering::Relaxed)),
        &binding_id,
        user_id,
        user_id,
    )
    .expect("repo grant issuance");
    ledger
        .create_grant(&issuance, RepositoryGrantPermissions::Use, &instant(T0))
        .expect("repo grant");
    binding_id
}

/// Stages one repository binding without any repository access grant: the
/// binding exists (retained frames can name it) but is invisible.
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
        .upsert(&projection, None, 0, &instant(T0))
        .expect("upsert");
    binding_id
}

/// Claims occupancy directly and walks the lease to `occupied`; returns the
/// (leaseId, fencingToken) stamp the device frames must carry.
fn stage_occupied_lease(data_directory: &Path, node: &str, holder_user_id: &str) -> (String, u64) {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut occupancy = ClientOccupancyService::new(&mut storage);
    let claim = OccupancyClaim::try_new(
        fresh_code_id().replacen("cct_", "ocl_", 1),
        node,
        holder_user_id,
        fresh_code_id().replacen("cct_", "req_", 1),
    )
    .expect("claim");
    let lease = occupancy.atomic_claim(&claim, &instant(T0)).expect("claim");
    let occupied = occupancy
        .record_acknowledgement(
            &lease.occupancy_lease_id,
            lease.fencing_token,
            None,
            &instant("2026-09-04T12:01:00.000Z"),
        )
        .expect("ack");
    assert_eq!(occupied.state, OccupancyLeaseState::Occupied);
    (occupied.occupancy_lease_id, occupied.fencing_token)
}

/// Releases one active lease directly (zero running sessions), the way the
/// drain completion would.
fn release_lease(data_directory: &Path, lease_id: &str, token: u64) {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut occupancy = ClientOccupancyService::new(&mut storage);
    let released = occupancy
        .request_release(lease_id, token, 0, &instant("2026-09-04T12:02:00.000Z"))
        .expect("release");
    assert_eq!(released.state, OccupancyLeaseState::Released);
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

// ---- fake device frames ----------------------------------------------------

/// Sends one `client.candidate.retained` frame the way the Device Client's
/// registry reports a frozen candidate: stamped `C + L` from the occupancy
/// mirror.
#[allow(clippy::too_many_arguments)]
async fn send_retained(
    address: std::net::SocketAddr,
    data_directory: &Path,
    node: &str,
    credential: &str,
    lease_id: &str,
    token: u64,
    mirror_revision: u64,
    binding_id: &str,
    commit: &str,
) {
    let retained = ClientToServerMessage::CandidateRetained(ClientCandidateRetainedPayload {
        occupancy: occupancy_stamp(lease_id, token, mirror_revision),
        worker_session_id: "ws_000000000000000000000001".to_owned(),
        receipt: winwincode_client_port::domain::LocalCandidateReceipt {
            local_candidate_receipt_id: fresh_receipt_id("lcr_"),
            candidate_ref: format!("refs/winwincode/candidates/{commit}"),
            repository_binding_id: binding_id.to_owned(),
            candidate_commit: commit.to_owned(),
            local_ref_name: format!("refs/winwincode/candidates/{commit}"),
            state: LocalCandidateState::Retained,
            created_at: T0.to_owned(),
            revision: 1,
        },
    });
    let sequence = next_client_sequence(data_directory, node);
    let (status, _) = post_exchange(
        address,
        &exchange_request(
            &[frame(node, DEVICE_INSTANCE, sequence, retained)],
            downlink_ack_cursor(data_directory, node),
        ),
        Some(credential),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
}

/// Sends one raw `client.candidate.apply_result` frame with an explicit
/// occupancy stamp — the superseded-ticket probe.
#[allow(clippy::too_many_arguments)]
async fn send_apply_result(
    address: std::net::SocketAddr,
    data_directory: &Path,
    node: &str,
    credential: &str,
    lease_id: &str,
    token: u64,
    receipt_id: &str,
    binding_id: &str,
    commit: &str,
    result: ApplyResult,
    resulting_commit: Option<String>,
) {
    let apply_result =
        ClientToServerMessage::CandidateApplyResult(ClientCandidateApplyResultPayload {
            occupancy: occupancy_stamp(lease_id, token, 1),
            receipt: winwincode_client_port::domain::LocalApplyReceipt {
                local_apply_receipt_id: receipt_id.to_owned(),
                candidate_ref: format!("refs/winwincode/candidates/{commit}"),
                repository_binding_id: binding_id.to_owned(),
                target_branch: "main".to_owned(),
                expected_head: commit.to_owned(),
                strategy: ApplyStrategy::CherryPick,
                result,
                resulting_commit,
                conflict_artifact_ref: None,
                created_at: T0.to_owned(),
                revision: 1,
            },
        });
    let sequence = next_client_sequence(data_directory, node);
    let (status, _) = post_exchange(
        address,
        &exchange_request(
            &[frame(node, DEVICE_INSTANCE, sequence, apply_result)],
            downlink_ack_cursor(data_directory, node),
        ),
        Some(credential),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
}

fn occupancy_stamp(lease_id: &str, token: u64, mirror_revision: u64) -> OccupancyCommandContext {
    OccupancyCommandContext {
        command: CommandContext {
            expected_revision: mirror_revision,
            idempotency_key: format!("idem_device_{token}"),
        },
        occupancy_lease_id: lease_id.to_owned(),
        occupancy_fencing_token: token,
    }
}

/// How the fake device answers a `client.candidate.apply` command.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ApplyVerdict {
    /// The branch engine created the branch.
    BranchCreated,
    /// The apply engine reached the target branch.
    Applied,
    /// The apply engine failed closed: the target base moved.
    BaseStale,
}

/// The fake device: polls the durable downlink outbox like the real daemon
/// polls its inbox, acknowledges every frame, and answers every
/// `client.candidate.apply` command with a `client.candidate.apply_result`
/// receipt echoing the exact occupancy stamp and command facts under the
/// requested verdict.
fn spawn_candidate_responder(
    data_directory: PathBuf,
    address: std::net::SocketAddr,
    node: String,
    credential: String,
    mut inbox_ack: u64,
    mut next_sequence: u64,
    verdict: ApplyVerdict,
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
                if frame_value["kind"] != json!("client.candidate.apply") {
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
                let mirror: u64 = payload["expectedRevision"]
                    .as_u64()
                    .expect("mirror revision");
                let candidate_ref = payload["candidateRef"]
                    .as_str()
                    .expect("candidate ref")
                    .to_owned();
                let commit = candidate_ref
                    .strip_prefix("refs/winwincode/candidates/")
                    .expect("canonical candidate ref")
                    .to_owned();
                let strategy: ApplyStrategy =
                    serde_json::from_value(payload["strategy"].clone()).expect("strategy");
                let (result, resulting_commit) = match verdict {
                    ApplyVerdict::BranchCreated => {
                        (ApplyResult::BranchCreated, Some(commit.clone()))
                    }
                    ApplyVerdict::Applied => {
                        (ApplyResult::Applied, Some(RESULTING_COMMIT.to_owned()))
                    }
                    ApplyVerdict::BaseStale => (ApplyResult::BaseStale, None),
                };
                let receipt_id = fresh_receipt_id("lar_");
                let apply_result = ClientToServerMessage::CandidateApplyResult(
                    ClientCandidateApplyResultPayload {
                        occupancy: occupancy_stamp(&lease_id, token, mirror),
                        receipt: winwincode_client_port::domain::LocalApplyReceipt {
                            local_apply_receipt_id: receipt_id,
                            candidate_ref,
                            repository_binding_id: payload["repositoryBindingId"]
                                .as_str()
                                .expect("binding")
                                .to_owned(),
                            target_branch: payload["targetBranch"]
                                .as_str()
                                .expect("target branch")
                                .to_owned(),
                            expected_head: payload["expectedHead"]
                                .as_str()
                                .expect("expected head")
                                .to_owned(),
                            strategy,
                            result,
                            resulting_commit,
                            conflict_artifact_ref: None,
                            created_at: T0.to_owned(),
                            revision: 1,
                        },
                    },
                );
                let request = exchange_request(
                    &[frame(&node, DEVICE_INSTANCE, next_sequence, apply_result)],
                    stored.sequence,
                );
                next_sequence += 1;
                let (status, _) = post_exchange(address, &request, Some(&credential)).await;
                assert!(status.starts_with("HTTP/1.1 200"), "{status}");
            }
        }
    })
}

// ---- candidate HTTP helpers -------------------------------------------------

fn branch_body(client_id: &str, candidate_ref: &str, binding_id: &str) -> String {
    json!({
        "schemaVersion": SCHEMA_VERSION,
        "clientId": client_id,
        "candidateRef": candidate_ref,
        "repositoryBindingId": binding_id,
    })
    .to_string()
}

fn apply_body(
    client_id: &str,
    candidate_ref: &str,
    binding_id: &str,
    target_branch: &str,
    expected_head: &str,
) -> String {
    json!({
        "schemaVersion": SCHEMA_VERSION,
        "clientId": client_id,
        "candidateRef": candidate_ref,
        "repositoryBindingId": binding_id,
        "targetBranch": target_branch,
        "expectedHead": expected_head,
    })
    .to_string()
}

fn candidate_ref_of(commit: &str) -> String {
    format!("refs/winwincode/candidates/{commit}")
}

/// Reads the durable candidate row of one node directly from the ledger.
fn candidate_state(data_directory: &Path, node: &str, candidate_ref: &str) -> Option<String> {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let ledger = storage.local_candidate_ledger().expect("ledger");
    ledger
        .candidate_for_ref(node, candidate_ref)
        .expect("candidate lookup")
        .map(|record| record.state.as_str().to_owned())
}

/// Counts the immutable apply receipts of one node's candidate.
fn apply_history_len(data_directory: &Path, node: &str, candidate_ref: &str) -> usize {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let ledger = storage.local_candidate_ledger().expect("ledger");
    let candidate = ledger
        .candidate_for_ref(node, candidate_ref)
        .expect("candidate lookup");
    candidate.map_or(0, |candidate| {
        ledger
            .apply_history_for_candidate(&candidate.local_candidate_receipt_id)
            .expect("history")
            .len()
    })
}

// ---- tests ------------------------------------------------------------------

#[tokio::test]
async fn candidate_routes_require_a_signed_in_session() {
    let data_directory = test_directory("candidates-auth");
    let auth_directory = test_directory("candidates-auth-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();

    let response = http_request(
        address,
        &cookie_get("/api/v1/clients/927351842/candidates", "not-a-session"),
    )
    .await;
    assert_eq!(status_of(&response), "401");
    assert_eq!(wire_code(&response), "AUTHENTICATION_REQUIRED");

    let response = http_request(
        address,
        &plain_post(
            "/api/v1/clients/candidates/branch",
            &branch_body("927351842", "refs/winwincode/candidates/ab", "rbd_1"),
        ),
    )
    .await;
    assert_eq!(status_of(&response), "401");

    let response = http_request(
        address,
        &plain_post(
            "/api/v1/clients/candidates/apply",
            &apply_body(
                "927351842",
                "refs/winwincode/candidates/ab",
                "rbd_1",
                "main",
                "0123456789abcdef0123456789abcdef01234567",
            ),
        ),
    )
    .await;
    assert_eq!(status_of(&response), "401");

    let response = http_request(
        address,
        &plain_post(
            "/api/v1/clients/candidates/discard",
            &branch_body("927351842", "refs/winwincode/candidates/ab", "rbd_1"),
        ),
    )
    .await;
    assert_eq!(status_of(&response), "401");

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn retained_frames_project_into_the_dual_authorized_list() {
    let data_directory = test_directory("candidates-list");
    let auth_directory = test_directory("candidates-list-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (owner_cookie, owner_id) = initialize_and_login_owner(address).await;
    let (member_cookie, _member_id) =
        create_and_login_member(address, &owner_cookie, "member").await;

    let (node, public_client_id, credential) = enroll_online_device(address, &data_directory).await;
    consume_code_as(
        &data_directory,
        &node,
        &publish_connect_code(&data_directory, &node, "68421975"),
        &owner_id,
    );
    let visible_binding = stage_visible_binding(&data_directory, &node, &owner_id);
    let invisible_binding = stage_invisible_binding(&data_directory, &node);

    // No candidate is retained yet.
    let response = http_request(
        address,
        &cookie_get(
            &format!("/api/v1/clients/{public_client_id}/candidates"),
            &owner_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "200", "{response}");
    assert_eq!(response_body(&response)["candidates"], json!([]));

    // The device freezes two candidates: one on the shared binding, one on
    // the unshared binding.
    let commit = "0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776";
    let hidden_commit = "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d";
    send_retained(
        address,
        &data_directory,
        &node,
        &credential,
        "ocl_test",
        1,
        1,
        &visible_binding,
        commit,
    )
    .await;
    send_retained(
        address,
        &data_directory,
        &node,
        &credential,
        "ocl_test",
        1,
        1,
        &invisible_binding,
        hidden_commit,
    )
    .await;

    // The holder sees exactly the visible candidate with the ledger facts.
    let response = http_request(
        address,
        &cookie_get(
            &format!("/api/v1/clients/{public_client_id}/candidates"),
            &owner_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "200", "{response}");
    let candidates = response_body(&response)["candidates"]
        .as_array()
        .expect("candidates array")
        .clone();
    assert_eq!(candidates.len(), 1, "{candidates:?}");
    let card = &candidates[0];
    assert_eq!(card["candidateRef"], json!(candidate_ref_of(commit)));
    assert_eq!(card["repositoryBindingId"], json!(visible_binding));
    assert_eq!(card["candidateCommit"], json!(commit));
    assert_eq!(card["state"], json!("retained"));
    assert_eq!(card["revision"], json!(1));
    assert!(card["branchName"].is_null());
    assert_eq!(card["history"], json!([]));
    assert!(
        card["localCandidateReceiptId"]
            .as_str()
            .expect("id")
            .starts_with("lcr_")
    );

    // The idempotent replay settles as the same retention: still one card.
    send_retained(
        address,
        &data_directory,
        &node,
        &credential,
        "ocl_test",
        1,
        1,
        &visible_binding,
        commit,
    )
    .await;
    let response = http_request(
        address,
        &cookie_get(
            &format!("/api/v1/clients/{public_client_id}/candidates"),
            &owner_cookie,
        ),
    )
    .await;
    assert_eq!(
        response_body(&response)["candidates"]
            .as_array()
            .expect("candidates")
            .len(),
        1,
        "the replayed retention dedupes"
    );

    // A user without the dual grants sees no candidates at all.
    let response = http_request(
        address,
        &cookie_get(
            &format!("/api/v1/clients/{public_client_id}/candidates"),
            &member_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "200", "{response}");
    assert_eq!(response_body(&response)["candidates"], json!([]));

    // An unknown Client id is not a candidate source.
    let response = http_request(
        address,
        &cookie_get("/api/v1/clients/111222333/candidates", &owner_cookie),
    )
    .await;
    assert_eq!(status_of(&response), "404");
    assert_eq!(wire_code(&response), "CLIENT_NOT_FOUND");

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn branch_chain_runs_http_downlink_device_ack_and_is_idempotent() {
    let data_directory = test_directory("candidates-branch");
    let auth_directory = test_directory("candidates-branch-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (owner_cookie, owner_id) = initialize_and_login_owner(address).await;

    let (node, public_client_id, credential) = enroll_online_device(address, &data_directory).await;
    consume_code_as(
        &data_directory,
        &node,
        &publish_connect_code(&data_directory, &node, "12121212"),
        &owner_id,
    );
    let binding = stage_visible_binding(&data_directory, &node, &owner_id);
    let (lease_id, token) = stage_occupied_lease(&data_directory, &node, &owner_id);
    let commit = "0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776";
    send_retained(
        address,
        &data_directory,
        &node,
        &credential,
        &lease_id,
        token,
        1,
        &binding,
        commit,
    )
    .await;

    let responder = spawn_candidate_responder(
        data_directory.clone(),
        address,
        node.clone(),
        credential,
        downlink_ack_cursor(&data_directory, &node),
        next_client_sequence(&data_directory, &node),
        ApplyVerdict::BranchCreated,
    );

    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/candidates/branch",
            &branch_body(&public_client_id, &candidate_ref_of(commit), &binding),
            &owner_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "201", "{response}");
    let value = response_body(&response);
    let branch_name = value["branchName"]
        .as_str()
        .expect("branch name")
        .to_owned();
    assert_eq!(
        branch_name,
        format!("winwincode/candidate-{}", &commit[..7]),
        "the branch command requested the deterministic device-namespace name"
    );
    assert_eq!(value["candidate"]["state"], json!("branch_created"));
    assert_eq!(value["candidate"]["branchName"], json!(branch_name));
    let history = value["candidate"]["history"].as_array().expect("history");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["result"], json!("branch_created"));
    assert_eq!(history[0]["strategy"], json!("create_branch"));
    assert_eq!(history[0]["targetBranch"], json!(branch_name));

    // The device answered from the durable outbox: the command was durable
    // before the Server responded, and the ledger carries the settlement.
    assert_eq!(
        candidate_state(&data_directory, &node, &candidate_ref_of(commit)).expect("candidate"),
        "branch_created"
    );
    assert_eq!(
        apply_history_len(&data_directory, &node, &candidate_ref_of(commit)),
        1
    );

    // The repeated request returns the original branch without a device
    // round trip (the responder no longer runs).
    responder.abort();
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/candidates/branch",
            &branch_body(&public_client_id, &candidate_ref_of(commit), &binding),
            &owner_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "201", "{response}");
    assert_eq!(response_body(&response)["branchName"], json!(branch_name));
    assert_eq!(
        apply_history_len(&data_directory, &node, &candidate_ref_of(commit)),
        1,
        "the replay appended no second receipt"
    );

    // An unknown candidate is a resource miss.
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/candidates/branch",
            &branch_body(
                &public_client_id,
                &candidate_ref_of("9999999999999999999999999999999999999999"),
                &binding,
            ),
            &owner_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "404", "{response}");
    assert_eq!(wire_code(&response), "RESOURCE_NOT_FOUND");

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn apply_chain_settles_both_outcomes_and_repeats_the_original_receipt() {
    let data_directory = test_directory("candidates-apply");
    let auth_directory = test_directory("candidates-apply-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (owner_cookie, owner_id) = initialize_and_login_owner(address).await;

    let (node, public_client_id, credential) = enroll_online_device(address, &data_directory).await;
    consume_code_as(
        &data_directory,
        &node,
        &publish_connect_code(&data_directory, &node, "23232323"),
        &owner_id,
    );
    let binding = stage_visible_binding(&data_directory, &node, &owner_id);
    let (lease_id, token) = stage_occupied_lease(&data_directory, &node, &owner_id);
    let applied_commit = "0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776";
    let stale_commit = "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d";
    send_retained(
        address,
        &data_directory,
        &node,
        &credential,
        &lease_id,
        token,
        1,
        &binding,
        applied_commit,
    )
    .await;
    send_retained(
        address,
        &data_directory,
        &node,
        &credential,
        &lease_id,
        token,
        1,
        &binding,
        stale_commit,
    )
    .await;

    // Applied outcome: the receipt settles with its resulting commit.
    let responder = spawn_candidate_responder(
        data_directory.clone(),
        address,
        node.clone(),
        credential.clone(),
        downlink_ack_cursor(&data_directory, &node),
        next_client_sequence(&data_directory, &node),
        ApplyVerdict::Applied,
    );
    let request = apply_body(
        &public_client_id,
        &candidate_ref_of(applied_commit),
        &binding,
        "main",
        "0123456789abcdef0123456789abcdef01234567",
    );
    let response = http_request(
        address,
        &cookie_post("/api/v1/clients/candidates/apply", &request, &owner_cookie),
    )
    .await;
    assert_eq!(status_of(&response), "201", "{response}");
    let value = response_body(&response);
    assert_eq!(value["receipt"]["result"], json!("applied"));
    assert_eq!(value["receipt"]["strategy"], json!("cherry_pick"));
    assert_eq!(value["receipt"]["targetBranch"], json!("main"));
    assert_eq!(value["receipt"]["resultingCommit"], json!(RESULTING_COMMIT));
    let receipt_id = value["receipt"]["localApplyReceiptId"]
        .as_str()
        .expect("receipt id")
        .to_owned();
    assert_eq!(
        candidate_state(&data_directory, &node, &candidate_ref_of(applied_commit))
            .expect("candidate"),
        "applied",
        "the applied result is terminal"
    );

    // The repeated apply returns the original receipt — never a second one.
    responder.abort();
    let response = http_request(
        address,
        &cookie_post("/api/v1/clients/candidates/apply", &request, &owner_cookie),
    )
    .await;
    assert_eq!(status_of(&response), "201", "{response}");
    assert_eq!(
        response_body(&response)["receipt"]["localApplyReceiptId"],
        json!(receipt_id)
    );
    assert_eq!(
        apply_history_len(&data_directory, &node, &candidate_ref_of(applied_commit)),
        1
    );

    // A different apply command on the terminal candidate is refused.
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/candidates/apply",
            &apply_body(
                &public_client_id,
                &candidate_ref_of(applied_commit),
                &binding,
                "other",
                "0123456789abcdef0123456789abcdef01234567",
            ),
            &owner_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "409", "{response}");
    assert_eq!(wire_code(&response), "WRONG_STATE");

    // Base-stale outcome: the receipt settles fail-closed and the candidate
    // stays retryable (`failed`).
    let responder = spawn_candidate_responder(
        data_directory.clone(),
        address,
        node.clone(),
        credential,
        downlink_ack_cursor(&data_directory, &node),
        next_client_sequence(&data_directory, &node),
        ApplyVerdict::BaseStale,
    );
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/candidates/apply",
            &apply_body(
                &public_client_id,
                &candidate_ref_of(stale_commit),
                &binding,
                "main",
                "0123456789abcdef0123456789abcdef01234567",
            ),
            &owner_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "201", "{response}");
    assert_eq!(
        response_body(&response)["receipt"]["result"],
        json!("base_stale")
    );
    assert!(
        response_body(&response)["receipt"]["resultingCommit"].is_null(),
        "a fail-closed result claims no commit"
    );
    responder.abort();
    assert_eq!(
        candidate_state(&data_directory, &node, &candidate_ref_of(stale_commit))
            .expect("candidate"),
        "failed"
    );

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn superseded_apply_tickets_are_refused_and_the_current_token_settles() {
    let data_directory = test_directory("candidates-fencing");
    let auth_directory = test_directory("candidates-fencing-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (_owner_cookie, owner_id) = initialize_and_login_owner(address).await;

    let (node, _public_client_id, credential) =
        enroll_online_device(address, &data_directory).await;
    consume_code_as(
        &data_directory,
        &node,
        &publish_connect_code(&data_directory, &node, "34343434"),
        &owner_id,
    );
    let binding = stage_visible_binding(&data_directory, &node, &owner_id);
    let (first_lease, first_token) = stage_occupied_lease(&data_directory, &node, &owner_id);
    let commit = "0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776";
    send_retained(
        address,
        &data_directory,
        &node,
        &credential,
        &first_lease,
        first_token,
        1,
        &binding,
        commit,
    )
    .await;

    // The occupancy is released and re-claimed: the fresh lease mints a
    // strictly higher fencing token, superseding every older ticket.
    release_lease(&data_directory, &first_lease, first_token);
    let (second_lease, second_token) = stage_occupied_lease(&data_directory, &node, &owner_id);
    assert!(second_token > first_token, "tokens are strictly monotonic");

    // The device reports the result of its superseded ticket (stamped with
    // the released lease): the exchange refuses it — no ledger settlement.
    send_apply_result(
        address,
        &data_directory,
        &node,
        &credential,
        &first_lease,
        first_token,
        &fresh_receipt_id("lar_"),
        &binding,
        commit,
        ApplyResult::Applied,
        Some(RESULTING_COMMIT.to_owned()),
    )
    .await;
    assert_eq!(
        apply_history_len(&data_directory, &node, &candidate_ref_of(commit)),
        0,
        "the superseded ticket never reaches the ledger"
    );
    assert_eq!(
        candidate_state(&data_directory, &node, &candidate_ref_of(commit)).expect("candidate"),
        "retained"
    );

    // A mismatching token under the current lease is refused the same way.
    send_apply_result(
        address,
        &data_directory,
        &node,
        &credential,
        &second_lease,
        second_token - 1,
        &fresh_receipt_id("lar_"),
        &binding,
        commit,
        ApplyResult::Applied,
        Some(RESULTING_COMMIT.to_owned()),
    )
    .await;
    assert_eq!(
        apply_history_len(&data_directory, &node, &candidate_ref_of(commit)),
        0
    );

    // The ticket stamped with the current lease and token settles.
    send_apply_result(
        address,
        &data_directory,
        &node,
        &credential,
        &second_lease,
        second_token,
        &fresh_receipt_id("lar_"),
        &binding,
        commit,
        ApplyResult::Applied,
        Some(RESULTING_COMMIT.to_owned()),
    )
    .await;
    assert_eq!(
        apply_history_len(&data_directory, &node, &candidate_ref_of(commit)),
        1,
        "the current ticket settles exactly once"
    );
    assert_eq!(
        candidate_state(&data_directory, &node, &candidate_ref_of(commit)).expect("candidate"),
        "applied"
    );

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[tokio::test]
async fn discard_settles_terminal_and_idempotent() {
    let data_directory = test_directory("candidates-discard");
    let auth_directory = test_directory("candidates-discard-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (owner_cookie, owner_id) = initialize_and_login_owner(address).await;

    let (node, public_client_id, credential) = enroll_online_device(address, &data_directory).await;
    consume_code_as(
        &data_directory,
        &node,
        &publish_connect_code(&data_directory, &node, "45454545"),
        &owner_id,
    );
    let binding = stage_visible_binding(&data_directory, &node, &owner_id);
    let (lease_id, token) = stage_occupied_lease(&data_directory, &node, &owner_id);
    let commit = "0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776";
    send_retained(
        address,
        &data_directory,
        &node,
        &credential,
        &lease_id,
        token,
        1,
        &binding,
        commit,
    )
    .await;

    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/candidates/discard",
            &branch_body(&public_client_id, &candidate_ref_of(commit), &binding),
            &owner_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "200", "{response}");
    let value = response_body(&response);
    assert_eq!(value["candidate"]["state"], json!("discarded"));
    let history = value["candidate"]["history"].as_array().expect("history");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["result"], json!("discarded"));
    assert!(history[0]["resultingCommit"].is_null());
    assert_eq!(
        candidate_state(&data_directory, &node, &candidate_ref_of(commit)).expect("candidate"),
        "discarded"
    );

    // The repeated discard is an accepted idempotent replay: still one
    // receipt, still terminal.
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/candidates/discard",
            &branch_body(&public_client_id, &candidate_ref_of(commit), &binding),
            &owner_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "200", "{response}");
    assert_eq!(
        response_body(&response)["candidate"]["state"],
        json!("discarded")
    );
    assert_eq!(
        apply_history_len(&data_directory, &node, &candidate_ref_of(commit)),
        1
    );

    // Every further mutation refuses the terminal lifecycle.
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/candidates/branch",
            &branch_body(&public_client_id, &candidate_ref_of(commit), &binding),
            &owner_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "409", "{response}");
    assert_eq!(wire_code(&response), "WRONG_STATE");

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn branch_and_apply_require_the_dual_authorization() {
    let data_directory = test_directory("candidates-authz");
    let auth_directory = test_directory("candidates-authz-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (owner_cookie, owner_id) = initialize_and_login_owner(address).await;
    let (member_cookie, member_id) =
        create_and_login_member(address, &owner_cookie, "member").await;
    assert_ne!(owner_id, member_id);

    let (node, public_client_id, credential) = enroll_online_device(address, &data_directory).await;
    consume_code_as(
        &data_directory,
        &node,
        &publish_connect_code(&data_directory, &node, "56565656"),
        &owner_id,
    );
    // A binding the owner cannot see (no repository grant) and one they can.
    let invisible = stage_invisible_binding(&data_directory, &node);
    let visible = stage_visible_binding(&data_directory, &node, &owner_id);
    let (lease_id, token) = stage_occupied_lease(&data_directory, &node, &owner_id);
    let commit = "0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776";
    let hidden_commit = "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d";
    send_retained(
        address,
        &data_directory,
        &node,
        &credential,
        &lease_id,
        token,
        1,
        &visible,
        commit,
    )
    .await;
    send_retained(
        address,
        &data_directory,
        &node,
        &credential,
        &lease_id,
        token,
        1,
        &invisible,
        hidden_commit,
    )
    .await;

    // A signed-in user without the occupancy fails the holder gate.
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/candidates/branch",
            &branch_body(&public_client_id, &candidate_ref_of(commit), &visible),
            &member_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "403", "{response}");
    assert_eq!(wire_code(&response), "PERMISSION_DENIED");

    // The holder without the repository grant fails the visibility half of
    // the dual authorization.
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/candidates/branch",
            &branch_body(
                &public_client_id,
                &candidate_ref_of(hidden_commit),
                &invisible,
            ),
            &owner_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "403", "{response}");
    assert_eq!(wire_code(&response), "ACCESS_DENIED");

    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/candidates/apply",
            &apply_body(
                &public_client_id,
                &candidate_ref_of(hidden_commit),
                &invisible,
                "main",
                "0123456789abcdef0123456789abcdef01234567",
            ),
            &owner_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "403", "{response}");
    assert_eq!(wire_code(&response), "ACCESS_DENIED");

    // The invisible candidate is invisible in the list projection too.
    let response = http_request(
        address,
        &cookie_get(
            &format!("/api/v1/clients/{public_client_id}/candidates"),
            &owner_cookie,
        ),
    )
    .await;
    let refs = response_body(&response)["candidates"]
        .as_array()
        .expect("candidates")
        .iter()
        .map(|card| card["repositoryBindingId"].clone())
        .collect::<Vec<_>>();
    assert_eq!(refs, vec![json!(visible)]);

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}
