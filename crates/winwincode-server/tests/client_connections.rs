// SPDX-License-Identifier: Apache-2.0

//! The user-facing Client connection flow over real HTTP: the three
//! `/api/v1/clients` routes wired to `ClientConnectionsApplication` and the
//! real client exchange, covering the §16.3 error taxonomy, the bounded
//! challenge wait driven by a fake device that answers
//! `client.access.challenge_ack` over the exchange protocol, the atomic
//! consume-and-grant with idempotent retries, the device directory shape,
//! and immediate revocation.

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rusqlite::Connection;
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
use winwincode_client_port::domain::ClientChallengeAckStatus;
use winwincode_client_port::domain::ClientLockState;
use winwincode_client_port::domain::ClientPlatformTarget;
use winwincode_client_port::domain::PresenceState;
use winwincode_client_port::messages::ClientAccessChallengeAckPayload;
use winwincode_client_port::messages::ClientEnrollPayload;
use winwincode_client_port::messages::ClientHelloPayload;
use winwincode_client_port::messages::ClientToServerEnvelope;
use winwincode_client_port::messages::ClientToServerMessage;
use winwincode_client_port::messages::CommandContext;
use winwincode_control_plane::AccessGrantService;
use winwincode_control_plane::ClientRegistryService;
use winwincode_control_plane::ConnectCodeService;
use winwincode_domain::Instant;
use winwincode_server::{
    ApiError, AuthSessionBootstrap, AuthSessionConfig, AuthenticatedPrincipal,
    ClientConnectionsApplication, ClientConnectionsConfig, ClientConnectionsErrorKind,
    ClientExchangeApplication, ClientExchangeConfig, ClientExchangePort, ControlPlaneApiPort,
    EventSubscription, RequestAuthenticator, ServerConfig, ServerTls, SqliteAuthSessionManager,
    UserAccountService, start_server_with_remote_worker,
};
use winwincode_storage::{
    AccessGrantIssuance, ClientDownlinkAppend, ClientNodeRegistration, ClientPresenceState,
    ConnectCodeConsume, ConnectCodePublication, GrantTrustMode, SqliteStorage,
};

const BOOTSTRAP_PROOF: &str = "client-connections-test-bootstrap";
const ORIGIN: &str = "https://client.example";
const OWNER_PASSWORD: &str = "initial-owner-password";
/// Placeholder client node id a fresh device sends before the server assigns
/// the canonical `cnd_` identity.
const FRESH_NODE: &str = "device-local-pending";
const INSTANCE: &str = "cix_A1A1A1A1A1A1A1A1A1A1A1A1A1";
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
            "unused in client connection tests",
        ))
    }

    fn query(&self, _: &AuthenticatedPrincipal, _: Value) -> Result<Value, ApiError> {
        Err(ApiError::new(
            501,
            "NOT_IMPLEMENTED",
            "unused in client connection tests",
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
    let accounts = Arc::new(UserAccountService::open(directory).expect("account service"));
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

fn capacity() -> ClientCapacityReport {
    ClientCapacityReport {
        max_concurrent_worker_sessions: 4,
        running_worker_sessions: 0,
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
        &exchange_request(&[frame(FRESH_NODE, INSTANCE, 1, enroll)], 0),
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
    assert_eq!(
        body["frames"].as_array().expect("downlink batch").len(),
        1,
        "the enrollment acceptance frame is delivered in the same response"
    );
    (node, public_client_id, credential, 2)
}

/// Walks the announcement hello so the node projects `online`.
async fn walk_hello(
    address: std::net::SocketAddr,
    node: &str,
    instance: &str,
    credential: &str,
    sequence: u64,
) {
    let hello = ClientToServerMessage::Hello(ClientHelloPayload {
        client_version: "0.1.0-alpha.1".to_owned(),
        capacity: capacity(),
        accepting_connections: true,
        lock_state: ClientLockState::Unlocked,
        presence_state: PresenceState::Online,
    });
    let (status, body) = post_exchange(
        address,
        &exchange_request(&[frame(node, instance, sequence, hello)], sequence - 1),
        Some(credential),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert_eq!(
        body.expect("hello response body")["ackSequence"],
        json!(sequence)
    );
}

/// Enrolls and walks hello: the node is `online` with a credential.
async fn enroll_online_device(address: std::net::SocketAddr) -> (String, String, String, u64) {
    let (node, public_client_id, credential, next) = enroll_device(address).await;
    walk_hello(address, &node, INSTANCE, &credential, next).await;
    (node, public_client_id, credential, next + 1)
}

/// Publishes one connect code digest the way the Device Client would, and
/// returns the 8-digit code the user will type.
fn publish_connect_code(data_directory: &Path, node: &str, code: &str) -> String {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut connect = ConnectCodeService::new(&mut storage);
    let now = Instant("2026-09-04T12:00:00.000Z".to_owned());
    let digest = format!("sha256:{:x}", Sha256::digest(code.as_bytes()));
    let publication = ConnectCodePublication::try_new(
        fresh_code_id(),
        digest,
        node,
        INSTANCE,
        1,
        Instant(VALID_UNTIL.to_owned()),
        5,
    )
    .expect("valid publication");
    connect.publish(&publication, &now).expect("publish code");
    code.to_owned()
}

/// Consumes one published code directly, staging a grant another user (or a
/// concurrent winner) would have taken.
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

fn set_presence(data_directory: &Path, node: &str, presence: ClientPresenceState) {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut registry = ClientRegistryService::new(&mut storage);
    let record = registry.snapshot(node).expect("snapshot").expect("node");
    registry
        .update_presence(node, presence, record.revision)
        .expect("presence transition");
}

/// Rewrites one registry column of a node directly (test staging for device
/// local facts no public API mutates).
fn set_node_column(data_directory: &Path, node: &str, assignment: &str) {
    let connection =
        Connection::open(data_directory.join("control-plane.sqlite3")).expect("open database");
    connection
        .execute(
            &format!("UPDATE client_nodes SET {assignment} WHERE client_node_id = ?1"),
            [node],
        )
        .expect("node column update");
}

fn expire_all_connect_codes(data_directory: &Path) {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut connect = ConnectCodeService::new(&mut storage);
    let expired = connect
        .expire_codes_due(&Instant("2100-01-01T00:00:00.000Z".to_owned()))
        .expect("expire codes");
    assert!(!expired.is_empty(), "the published code must be expired");
}

/// The fake device: polls the durable downlink outbox like the real daemon
/// polls its inbox, and answers every `client.access.challenge` with a
/// confirmed `client.access.challenge_ack` frame over the exchange protocol.
fn spawn_challenge_responder(
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
                let frame_value: Value = serde_json::from_str(&stored.frame).expect("frame json");
                inbox_ack = stored.sequence;
                if frame_value["kind"] != json!("client.access.challenge") {
                    continue;
                }
                let payload = &frame_value["payload"];
                let ack_frame = ClientToServerMessage::AccessChallengeAck(Box::new(
                    ClientAccessChallengeAckPayload {
                        command: CommandContext {
                            expected_revision: 0,
                            idempotency_key: format!(
                                "idem_ack_{}",
                                payload["challengeId"].as_str().expect("challenge id")
                            ),
                        },
                        challenge_id: payload["challengeId"]
                            .as_str()
                            .expect("challenge id")
                            .to_owned(),
                        connect_code_id: payload["connectCodeId"]
                            .as_str()
                            .expect("connect code id")
                            .to_owned(),
                        status: ClientChallengeAckStatus::Confirmed,
                    },
                ));
                let request = exchange_request(
                    &[frame(&node, INSTANCE, next_sequence, ack_frame)],
                    stored.sequence,
                );
                next_sequence += 1;
                let (status, body) = post_exchange(address, &request, Some(&credential)).await;
                assert!(status.starts_with("HTTP/1.1 200"), "{status} {body:?}");
            }
        }
    })
}

fn connect_body(client_id: &str, code: &str) -> String {
    json!({
        "schemaVersion": SCHEMA_VERSION,
        "clientId": client_id,
        "connectionCode": code,
    })
    .to_string()
}

fn wire_code(response: &str) -> String {
    response_body(response)["error"]["code"]
        .as_str()
        .expect("error code")
        .to_owned()
}

// ---- tests ----------------------------------------------------------------

#[tokio::test]
async fn connect_and_directory_require_a_signed_in_session() {
    let data_directory = test_directory("client-connect-auth");
    let auth_directory = test_directory("client-connect-auth-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();

    let response = http_request(
        address,
        &plain_post(
            "/api/v1/clients/connections",
            &connect_body("927351842", "12345678"),
        ),
    )
    .await;
    assert_eq!(status_of(&response), "401");
    assert_eq!(wire_code(&response), "AUTHENTICATION_REQUIRED");

    let response = http_request(address, &cookie_get("/api/v1/clients", "not-a-session")).await;
    assert_eq!(status_of(&response), "401");

    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/grants/revoke",
            &json!({
                "schemaVersion": SCHEMA_VERSION,
                "clientId": "927351842",
            })
            .to_string(),
            "not-a-session",
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
async fn connect_reports_every_domain_failure_of_the_taxonomy() {
    let data_directory = test_directory("client-connect-errors");
    let auth_directory = test_directory("client-connect-errors-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (cookie, _user_id) = initialize_and_login_owner(address).await;

    // Unknown Client ID.
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/connections",
            &connect_body("000000000", "12345678"),
            &cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "404", "{response}");
    assert_eq!(wire_code(&response), "CLIENT_NOT_FOUND");

    // A pending-enrollment device is not a connectable Client yet.
    let (node, public_client_id, credential, next) = enroll_device(address).await;
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/connections",
            &connect_body(&public_client_id, "12345678"),
            &cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "404", "{response}");
    assert_eq!(wire_code(&response), "CLIENT_NOT_FOUND");

    // A malformed input is a request failure, not a domain error.
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/connections",
            &connect_body(&public_client_id, "1234"),
            &cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "400", "{response}");
    assert_eq!(wire_code(&response), "INVALID_REQUEST");

    // The device walks hello and goes online; a wrong code is invalid
    // (attempt 1 of the fixed window).
    // A hello from a fresh launch instance takes the device projection over
    // and carries the reported capacity (4 worker sessions).
    walk_hello(
        address,
        &node,
        "cix_B2B2B2B2B2B2B2B2B2B2B2B2B2",
        &credential,
        next,
    )
    .await;
    publish_connect_code(&data_directory, &node, "11112222");
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/connections",
            &connect_body(&public_client_id, "11112223"),
            &cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "409", "{response}");
    assert_eq!(wire_code(&response), "CONNECT_CODE_INVALID");

    // An expired code reads as the expiry category (attempt 2).
    let expiring = publish_connect_code(&data_directory, &node, "22223333");
    expire_all_connect_codes(&data_directory);
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/connections",
            &connect_body(&public_client_id, &expiring),
            &cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "409", "{response}");
    assert_eq!(wire_code(&response), "CONNECT_CODE_EXPIRED");

    // A consumed code is exhausted (attempt 3).
    let consumed = publish_connect_code(&data_directory, &node, "33334444");
    consume_code_as(
        &data_directory,
        &node,
        &consumed,
        "usr_22222222222222222222222222",
    );
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/connections",
            &connect_body(&public_client_id, &consumed),
            &cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "409", "{response}");
    assert_eq!(wire_code(&response), "CONNECT_CODE_EXPIRED");

    // An offline device: presence precedes any code verification.
    set_presence(&data_directory, &node, ClientPresenceState::Offline);
    let usable_code = publish_connect_code(&data_directory, &node, "44445555");
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/connections",
            &connect_body(&public_client_id, &usable_code),
            &cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "409", "{response}");
    assert_eq!(wire_code(&response), "CLIENT_OFFLINE");

    // A Client that no longer accepts connections.
    set_presence(&data_directory, &node, ClientPresenceState::Online);
    set_node_column(&data_directory, &node, "accepting_connections = 0");
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/connections",
            &connect_body(&public_client_id, &usable_code),
            &cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "409", "{response}");
    assert_eq!(wire_code(&response), "CLIENT_CONNECTIONS_FORBIDDEN");
    set_node_column(&data_directory, &node, "accepting_connections = 1");

    // A locked Client, by presence and by the machine-level lock switch.
    set_presence(&data_directory, &node, ClientPresenceState::Locked);
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/connections",
            &connect_body(&public_client_id, &usable_code),
            &cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "409", "{response}");
    assert_eq!(wire_code(&response), "CLIENT_LOCKED");
    set_presence(&data_directory, &node, ClientPresenceState::Online);
    set_node_column(&data_directory, &node, "lock_state = 'locked'");
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/connections",
            &connect_body(&public_client_id, &usable_code),
            &cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "409", "{response}");
    assert_eq!(wire_code(&response), "CLIENT_LOCKED");
    set_node_column(&data_directory, &node, "lock_state = 'unlocked'");

    // Rate limiting: the three earlier code failures plus two more burn the
    // fixed window; then even the fully valid attempt is throttled.
    for _ in 0..2 {
        let response = http_request(
            address,
            &cookie_post(
                "/api/v1/clients/connections",
                &connect_body(&public_client_id, "99999999"),
                &cookie,
            ),
        )
        .await;
        assert_eq!(status_of(&response), "409", "{response}");
        assert_eq!(wire_code(&response), "CONNECT_CODE_INVALID");
    }
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/connections",
            &connect_body(&public_client_id, &usable_code),
            &cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "429", "{response}");
    assert_eq!(wire_code(&response), "RATE_LIMITED");
    assert_eq!(response_body(&response)["error"]["retryable"], json!(true));

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn connect_completes_when_the_device_confirms_and_stays_idempotent() {
    let data_directory = test_directory("client-connect-happy");
    let auth_directory = test_directory("client-connect-happy-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (cookie, user_id) = initialize_and_login_owner(address).await;

    let (node, public_client_id, credential, next_sequence) = enroll_online_device(address).await;
    let code = publish_connect_code(&data_directory, &node, "68421975");
    let responder = spawn_challenge_responder(
        data_directory.clone(),
        address,
        node.clone(),
        credential,
        1,
        next_sequence,
    );

    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/connections",
            &connect_body(&public_client_id, &code),
            &cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "201", "{response}");
    let value = response_body(&response);
    assert_eq!(value["schemaVersion"], json!(SCHEMA_VERSION));
    let clients = value["clients"].as_array().expect("clients array");
    assert_eq!(clients.len(), 1, "the fresh directory carries the device");
    assert_eq!(clients[0]["clientId"], json!(public_client_id));
    assert_eq!(clients[0]["presence"], json!("online"));
    assert_eq!(clients[0]["occupancy"], json!("available"));

    // Durable facts: exactly one active grant with the first-user permission
    // set, the code consumed, the challenge settled, and the acked challenge
    // frame retained no longer.
    {
        let mut storage = SqliteStorage::open(&data_directory).expect("storage");
        let mut grants = AccessGrantService::new(&mut storage);
        let active = grants
            .active_grants_for_user(&user_id)
            .expect("active grants");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].client_node_id, node);
        assert_eq!(active[0].permissions.as_str(), "use+manage+share");
        let mut connect = ConnectCodeService::new(&mut storage);
        let digest = format!("sha256:{:x}", Sha256::digest(code.as_bytes()));
        let code_record = connect
            .code_snapshot_by_digest(&digest)
            .expect("code lookup")
            .expect("code");
        assert_eq!(
            code_record.state,
            winwincode_storage::ConnectCodeState::Consumed
        );
        let pending = connect
            .pending_challenge_for_subject(&node, &user_id, &code_record.connect_code_id)
            .expect("challenge lookup");
        assert!(pending.is_none(), "the challenge settled");
        let outbox = storage.client_downlink_outbox().expect("outbox");
        assert_eq!(
            outbox.deliverable(&node, 0, 100).expect("retained").len(),
            0,
            "the acked challenge frame is retained no longer"
        );
    }
    {
        let connection =
            Connection::open(data_directory.join("control-plane.sqlite3")).expect("open database");
        let audits = connection
            .query_row(
                "SELECT COUNT(*) FROM client_connect_audit WHERE action = 'client.access.granted'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("audit count");
        assert_eq!(audits, 1, "the grant authorization is audited");
    }

    // Idempotent retry: the same request returns the same device list without
    // a second grant.
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/connections",
            &connect_body(&public_client_id, &code),
            &cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "201", "{response}");
    assert_eq!(
        response_body(&response)["clients"]
            .as_array()
            .expect("clients")
            .len(),
        1,
        "no duplicate directory entry"
    );
    {
        let mut storage = SqliteStorage::open(&data_directory).expect("storage");
        let mut grants = AccessGrantService::new(&mut storage);
        assert_eq!(
            grants
                .active_grants_for_user(&user_id)
                .expect("grants")
                .len(),
            1,
            "the partial unique index keeps exactly one grant"
        );
    }

    responder.abort();
    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[tokio::test]
async fn connect_times_out_with_client_offline_and_feeds_the_rate_limit() {
    let data_directory = test_directory("client-connect-timeout");
    let auth_directory = test_directory("client-connect-timeout-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();

    let (node, public_client_id, _credential, _next) = enroll_online_device(address).await;
    let code = publish_connect_code(&data_directory, &node, "55556666");
    // No responder: the durable challenge stays pending until the deadline.
    let application = ClientConnectionsApplication::open(
        &data_directory,
        &ClientConnectionsConfig {
            challenge_wait: Duration::from_millis(300),
            poll_interval: Duration::from_millis(25),
            ..ClientConnectionsConfig::default()
        },
    )
    .expect("valid application");
    let request = json!({
        "schemaVersion": SCHEMA_VERSION,
        "clientId": public_client_id,
        "connectionCode": code,
    });
    let user = "usr_11111111111111111111111111";
    let first = application
        .connect(user, "127.0.0.1", &request)
        .await
        .expect_err("the bounded wait must fail");
    assert_eq!(first.kind(), ClientConnectionsErrorKind::ClientOffline);

    // Every retry reuses the same pending challenge and fails offline again,
    // burning one failure per attempt in all three dimensions.
    for _ in 0..4 {
        let retry = application
            .connect(user, "127.0.0.1", &request)
            .await
            .expect_err("the retry must fail");
        assert_eq!(retry.kind(), ClientConnectionsErrorKind::ClientOffline);
    }
    // Five failures reached the fixed-window threshold: the next attempt is
    // throttled before any challenge work happens.
    let throttled = application
        .connect(user, "127.0.0.1", &request)
        .await
        .expect_err("the throttled attempt must fail");
    assert_eq!(throttled.kind(), ClientConnectionsErrorKind::RateLimited);

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn directory_shape_and_immediate_revocation() {
    let data_directory = test_directory("client-connect-revoke");
    let auth_directory = test_directory("client-connect-revoke-sessions");
    let running = start_server(&data_directory, &auth_directory).await;
    let address = running.local_address();
    let (owner_cookie, owner_id) = initialize_and_login_owner(address).await;

    // A second member user for the holder-vs-owner authorization matrix.
    let create = json!({
        "schemaVersion": SCHEMA_VERSION,
        "username": "member",
        "role": "member",
    })
    .to_string();
    let response = http_request(
        address,
        &cookie_post("/api/v1/users", &create, &owner_cookie),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 201"), "{response}");
    let temporary = response_body(&response)["temporaryPassword"]
        .as_str()
        .expect("temporary password")
        .to_owned();
    let (member_cookie, member_id) = login(address, "member", &temporary).await;

    let (node, public_client_id, credential, next) = enroll_device(address).await;
    // A hello from a fresh launch instance takes the device projection over
    // and carries the reported capacity (4 worker sessions).
    walk_hello(
        address,
        &node,
        "cix_B2B2B2B2B2B2B2B2B2B2B2B2B2",
        &credential,
        next,
    )
    .await;
    consume_code_as(
        &data_directory,
        &node,
        &publish_connect_code(&data_directory, &node, "77778888"),
        &owner_id,
    );
    consume_code_as(
        &data_directory,
        &node,
        &publish_connect_code(&data_directory, &node, "88889999"),
        &member_id,
    );

    // Directory shape, field by field.
    let response = http_request(address, &cookie_get("/api/v1/clients", &owner_cookie)).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let value = response_body(&response);
    assert_eq!(value["schemaVersion"], json!(SCHEMA_VERSION));
    let clients = value["clients"].as_array().expect("clients array");
    assert_eq!(clients.len(), 1);
    let card = &clients[0];
    let mut field_names = card
        .as_object()
        .expect("card object")
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    field_names.sort_unstable();
    assert_eq!(
        field_names,
        vec![
            "capacityTotal",
            "capacityUsed",
            "clientId",
            "displayName",
            "lastHeartbeatAt",
            "occupancy",
            "presence",
            "version",
        ]
    );
    assert_eq!(card["clientId"], json!(public_client_id));
    assert_eq!(card["displayName"], json!("Cheng's MacBook"));
    assert_eq!(card["presence"], json!("online"));
    assert_eq!(card["occupancy"], json!("available"));
    assert_eq!(card["capacityUsed"], json!(0));
    assert_eq!(card["capacityTotal"], json!(4));
    assert!(card["lastHeartbeatAt"].is_string());
    assert_eq!(card["version"], json!("0.1.0-alpha.1"));

    // The member sees the same device.
    let response = http_request(address, &cookie_get("/api/v1/clients", &member_cookie)).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert_eq!(
        response_body(&response)["clients"]
            .as_array()
            .expect("clients")
            .len(),
        1
    );

    // A member may not revoke somebody else's grant.
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/grants/revoke",
            &json!({
                "schemaVersion": SCHEMA_VERSION,
                "clientId": public_client_id,
                "userId": owner_id,
            })
            .to_string(),
            &member_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "403", "{response}");
    assert_eq!(wire_code(&response), "PERMISSION_DENIED");

    // The holder revokes their own grant immediately.
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/grants/revoke",
            &json!({
                "schemaVersion": SCHEMA_VERSION,
                "clientId": public_client_id,
            })
            .to_string(),
            &member_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "200", "{response}");
    let value = response_body(&response);
    assert_eq!(value["revoked"], json!(true));
    assert_eq!(value["userId"], json!(member_id));
    {
        let mut storage = SqliteStorage::open(&data_directory).expect("storage");
        let mut grants = AccessGrantService::new(&mut storage);
        assert!(
            grants
                .active_grant(&node, &member_id)
                .expect("grant lookup")
                .is_none(),
            "revocation takes effect immediately"
        );
    }
    let response = http_request(address, &cookie_get("/api/v1/clients", &member_cookie)).await;
    assert_eq!(
        response_body(&response)["clients"]
            .as_array()
            .expect("clients")
            .len(),
        0,
        "the revoked device leaves the directory"
    );

    // Revoking an already-revoked grant is a resource miss at this boundary.
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/grants/revoke",
            &json!({
                "schemaVersion": SCHEMA_VERSION,
                "clientId": public_client_id,
                "userId": member_id,
            })
            .to_string(),
            &member_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "404", "{response}");
    assert_eq!(wire_code(&response), "RESOURCE_NOT_FOUND");

    // The Owner revokes on behalf of anyone; the owner grant is untouched,
    // and a revoked user can reconnect with a fresh code afterwards.
    let reconnect = publish_connect_code(&data_directory, &node, "12121212");
    consume_code_as(&data_directory, &node, &reconnect, &member_id);
    let response = http_request(
        address,
        &cookie_post(
            "/api/v1/clients/grants/revoke",
            &json!({
                "schemaVersion": SCHEMA_VERSION,
                "clientId": public_client_id,
                "userId": member_id,
            })
            .to_string(),
            &owner_cookie,
        ),
    )
    .await;
    assert_eq!(status_of(&response), "200", "{response}");
    {
        let mut storage = SqliteStorage::open(&data_directory).expect("storage");
        let mut grants = AccessGrantService::new(&mut storage);
        assert!(
            grants
                .active_grant(&node, &member_id)
                .expect("grant lookup")
                .is_none()
        );
        assert!(
            grants
                .active_grant(&node, &owner_id)
                .expect("grant lookup")
                .is_some(),
            "the owner grant is untouched"
        );
    }
    let connection =
        Connection::open(data_directory.join("control-plane.sqlite3")).expect("open database");
    let revocations = connection
        .query_row(
            "SELECT COUNT(*) FROM client_connect_audit WHERE action = 'client.access.revoked'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("audit count");
    assert_eq!(revocations, 2, "every revocation is audited");

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
    let _ = std::fs::remove_dir_all(&auth_directory);
}

#[tokio::test]
async fn downlink_outbox_retains_by_acknowledgement_cursor_and_purges() {
    let data_directory = test_directory("client-connect-outbox");
    let mut storage = SqliteStorage::open(&data_directory).expect("storage");
    let node = "cnd_A1A1A1A1A1A1A1A1A1A1A1A1A1";
    {
        let mut registry = ClientRegistryService::new(&mut storage);
        let registration = ClientNodeRegistration::try_new(
            node,
            "927351842",
            "device",
            "aarch64-apple-darwin",
            "aarch64",
            "0.1.0",
            None,
            Some(INSTANCE.to_owned()),
            4,
        )
        .expect("registration");
        let now = Instant("2026-09-04T12:00:00.000Z".to_owned());
        registry
            .register(&registration, 0, &now)
            .expect("registered");
    }
    let now = Instant("2026-09-04T12:00:00.000Z".to_owned());
    let append_frame = |storage: &mut SqliteStorage, sequence: u64| {
        let mut outbox = storage.client_downlink_outbox().expect("outbox");
        outbox
            .append(
                &ClientDownlinkAppend::try_new(
                    node,
                    format!("msg_{sequence:020}"),
                    sequence,
                    json!({"kind": "test", "sequence": sequence}).to_string(),
                )
                .expect("append command"),
                &now,
            )
            .expect("appended")
    };
    append_frame(&mut storage, 1);
    append_frame(&mut storage, 2);
    {
        let mut outbox = storage.client_downlink_outbox().expect("outbox");
        let all = outbox.deliverable(node, 0, 10).expect("deliverable");
        assert_eq!(
            all.iter().map(|stored| stored.sequence).collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(
            all[0].frame,
            json!({"kind": "test", "sequence": 1}).to_string()
        );
        assert_eq!(outbox.high_water(node).expect("high water"), 2);
        // A sequence that is not the next stream position is rejected.
        let stale = outbox.append(
            &ClientDownlinkAppend::try_new(node, format!("msg_{:020}", 9), 9, "{}".to_owned())
                .expect("command"),
            &now,
        );
        assert!(stale.is_err(), "sequence 9 is not the next position");
    }
    {
        // Acknowledging the first frame trims only the acked prefix.
        let mut outbox = storage.client_downlink_outbox().expect("outbox");
        assert_eq!(outbox.retain_through(node, 1).expect("retain"), 1);
        let remaining = outbox.deliverable(node, 0, 10).expect("deliverable");
        assert_eq!(
            remaining
                .iter()
                .map(|stored| stored.sequence)
                .collect::<Vec<_>>(),
            [2]
        );
    }
    {
        // After full acknowledgement the table retains nothing and the next
        // append continues above the acknowledgement cursor: this test never
        // advanced the cursor, so the stream restarts at one above it.
        let mut outbox = storage.client_downlink_outbox().expect("outbox");
        assert_eq!(outbox.retain_through(node, 5).expect("retain"), 1);
        assert_eq!(outbox.high_water(node).expect("high water"), 0);
    }
    let appended = append_frame(&mut storage, 1);
    assert_eq!(appended.sequence, 1);
    let _ = std::fs::remove_dir_all(&data_directory);
}
