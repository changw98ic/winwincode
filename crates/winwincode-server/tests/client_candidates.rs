// SPDX-License-Identifier: Apache-2.0

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
use winwincode_client_port::candidate_application::candidate_application_digest;
use winwincode_client_port::domain::ApplyResult;
use winwincode_client_port::domain::ApplyStrategy;
use winwincode_client_port::domain::ClientArchitecture;
use winwincode_client_port::domain::ClientCapacityReport;
use winwincode_client_port::domain::ClientControlError;
use winwincode_client_port::domain::ClientControlErrorCode;
use winwincode_client_port::domain::ClientControlMessageKind;
use winwincode_client_port::domain::ClientLockState;
use winwincode_client_port::domain::ClientPlatformTarget;
use winwincode_client_port::domain::CommandAckStatus;
use winwincode_client_port::domain::LocalCandidateState;
use winwincode_client_port::domain::PresenceState;
use winwincode_client_port::messages::ClientCandidateApplyResultPayload;
use winwincode_client_port::messages::ClientCandidateRetainedPayload;
use winwincode_client_port::messages::ClientCommandAckPayload;
use winwincode_client_port::messages::ClientEnrollPayload;
use winwincode_client_port::messages::ClientHelloPayload;
use winwincode_client_port::messages::ClientToServerEnvelope;
use winwincode_client_port::messages::ClientToServerMessage;
use winwincode_client_port::messages::CommandContext;
use winwincode_client_port::messages::OccupancyCommandContext;
use winwincode_client_port::messages::{
    ApplicationReconciliationOutcome, ClientApplicationReconciledPayload,
};
use winwincode_control_plane::ClientOccupancyService;
use winwincode_control_plane::ClientRegistryService;
use winwincode_control_plane::ConnectCodeService;
use winwincode_domain::Instant;
use winwincode_server::{
    ApiError, AuthSessionBootstrap, AuthSessionConfig, AuthenticatedPrincipal,
    ClientCandidatesApplication, ClientCandidatesConfig, ClientCandidatesErrorKind,
    ClientExchangeApplication, ClientExchangeConfig, ClientExchangePort, ControlPlaneApiPort,
    EventSubscription, RequestAuthenticator, ServerConfig, SqliteAuthSessionManager,
    UserAccountService, start_server_with_remote_worker,
};
use winwincode_storage::{
    AccessGrantIssuance, CandidateApplicationRecord, ConnectCodeConsume, ConnectCodePublication,
    GrantTrustMode, OccupancyClaim, OccupancyLeaseState, RepositoryAccessGrantIssuance,
    RepositoryAvailability, RepositoryBindingProjection, RepositoryDirtyState,
    RepositoryGrantPermissions, SqliteStorage,
};

const BOOTSTRAP_PROOF: &str = "client-candidate-test-bootstrap";
const ORIGIN: &str = "https://client.example";
const OWNER_PASSWORD: &str = "initial-owner-password";
const FRESH_NODE: &str = "device-local-pending";
const ENROLL_INSTANCE: &str = "cix_A1A1A1A1A1A1A1A1A1A1A1A1A1";
const DEVICE_INSTANCE: &str = "cix_B2B2B2B2B2B2B2B2B2B2B2B2B2";
const SCHEMA_VERSION: &str = "winwincode/v1";
const VALID_UNTIL: &str = "2100-01-01T00:00:00.000Z";
const T0: &str = "2026-09-04T12:00:00.000Z";
const RESULTING_COMMIT: &str = "1234567890abcdef1234567890abcdef12345678";
const FIXTURE_BASE_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

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

async fn post_exchange(address: std::net::SocketAddr, body: &str) -> (String, Option<Value>) {
    let mut stream = TcpStream::connect(address).await.expect("connect server");
    let request = format!(
        "POST /internal/v1/client/exchange HTTP/1.1\r\nHost: control.example\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
        "clientNodeId": frames[0]["clientNodeId"],
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

async fn enroll_device(address: std::net::SocketAddr) -> (String, String) {
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
    (node, public_client_id)
}

async fn walk_hello(
    address: std::net::SocketAddr,
    data_directory: &Path,
    node: &str,
    instance: &str,
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
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert_eq!(
        body.expect("hello response body")["ackSequence"],
        json!(sequence)
    );
}

async fn enroll_online_device(
    address: std::net::SocketAddr,
    data_directory: &Path,
) -> (String, String) {
    let (node, public_client_id) = enroll_device(address).await;
    walk_hello(address, data_directory, &node, DEVICE_INSTANCE).await;
    (node, public_client_id)
}

fn publish_connect_code(data_directory: &Path, node: &str, code: &str) -> String {
    publish_connect_code_for_instance(data_directory, node, DEVICE_INSTANCE, code)
}

fn publish_connect_code_for_instance(
    data_directory: &Path,
    node: &str,
    instance: &str,
    code: &str,
) -> String {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut connect = ConnectCodeService::new(&mut storage);
    let digest = format!("sha256:{:x}", Sha256::digest(code.as_bytes()));
    let publication = ConnectCodePublication::try_new(
        fresh_code_id(),
        digest,
        node,
        instance,
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

#[allow(clippy::too_many_arguments)]
async fn send_retained(
    address: std::net::SocketAddr,
    data_directory: &Path,
    node: &str,
    lease_id: &str,
    token: u64,
    mirror_revision: u64,
    binding_id: &str,
    commit: &str,
) -> ClientToServerMessage {
    let retained = ClientToServerMessage::CandidateRetained(ClientCandidateRetainedPayload {
        occupancy: occupancy_stamp(
            lease_id,
            token,
            mirror_revision,
            &format!("candidate-retained-{commit}"),
        ),
        worker_session_id: "wsn_000000000000000000000001".to_owned(),
        receipt: winwincode_client_port::domain::LocalCandidateReceipt {
            local_candidate_receipt_id: fresh_receipt_id("lcr_"),
            candidate_ref: format!("refs/winwincode/candidates/{commit}"),
            repository_binding_id: binding_id.to_owned(),
            candidate_commit: commit.to_owned(),
            base_commit: FIXTURE_BASE_COMMIT.into(),
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
            &[frame(node, DEVICE_INSTANCE, sequence, retained.clone())],
            downlink_ack_cursor(data_directory, node),
        ),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    retained
}

#[allow(clippy::too_many_arguments)]
async fn send_apply_result(
    address: std::net::SocketAddr,
    data_directory: &Path,
    node: &str,
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
            occupancy: occupancy_stamp(
                lease_id,
                token,
                1,
                &format!("candidate-apply-result-{receipt_id}-{lease_id}-{token}"),
            ),
            receipt: winwincode_client_port::domain::LocalApplyReceipt {
                local_apply_receipt_id: receipt_id.to_owned(),
                candidate_ref: format!("refs/winwincode/candidates/{commit}"),
                repository_binding_id: binding_id.to_owned(),
                target_branch: "main".to_owned(),
                expected_head: FIXTURE_BASE_COMMIT.into(),
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
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
}

#[allow(clippy::too_many_arguments)]
fn stage_application_approval(
    directory: &Path,
    node: &str,
    owner: &str,
    lease: &str,
    epoch: u64,
    binding: &str,
    commit: &str,
    strategy: &str,
) -> CandidateApplicationRecord {
    let mut storage = SqliteStorage::open(directory).unwrap();
    let candidate = winwincode_control_plane::LocalCandidateService::new(&mut storage)
        .candidate_for_ref(node, &candidate_ref_of(commit))
        .unwrap()
        .unwrap();
    storage
        .grant_candidate_application(&CandidateApplicationRecord {
            application_id: fresh_receipt_id("lar_"),
            approval_id: fresh_receipt_id("apr_"),
            client_node_id: node.into(),
            local_candidate_receipt_id: candidate.local_candidate_receipt_id,
            repository_binding_id: binding.into(),
            candidate_ref: candidate_ref_of(commit),
            candidate_digest: candidate_application_digest(binding, &candidate.base_commit, commit),
            base_commit: candidate.base_commit,
            target_branch: "main".into(),
            strategy: strategy.into(),
            original_lease_id: lease.into(),
            original_epoch: epoch,
            requester_user_id: owner.into(),
            created_at: T0.into(),
            result_json: None,
            outcome: None,
            retry_lease_id: None,
            retry_epoch: None,
        })
        .unwrap()
}

async fn send_application_reconciliation(
    address: std::net::SocketAddr,
    directory: &Path,
    application: &CandidateApplicationRecord,
    current_lease: &str,
    current_epoch: u64,
    outcome: ApplicationReconciliationOutcome,
) {
    let message =
        ClientToServerMessage::ApplicationReconciled(ClientApplicationReconciledPayload {
            occupancy: occupancy_stamp(
                current_lease,
                current_epoch,
                1,
                &format!("reconcile-{}-{current_epoch}", application.application_id),
            ),
            application_id: application.application_id.clone(),
            approval_id: application.approval_id.clone(),
            original_epoch: application.original_epoch,
            repository_binding_id: application.repository_binding_id.clone(),
            candidate_ref: application.candidate_ref.clone(),
            candidate_digest: application.candidate_digest.clone(),
            base_commit: application.base_commit.clone(),
            target_branch: application.target_branch.clone(),
            outcome,
            result_commit: (outcome == ApplicationReconciliationOutcome::Applied)
                .then(|| RESULTING_COMMIT.into()),
        });
    let sequence = next_client_sequence(directory, &application.client_node_id);
    let request = exchange_request(
        &[frame(
            &application.client_node_id,
            DEVICE_INSTANCE,
            sequence,
            message,
        )],
        downlink_ack_cursor(directory, &application.client_node_id),
    );
    let (status, _) = post_exchange(address, &request).await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
}

fn occupancy_stamp(
    lease_id: &str,
    token: u64,
    mirror_revision: u64,
    idempotency_key: &str,
) -> OccupancyCommandContext {
    OccupancyCommandContext {
        command: CommandContext {
            expected_revision: mirror_revision,
            idempotency_key: idempotency_key.to_owned(),
        },
        occupancy_lease_id: lease_id.to_owned(),
        occupancy_fencing_token: token,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ApplyVerdict {
    BranchCreated,
    Applied,
    BaseStale,
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn spawn_candidate_responder(
    data_directory: PathBuf,
    address: std::net::SocketAddr,
    node: String,
    instance: String,
    expected_application: Option<(String, String)>,
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
                assert_eq!(frame_value["clientInstanceId"], instance);
                let payload = &frame_value["payload"];
                if let Some((application_id, approval_id)) = &expected_application {
                    assert_eq!(payload["applicationId"], application_id.as_str());
                    assert_eq!(payload["approvalId"], approval_id.as_str());
                }
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
                let receipt_id = if strategy == ApplyStrategy::CreateBranch {
                    fresh_receipt_id("lar_")
                } else {
                    payload["applicationId"]
                        .as_str()
                        .expect("approved application id")
                        .to_owned()
                };
                let apply_result = ClientToServerMessage::CandidateApplyResult(
                    ClientCandidateApplyResultPayload {
                        occupancy: occupancy_stamp(
                            &lease_id,
                            token,
                            mirror,
                            &format!("candidate-apply-result-{receipt_id}"),
                        ),
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
                    &[frame(&node, &instance, next_sequence, apply_result)],
                    stored.sequence,
                );
                next_sequence += 1;
                let (status, _) = post_exchange(address, &request).await;
                assert!(status.starts_with("HTTP/1.1 200"), "{status}");
            }
        }
    })
}

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

fn candidate_state(data_directory: &Path, node: &str, candidate_ref: &str) -> Option<String> {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let ledger = storage.local_candidate_ledger().expect("ledger");
    ledger
        .candidate_for_ref(node, candidate_ref)
        .expect("candidate lookup")
        .map(|record| record.state.as_str().to_owned())
}

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

fn drive_candidate_daemon_until(
    daemon: &mut winwincode_device_client::DeviceDaemon,
    mut ready: impl FnMut(&mut winwincode_device_client::DeviceDaemon) -> bool,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut now = std::time::Instant::now();
    while std::time::Instant::now() < deadline {
        if ready(daemon) {
            return;
        }
        match daemon.tick(now).unwrap() {
            winwincode_device_client::TickOutcome::Waiting { ready_in } => now += ready_in,
            winwincode_device_client::TickOutcome::Retrying { after, .. } => now += after,
            winwincode_device_client::TickOutcome::Exchanged { .. } => {}
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    panic!("Device recovery did not settle: {:?}", daemon.status());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn restarted_device_announces_before_applied_recovery_and_repeats_it_in_a_new_epoch() {
    use winwincode_device_client::apply_engine::{
        ApplicationCrashPoint, CandidateApplyRequest, apply_candidate_with_crash_point,
        report_recovered_applications,
    };
    use winwincode_device_client::{
        DaemonConfig, DeviceDaemon, DeviceIdentitySeed, DeviceStore, HttpExchangeTransport,
        OccupancyMirrorUpdate, ensure_device_identity,
    };
    fn git(repository: &Path, arguments: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repository)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
    let repository = test_directory("candidates-device-recovery-repository");
    let integration_root = test_directory("candidates-device-recovery-integration");
    std::fs::create_dir_all(&repository).unwrap();
    let repository = std::fs::canonicalize(repository).unwrap();
    git(&repository, &["init", "-q", "--initial-branch=main"]);
    std::fs::write(repository.join("base.txt"), "base\n").unwrap();
    git(&repository, &["add", "-A"]);
    git(
        &repository,
        &[
            "-c",
            "user.name=Recovery Test",
            "-c",
            "user.email=recovery@example.test",
            "commit",
            "-q",
            "-m",
            "base",
        ],
    );
    let base = git(&repository, &["rev-parse", "HEAD"]);
    std::fs::write(repository.join("candidate.txt"), "candidate\n").unwrap();
    git(&repository, &["add", "-A"]);
    git(
        &repository,
        &[
            "-c",
            "user.name=Recovery Test",
            "-c",
            "user.email=recovery@example.test",
            "commit",
            "-q",
            "-m",
            "candidate",
        ],
    );
    let commit = git(&repository, &["rev-parse", "HEAD"]);
    git(
        &repository,
        &["update-ref", &candidate_ref_of(&commit), &commit],
    );
    git(&repository, &["reset", "-q", "--hard", &base]);
    let data_directory = test_directory("candidates-device-recovery");
    let auth_directory = test_directory("candidates-device-recovery-auth");
    let device_directory = test_directory("candidates-device-recovery-device");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (cookie, owner) = initialize_and_login_owner(address).await;
    let seed = DeviceIdentitySeed {
        display_name: "Recovery Device".into(),
        platform: "darwin".into(),
        architecture: "arm64".into(),
        client_version: "0.1.0-alpha.1".into(),
    };
    let mut store = DeviceStore::open(&device_directory).unwrap();
    let identity = ensure_device_identity(&mut store, &seed, T0).unwrap();
    let config = DaemonConfig {
        base_url: format!("http://{address}/internal/v1/client/exchange"),
        heartbeat_interval: Duration::from_mins(2),
        enroll_poll_interval: Duration::from_millis(1),
        capacity: capacity(0),
        ..DaemonConfig::default()
    };
    let transport = Arc::new(HttpExchangeTransport::new(config.base_url.clone()));
    let mut daemon =
        DeviceDaemon::start(config.clone(), store, transport.clone(), &identity).unwrap();
    drive_candidate_daemon_until(&mut daemon, |daemon| {
        daemon.is_enrolled() && daemon.status().acked_through >= 4
    });
    let node = daemon.client_node_id().to_owned();
    let public = winwincode_device_client::load_device_identity(daemon.store_mut())
        .unwrap()
        .unwrap()
        .identity()
        .public_client_id()
        .to_owned();
    consume_code_as(
        &data_directory,
        &node,
        &publish_connect_code_for_instance(
            &data_directory,
            &node,
            identity.current_instance_id(),
            "82828282",
        ),
        &owner,
    );
    let binding = stage_visible_binding(&data_directory, &node, &owner);
    let (lease, epoch) = stage_occupied_lease(&data_directory, &node, &owner);
    daemon
        .store_mut()
        .advance_occupancy_mirror(&OccupancyMirrorUpdate {
            occupancy_lease_id: lease.clone(),
            fencing_token: epoch,
            holder_user_id: Some(owner.clone()),
            claim_request_id: None,
            idle_expires_at: None,
            acknowledged_at: T0.into(),
        })
        .unwrap();
    daemon
        .store_mut()
        .put_path_mapping(&winwincode_device_client::PathMappingRecord {
            repository_binding_id: binding.clone(),
            canonical_path: repository.to_str().unwrap().into(),
            git_common_directory: None,
            last_canonicalized_at: None,
            local_state: "ready".into(),
        })
        .unwrap();
    winwincode_device_client::record_candidate_retention(
        daemon.store_mut(),
        &winwincode_device_client::CandidateRetention {
            candidate_commit: commit.clone(),
            base_commit: base.clone(),
            repository_binding_id: binding.clone(),
            worker_session_id: "wsn_000000000000000000000001".into(),
            local_git_ref: candidate_ref_of(&commit),
            retained_at: T0.into(),
        },
    )
    .unwrap();
    daemon
        .enqueue(ClientToServerMessage::CandidateRetained(
            ClientCandidateRetainedPayload {
                occupancy: occupancy_stamp(&lease, epoch, 1, "device-recovery-retained"),
                worker_session_id: "wsn_000000000000000000000001".into(),
                receipt: winwincode_client_port::domain::LocalCandidateReceipt {
                    local_candidate_receipt_id: fresh_receipt_id("lcr_"),
                    candidate_ref: candidate_ref_of(&commit),
                    repository_binding_id: binding.clone(),
                    candidate_commit: commit.clone(),
                    base_commit: base.clone(),
                    local_ref_name: candidate_ref_of(&commit),
                    state: LocalCandidateState::Retained,
                    created_at: T0.into(),
                    revision: 1,
                },
            },
        ))
        .unwrap();
    drive_candidate_daemon_until(&mut daemon, |daemon| {
        daemon.outbox_snapshot().unwrap().frames.is_empty()
    });
    let application = stage_application_approval(
        &data_directory,
        &node,
        &owner,
        &lease,
        epoch,
        &binding,
        &commit,
        "cherry_pick",
    );
    let mut request = CandidateApplyRequest {
        application_id: application.application_id.clone(),
        approval_id: application.approval_id.clone(),
        candidate_digest: application.candidate_digest.clone(),
        reconciled_retry: false,
        repository_binding_id: binding.clone(),
        candidate_ref: application.candidate_ref.clone(),
        target_branch: "main".into(),
        expected_head: base.clone(),
        strategy: ApplyStrategy::CherryPick,
        occupancy_lease_id: lease.clone(),
        occupancy_fencing_token: epoch,
    };
    apply_candidate_with_crash_point(
        &mut daemon,
        &request,
        &integration_root,
        ApplicationCrashPoint::AfterStarted,
    )
    .unwrap_err();
    assert_eq!(git(&repository, &["rev-parse", "HEAD"]), base);
    daemon
        .enqueue(ClientToServerMessage::Heartbeat(
            winwincode_client_port::messages::ClientHeartbeatPayload {
                capacity: capacity(0),
                accepting_connections: true,
                lock_state: ClientLockState::Unlocked,
                presence_state: PresenceState::Online,
                occupancy_lease_id: Some(lease.clone()),
            },
        ))
        .unwrap();
    let retained = daemon.outbox_snapshot().unwrap();
    daemon.into_store().close().unwrap();
    let mut store = DeviceStore::open(&device_directory).unwrap();
    let restarted = ensure_device_identity(&mut store, &seed, T0).unwrap();
    assert_ne!(
        restarted.current_instance_id(),
        identity.current_instance_id()
    );
    let mut daemon =
        DeviceDaemon::start(config.clone(), store, transport.clone(), &restarted).unwrap();
    winwincode_device_client::apply_engine::recover_started_applications(&mut daemon).unwrap();
    assert_eq!(daemon.outbox_snapshot().unwrap(), retained);
    drive_candidate_daemon_until(&mut daemon, |daemon| {
        daemon.outbox_snapshot().unwrap().frames.is_empty()
            && SqliteStorage::open(&data_directory)
                .unwrap()
                .candidate_application(&application.application_id)
                .unwrap()
                .unwrap()
                .outcome
                .as_deref()
                == Some("not_started")
    });
    assert_eq!(report_recovered_applications(&mut daemon).unwrap(), 0);
    let mut storage = SqliteStorage::open(&data_directory).unwrap();
    storage
        .authorize_candidate_application_retry(&application.application_id, &lease, epoch)
        .unwrap();
    let authorized = storage
        .candidate_application(&application.application_id)
        .unwrap()
        .unwrap();
    assert_eq!(authorized.outcome.as_deref(), Some("retry_authorized"));
    assert_eq!(authorized.retry_epoch, Some(epoch));
    drop(storage);
    request.reconciled_retry = true;
    apply_candidate_with_crash_point(
        &mut daemon,
        &request,
        &integration_root,
        ApplicationCrashPoint::AfterRepositoryCommit,
    )
    .unwrap_err();
    let applied = git(&repository, &["rev-parse", "HEAD"]);
    assert_ne!(applied, base);
    assert!(daemon.outbox_snapshot().unwrap().frames.is_empty());
    daemon.into_store().close().unwrap();
    let mut store = DeviceStore::open(&device_directory).unwrap();
    let restarted_again = ensure_device_identity(&mut store, &seed, T0).unwrap();
    assert_ne!(
        restarted_again.current_instance_id(),
        restarted.current_instance_id()
    );
    let mut daemon = DeviceDaemon::start(config, store, transport, &restarted_again).unwrap();
    drive_candidate_daemon_until(&mut daemon, |daemon| {
        daemon.status().exchanges_started > 0 && daemon.outbox_snapshot().unwrap().frames.is_empty()
    });
    let application_after = SqliteStorage::open(&data_directory)
        .unwrap()
        .candidate_application(&application.application_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        application_after.outcome.as_deref(),
        Some("applied"),
        "bootstrap must deliver the changed fact before another recovery request"
    );
    assert_eq!(application_after.application_id, application.application_id);
    assert_eq!(application_after.approval_id, application.approval_id);
    assert!(application_after.result_json.is_some());
    let listing = http_request(
        address,
        &cookie_get(&format!("/api/v1/clients/{public}/candidates"), &cookie),
    )
    .await;
    assert_eq!(status_of(&listing), "200", "{listing}");
    let listed = response_body(&listing);
    assert_eq!(listed["candidates"][0]["application"]["status"], "applied");
    assert_eq!(
        listed["candidates"][0]["application"]["approvalId"],
        application.approval_id
    );
    assert_eq!(report_recovered_applications(&mut daemon).unwrap(), 0);
    assert!(daemon.outbox_snapshot().unwrap().frames.is_empty());
    assert_eq!(daemon.occupancy_mirror().unwrap().fencing_token, epoch);
    let trailer = format!("WinWinCode-Application: {}", application.application_id);
    assert_eq!(
        git(&repository, &["log", "--format=%B", "HEAD"])
            .lines()
            .filter(|line| *line == trailer)
            .count(),
        1
    );
    assert_eq!(
        next_client_sequence(&data_directory, &node),
        daemon.outbox_snapshot().unwrap().highest_sequence + 1
    );
    std::fs::write(repository.join("retry.txt"), "next candidate\n").unwrap();
    git(&repository, &["add", "-A"]);
    git(
        &repository,
        &[
            "-c",
            "user.name=Recovery Test",
            "-c",
            "user.email=recovery@example.test",
            "commit",
            "-q",
            "-m",
            "retry candidate",
        ],
    );
    let retry_candidate = git(&repository, &["rev-parse", "HEAD"]);
    let retry_ref = candidate_ref_of(&retry_candidate);
    git(&repository, &["update-ref", &retry_ref, &retry_candidate]);
    git(&repository, &["reset", "-q", "--hard", &applied]);
    winwincode_device_client::record_candidate_retention(
        daemon.store_mut(),
        &winwincode_device_client::CandidateRetention {
            candidate_commit: retry_candidate.clone(),
            base_commit: applied.clone(),
            repository_binding_id: binding.clone(),
            worker_session_id: "wsn_000000000000000000000001".into(),
            local_git_ref: retry_ref.clone(),
            retained_at: T0.into(),
        },
    )
    .unwrap();
    daemon
        .enqueue(ClientToServerMessage::CandidateRetained(
            ClientCandidateRetainedPayload {
                occupancy: occupancy_stamp(&lease, epoch, 1, "device-recovery-retry-retained"),
                worker_session_id: "wsn_000000000000000000000001".into(),
                receipt: winwincode_client_port::domain::LocalCandidateReceipt {
                    local_candidate_receipt_id: fresh_receipt_id("lcr_"),
                    candidate_ref: retry_ref.clone(),
                    repository_binding_id: binding.clone(),
                    candidate_commit: retry_candidate,
                    base_commit: applied.clone(),
                    local_ref_name: retry_ref.clone(),
                    state: LocalCandidateState::Retained,
                    created_at: T0.into(),
                    revision: 1,
                },
            },
        ))
        .unwrap();
    drive_candidate_daemon_until(&mut daemon, |daemon| {
        daemon.outbox_snapshot().unwrap().frames.is_empty()
    });
    std::fs::write(repository.join("scratch.txt"), "user work\n").unwrap();
    let retry_body = apply_body(&public, &retry_ref, &binding, "main", &applied);
    let initial_post = cookie_post("/api/v1/clients/candidates/apply", &retry_body, &cookie);
    let initial_waiter = tokio::spawn(async move { http_request(address, &initial_post).await });
    drive_candidate_daemon_until(&mut daemon, |daemon| {
        daemon.outbox_snapshot().unwrap().frames.is_empty()
            && SqliteStorage::open(&data_directory)
                .unwrap()
                .candidate_application_for_candidate(&node, &retry_ref)
                .unwrap()
                .is_some_and(|application| application.outcome.as_deref() == Some("not_started"))
    });
    let initial_response = initial_waiter.await.unwrap();
    assert_eq!(
        status_of(&initial_response),
        "409",
        "the original waiter must receive the retryable refusal: {initial_response}"
    );
    let unstarted = SqliteStorage::open(&data_directory)
        .unwrap()
        .candidate_application_for_candidate(&node, &retry_ref)
        .unwrap()
        .unwrap();
    assert!(unstarted.result_json.is_none());
    assert_eq!(
        candidate_state(&data_directory, &node, &retry_ref).as_deref(),
        Some("retained")
    );
    std::fs::remove_file(repository.join("scratch.txt")).unwrap();
    let retry_post = cookie_post("/api/v1/clients/candidates/apply", &retry_body, &cookie);
    let retry_waiter = tokio::spawn(async move { http_request(address, &retry_post).await });
    drive_candidate_daemon_until(&mut daemon, |daemon| {
        daemon.outbox_snapshot().unwrap().frames.is_empty()
            && SqliteStorage::open(&data_directory)
                .unwrap()
                .candidate_application(&unstarted.application_id)
                .unwrap()
                .unwrap()
                .outcome
                .as_deref()
                == Some("applied")
    });
    let retry_response = retry_waiter.await.unwrap();
    assert_eq!(status_of(&retry_response), "201", "{retry_response}");
    let retried = SqliteStorage::open(&data_directory)
        .unwrap()
        .candidate_application(&unstarted.application_id)
        .unwrap()
        .unwrap();
    assert_eq!(retried.approval_id, unstarted.approval_id);
    assert_eq!(apply_history_len(&data_directory, &node, &retry_ref), 1);
    let head_after_retry = git(&repository, &["rev-parse", "HEAD"]);
    let release =
        json!({"schemaVersion":SCHEMA_VERSION,"clientId":public,"mode":"release"}).to_string();
    let response = http_request(
        address,
        &cookie_post("/api/v1/clients/occupancy", &release, &cookie)
            .replacen("POST ", "DELETE ", 1),
    )
    .await;
    assert_eq!(status_of(&response), "200", "{response}");
    let claim = cookie_post(
        "/api/v1/clients/occupancy",
        &json!({"schemaVersion":SCHEMA_VERSION,"clientId":public}).to_string(),
        &cookie,
    );
    let claim_task = tokio::spawn(async move { http_request(address, &claim).await });
    drive_candidate_daemon_until(&mut daemon, |daemon| {
        daemon
            .occupancy_mirror()
            .is_some_and(|mirror| mirror.fencing_token > epoch)
            && daemon.outbox_snapshot().unwrap().frames.is_empty()
    });
    let response = claim_task.await.unwrap();
    assert_eq!(status_of(&response), "201", "{response}");
    let new_epoch = daemon.occupancy_mirror().unwrap().fencing_token;
    let storage = SqliteStorage::open(&data_directory).unwrap();
    let database = rusqlite::Connection::open(storage.database_path()).unwrap();
    let mut events = database
        .prepare(
            "SELECT epoch,payload FROM candidate_application_events WHERE application_id=?1
         AND kind='application.reconciled' ORDER BY rowid",
        )
        .unwrap();
    let facts: Vec<(i64, String)> = events
        .query_map([&application.application_id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        facts.iter().map(|fact| fact.0).collect::<Vec<_>>(),
        [
            i64::try_from(epoch).unwrap(),
            i64::try_from(epoch).unwrap(),
            i64::try_from(new_epoch).unwrap()
        ]
    );
    for (index, (_, payload)) in facts.into_iter().enumerate() {
        let payload: ClientApplicationReconciledPayload = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload.original_epoch, epoch);
        assert_eq!(payload.approval_id, application.approval_id);
        assert_eq!(
            payload.outcome,
            if index == 0 {
                ApplicationReconciliationOutcome::NotStarted
            } else {
                ApplicationReconciliationOutcome::Applied
            }
        );
        assert_eq!(
            payload.result_commit.as_deref(),
            (index > 0).then_some(applied.as_str())
        );
    }
    assert_eq!(
        apply_history_len(&data_directory, &node, &application.candidate_ref),
        1
    );
    assert_eq!(git(&repository, &["rev-parse", "HEAD"]), head_after_retry);
    assert_eq!(
        git(&repository, &["log", "--format=%B", "HEAD"])
            .lines()
            .filter(|line| *line == trailer)
            .count(),
        1
    );
    daemon.into_store().close().unwrap();
    running.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(data_directory);
    let _ = std::fs::remove_dir_all(auth_directory);
    let _ = std::fs::remove_dir_all(device_directory);
    let _ = std::fs::remove_dir_all(repository);
    let _ = std::fs::remove_dir_all(integration_root);
}

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
    let (node, public_client_id) = enroll_online_device(address, &data_directory).await;
    consume_code_as(
        &data_directory,
        &node,
        &publish_connect_code(&data_directory, &node, "68421975"),
        &owner_id,
    );
    let visible_binding = stage_visible_binding(&data_directory, &node, &owner_id);
    let invisible_binding = stage_invisible_binding(&data_directory, &node);

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

    let commit = "0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776";
    let hidden_commit = "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d";
    let retained = send_retained(
        address,
        &data_directory,
        &node,
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
        "ocl_test",
        1,
        1,
        &invisible_binding,
        hidden_commit,
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

    let (status, _) = post_exchange(
        address,
        &exchange_request(
            &[frame(
                &node,
                DEVICE_INSTANCE,
                next_client_sequence(&data_directory, &node),
                retained,
            )],
            downlink_ack_cursor(&data_directory, &node),
        ),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
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

    let (node, public_client_id) = enroll_online_device(address, &data_directory).await;
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
        DEVICE_INSTANCE.into(),
        None,
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

    assert_eq!(
        candidate_state(&data_directory, &node, &candidate_ref_of(commit)).expect("candidate"),
        "branch_created"
    );
    assert_eq!(
        apply_history_len(&data_directory, &node, &candidate_ref_of(commit)),
        1
    );

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

    let (node, public_client_id) = enroll_online_device(address, &data_directory).await;
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
        &lease_id,
        token,
        1,
        &binding,
        stale_commit,
    )
    .await;

    let responder = spawn_candidate_responder(
        data_directory.clone(),
        address,
        node.clone(),
        DEVICE_INSTANCE.into(),
        None,
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
    assert_eq!(value["receipt"]["strategy"], json!("merge"));
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

    let responder = spawn_candidate_responder(
        data_directory.clone(),
        address,
        node.clone(),
        DEVICE_INSTANCE.into(),
        None,
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
async fn rejected_old_instance_apply_can_be_retried_with_the_original_approval() {
    const RECOVERED_INSTANCE: &str = "cix_C3C3C3C3C3C3C3C3C3C3C3C3C3";
    let data_directory = test_directory("candidate-instance-retry");
    let auth_directory = test_directory("candidate-instance-retry-auth");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (_cookie, owner) = initialize_and_login_owner(address).await;
    let (node, public) = enroll_online_device(address, &data_directory).await;
    consume_code_as(
        &data_directory,
        &node,
        &publish_connect_code(&data_directory, &node, "34343434"),
        &owner,
    );
    let binding = stage_visible_binding(&data_directory, &node, &owner);
    let (lease, epoch) = stage_occupied_lease(&data_directory, &node, &owner);
    let commit = "0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776";
    send_retained(
        address,
        &data_directory,
        &node,
        &lease,
        epoch,
        1,
        &binding,
        commit,
    )
    .await;
    let request: Value = serde_json::from_str(&apply_body(
        &public,
        &candidate_ref_of(commit),
        &binding,
        "main",
        FIXTURE_BASE_COMMIT,
    ))
    .unwrap();
    let config = ClientCandidatesConfig {
        apply_wait: Duration::from_millis(20),
        poll_interval: Duration::from_millis(1),
    };
    let application = ClientCandidatesApplication::open(&data_directory, &config).unwrap();
    let first = application
        .apply_candidate(&owner, &request)
        .await
        .expect_err("the rejected command has no apply result");
    assert_eq!(first.kind(), ClientCandidatesErrorKind::ApplyResultTimeout);

    let (downlink, original) = {
        let mut storage = SqliteStorage::open(&data_directory).unwrap();
        let downlink = storage
            .client_downlink_outbox()
            .unwrap()
            .deliverable(&node, 0, 1)
            .unwrap()
            .pop()
            .unwrap();
        let original = storage
            .candidate_application_for_candidate(&node, &candidate_ref_of(commit))
            .unwrap()
            .unwrap();
        (downlink, original)
    };
    let queued: Value = serde_json::from_str(&downlink.frame).unwrap();
    assert_eq!(queued["payload"]["applicationId"], original.application_id);
    assert_eq!(queued["payload"]["approvalId"], original.approval_id);

    let sequence = next_client_sequence(&data_directory, &node);
    let hello = ClientToServerMessage::Hello(ClientHelloPayload {
        client_version: "0.1.0-alpha.1".into(),
        capacity: capacity(0),
        accepting_connections: true,
        lock_state: ClientLockState::Unlocked,
        presence_state: PresenceState::Online,
    });
    let rejected = ClientToServerMessage::CommandAck(ClientCommandAckPayload {
        command_kind: ClientControlMessageKind::CandidateApply,
        command_message_id: downlink.message_id,
        status: CommandAckStatus::RejectedStaleFencingToken,
        current_revision: Some(1),
        error: Some(ClientControlError {
            code: ClientControlErrorCode::DeviceInstanceChanged,
            message: "the candidate command has obsolete execution authority".into(),
            retryable: false,
        }),
    });
    let (status, _) = post_exchange(
        address,
        &exchange_request(
            &[
                frame(&node, RECOVERED_INSTANCE, sequence, hello),
                frame(&node, RECOVERED_INSTANCE, sequence + 1, rejected),
            ],
            downlink.sequence,
        ),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert_eq!(
        application.list(&owner, &public).unwrap()["candidates"][0]["application"]["status"],
        "applying"
    );

    let responder = spawn_candidate_responder(
        data_directory.clone(),
        address,
        node.clone(),
        RECOVERED_INSTANCE.into(),
        Some((
            original.application_id.clone(),
            original.approval_id.clone(),
        )),
        downlink_ack_cursor(&data_directory, &node),
        next_client_sequence(&data_directory, &node),
        ApplyVerdict::Applied,
    );
    let recovery =
        ClientCandidatesApplication::open(&data_directory, &ClientCandidatesConfig::default())
            .unwrap();
    let result = recovery.apply_candidate(&owner, &request).await.unwrap();
    assert_eq!(
        result["receipt"]["localApplyReceiptId"],
        original.application_id
    );
    assert_eq!(result["receipt"]["result"], "applied");
    responder.abort();

    let connection =
        rusqlite::Connection::open(data_directory.join("control-plane.sqlite3")).unwrap();
    let applications: i64 = connection
        .query_row("SELECT count(*) FROM candidate_applications", [], |row| {
            row.get(0)
        })
        .unwrap();
    let approvals: i64 = connection
        .query_row(
            "SELECT count(*) FROM candidate_application_events WHERE kind='approval.granted'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!((applications, approvals), (1, 1));
    drop(connection);
    let retained = SqliteStorage::open(&data_directory)
        .unwrap()
        .candidate_application(&original.application_id)
        .unwrap()
        .unwrap();
    assert_eq!(retained.approval_id, original.approval_id);
    running.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn failed_downlink_publication_rolls_back_approval_and_retry_across_restart() {
    for retry in [false, true] {
        let directory = test_directory("candidate-publication-rollback");
        let auth_directory = test_directory("candidate-publication-rollback-auth");
        let running = start_server(&directory, &auth_directory).await;
        let address = running.local_address();
        let (cookie, owner) = initialize_and_login_owner(address).await;
        let (node, public) = enroll_online_device(address, &directory).await;
        consume_code_as(
            &directory,
            &node,
            &publish_connect_code(&directory, &node, "78787878"),
            &owner,
        );
        let binding = stage_visible_binding(&directory, &node, &owner);
        let (lease, epoch) = stage_occupied_lease(&directory, &node, &owner);
        let commit = "0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776";
        send_retained(
            address, &directory, &node, &lease, epoch, 1, &binding, commit,
        )
        .await;
        let original = if retry {
            let application = stage_application_approval(
                &directory, &node, &owner, &lease, epoch, &binding, commit, "merge",
            );
            send_application_reconciliation(
                address,
                &directory,
                &application,
                &lease,
                epoch,
                ApplicationReconciliationOutcome::NotStarted,
            )
            .await;
            Some(application)
        } else {
            None
        };
        let database = directory.join("control-plane.sqlite3");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection.execute_batch(
            "CREATE TRIGGER interrupt_candidate_publication BEFORE INSERT ON client_downlink_frames
             WHEN json_extract(NEW.frame, '$.kind') = 'client.candidate.apply'
             BEGIN
               SELECT CASE WHEN NOT EXISTS (
                 SELECT 1 FROM candidate_applications a
                 WHERE a.application_id = json_extract(NEW.frame, '$.payload.applicationId')
                   AND a.approval_id = json_extract(NEW.frame, '$.payload.approvalId')
               ) THEN RAISE(ABORT, 'approval must precede the fault') END;
               SELECT RAISE(ABORT, 'injected fault after approval before downlink publication');
             END;",
        ).unwrap();
        let body = apply_body(
            &public,
            &candidate_ref_of(commit),
            &binding,
            "main",
            FIXTURE_BASE_COMMIT,
        );
        let response = http_request(
            address,
            &cookie_post("/api/v1/clients/candidates/apply", &body, &cookie),
        )
        .await;
        assert_eq!(status_of(&response), "503", "{response}");
        drop(connection);
        running.shutdown().await.unwrap();
        let storage = SqliteStorage::open(&directory).unwrap();
        let retained = storage
            .candidate_application_for_candidate(&node, &candidate_ref_of(commit))
            .unwrap();
        if let Some(original) = &original {
            let retained = retained.unwrap();
            assert_eq!(retained.application_id, original.application_id);
            assert_eq!(retained.outcome.as_deref(), Some("not_started"));
            assert_eq!(retained.retry_lease_id, None);
            assert_eq!(retained.retry_epoch, None);
        } else {
            assert!(
                retained.is_none(),
                "failed publication cannot leave an applying approval"
            );
        }
        drop(storage);
        let connection = rusqlite::Connection::open(&database).unwrap();
        let approvals: i64 = connection
            .query_row(
                "SELECT count(*) FROM candidate_application_events WHERE kind='approval.granted'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(approvals, i64::from(retry));
        let queued: i64 = connection.query_row(
            "SELECT count(*) FROM client_downlink_frames WHERE json_extract(frame, '$.kind')='client.candidate.apply'", [], |row| row.get(0),
        ).unwrap();
        assert_eq!(queued, 0);
        connection
            .execute_batch("DROP TRIGGER interrupt_candidate_publication")
            .unwrap();
        drop(connection);
        let restarted = start_server(&directory, &auth_directory).await;
        let address = restarted.local_address();
        walk_hello(address, &directory, &node, DEVICE_INSTANCE).await;
        let listing = http_request(
            address,
            &cookie_get(&format!("/api/v1/clients/{public}/candidates"), &cookie),
        )
        .await;
        assert_eq!(status_of(&listing), "200", "{listing}");
        let application = &response_body(&listing)["candidates"][0]["application"];
        if retry {
            assert_eq!(application["status"], "awaiting_retry");
        } else {
            assert!(application.is_null());
        }
        let responder = spawn_candidate_responder(
            directory.clone(),
            address,
            node.clone(),
            DEVICE_INSTANCE.into(),
            None,
            downlink_ack_cursor(&directory, &node),
            next_client_sequence(&directory, &node),
            ApplyVerdict::Applied,
        );
        let response = http_request(
            address,
            &cookie_post("/api/v1/clients/candidates/apply", &body, &cookie),
        )
        .await;
        assert_eq!(status_of(&response), "201", "{response}");
        let value = response_body(&response);
        assert_eq!(value["receipt"]["result"], "applied");
        if let Some(original) = original {
            assert_eq!(
                value["receipt"]["localApplyReceiptId"],
                original.application_id
            );
            let storage = SqliteStorage::open(&directory).unwrap();
            let applied = storage
                .candidate_application(&original.application_id)
                .unwrap()
                .unwrap();
            assert_eq!(applied.retry_lease_id.as_deref(), Some(lease.as_str()));
            assert_eq!(applied.retry_epoch, Some(epoch));
        }
        responder.abort();
        restarted.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(&directory);
        let _ = std::fs::remove_dir_all(&auth_directory);
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn approved_old_results_are_facts_and_only_current_reconciliation_settles() {
    let data_directory = test_directory("candidates-fencing");
    let auth_directory = test_directory("candidates-fencing-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (_owner_cookie, owner_id) = initialize_and_login_owner(address).await;

    let (node, _public_client_id) = enroll_online_device(address, &data_directory).await;
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
        &first_lease,
        first_token,
        1,
        &binding,
        commit,
    )
    .await;

    let application = stage_application_approval(
        &data_directory,
        &node,
        &owner_id,
        &first_lease,
        first_token,
        &binding,
        commit,
        "cherry_pick",
    );
    release_lease(&data_directory, &first_lease, first_token);
    let (second_lease, second_token) = stage_occupied_lease(&data_directory, &node, &owner_id);
    assert!(second_token > first_token, "tokens are strictly monotonic");

    send_apply_result(
        address,
        &data_directory,
        &node,
        &first_lease,
        first_token,
        &application.application_id,
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

    send_apply_result(
        address,
        &data_directory,
        &node,
        &second_lease,
        second_token - 1,
        &application.application_id,
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

    send_application_reconciliation(
        address,
        &data_directory,
        &application,
        &second_lease,
        second_token,
        ApplicationReconciliationOutcome::Applied,
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
    send_application_reconciliation(
        address,
        &data_directory,
        &application,
        &second_lease,
        second_token,
        ApplicationReconciliationOutcome::Applied,
    )
    .await;
    assert_eq!(
        apply_history_len(&data_directory, &node, &candidate_ref_of(commit)),
        1
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

    let (node, public_client_id) = enroll_online_device(address, &data_directory).await;
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
async fn reconciled_application_records_the_commit_and_preserves_a_discarded_candidate() {
    let data_directory = test_directory("candidates-reconcile-terminal");
    let auth_directory = test_directory("candidates-reconcile-terminal-auth");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (cookie, owner) = initialize_and_login_owner(address).await;
    let (node, public) = enroll_online_device(address, &data_directory).await;
    consume_code_as(
        &data_directory,
        &node,
        &publish_connect_code(&data_directory, &node, "56565656"),
        &owner,
    );
    let binding = stage_visible_binding(&data_directory, &node, &owner);
    let (old_lease, old_epoch) = stage_occupied_lease(&data_directory, &node, &owner);
    let commit = "0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776";
    send_retained(
        address,
        &data_directory,
        &node,
        &old_lease,
        old_epoch,
        1,
        &binding,
        commit,
    )
    .await;
    let application = stage_application_approval(
        &data_directory,
        &node,
        &owner,
        &old_lease,
        old_epoch,
        &binding,
        commit,
        "cherry_pick",
    );
    release_lease(&data_directory, &old_lease, old_epoch);
    let (lease, epoch) = stage_occupied_lease(&data_directory, &node, &owner);
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/candidates/discard",
            &branch_body(&public, &candidate_ref_of(commit), &binding),
            &cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "200", "{response}");
    send_application_reconciliation(
        address,
        &data_directory,
        &application,
        &lease,
        epoch,
        ApplicationReconciliationOutcome::Applied,
    )
    .await;
    assert_eq!(
        candidate_state(&data_directory, &node, &candidate_ref_of(commit)).as_deref(),
        Some("discarded")
    );
    assert_eq!(
        apply_history_len(&data_directory, &node, &candidate_ref_of(commit)),
        2
    );
    let storage = SqliteStorage::open(&data_directory).unwrap();
    assert_eq!(
        storage
            .candidate_application(&application.application_id)
            .unwrap()
            .unwrap()
            .outcome
            .as_deref(),
        Some("applied")
    );
    running.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[tokio::test]
async fn an_unknown_application_is_visible_as_blocked_and_cannot_be_retried() {
    let data_directory = test_directory("candidates-reconcile-unknown");
    let auth_directory = test_directory("candidates-reconcile-unknown-auth");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (cookie, owner) = initialize_and_login_owner(address).await;
    let (node, public) = enroll_online_device(address, &data_directory).await;
    consume_code_as(
        &data_directory,
        &node,
        &publish_connect_code(&data_directory, &node, "67676767"),
        &owner,
    );
    let binding = stage_visible_binding(&data_directory, &node, &owner);
    let (old_lease, old_epoch) = stage_occupied_lease(&data_directory, &node, &owner);
    let commit = "0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776";
    send_retained(
        address,
        &data_directory,
        &node,
        &old_lease,
        old_epoch,
        1,
        &binding,
        commit,
    )
    .await;
    let application = stage_application_approval(
        &data_directory,
        &node,
        &owner,
        &old_lease,
        old_epoch,
        &binding,
        commit,
        "cherry_pick",
    );
    release_lease(&data_directory, &old_lease, old_epoch);
    let (lease, epoch) = stage_occupied_lease(&data_directory, &node, &owner);
    send_application_reconciliation(
        address,
        &data_directory,
        &application,
        &lease,
        epoch,
        ApplicationReconciliationOutcome::OutcomeUnknown,
    )
    .await;
    let response = http_request(
        address,
        &cookie_get(&format!("/api/v1/clients/{public}/candidates"), &cookie),
    )
    .await;
    assert_eq!(
        response_body(&response)["candidates"][0]["application"]["status"],
        "blocked"
    );
    let before = downlink_ack_cursor(&data_directory, &node);
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/candidates/apply",
            &apply_body(
                &public,
                &candidate_ref_of(commit),
                &binding,
                "main",
                &application.base_commit,
            ),
            &cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "409", "{response}");
    assert_eq!(wire_code(&response), "WRONG_STATE");
    assert_eq!(downlink_ack_cursor(&data_directory, &node), before);
    assert_eq!(
        candidate_state(&data_directory, &node, &candidate_ref_of(commit)).as_deref(),
        Some("retained")
    );
    let mut storage = SqliteStorage::open(&data_directory).unwrap();
    assert!(
        storage
            .note_candidate_application_outcome(&application.application_id, "not_started")
            .is_err()
    );
    assert!(
        storage
            .authorize_candidate_application_retry(&application.application_id, &lease, epoch)
            .is_err()
    );
    running.shutdown().await.unwrap();
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
    let (node, public_client_id) = enroll_online_device(address, &data_directory).await;
    consume_code_as(
        &data_directory,
        &node,
        &publish_connect_code(&data_directory, &node, "56565656"),
        &owner_id,
    );
    let invisible = stage_invisible_binding(&data_directory, &node);
    let visible = stage_visible_binding(&data_directory, &node, &owner_id);
    let (lease_id, token) = stage_occupied_lease(&data_directory, &node, &owner_id);
    let commit = "0f9e8d7c6b5a4938271605f4e3d2c1b0a9988776";
    let hidden_commit = "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d";
    send_retained(
        address,
        &data_directory,
        &node,
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
        &lease_id,
        token,
        1,
        &invisible,
        hidden_commit,
    )
    .await;

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
