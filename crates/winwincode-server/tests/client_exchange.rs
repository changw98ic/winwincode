// SPDX-License-Identifier: Apache-2.0

//! Server-side `POST /internal/v1/client/exchange` integration tests:
//! enrollment with Device Credential issuance, credential enforcement,
//! gap/replay and duplicate settlement, and the hello/heartbeat projections.

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

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
use winwincode_client_port::messages::ClientToServerEnvelope;
use winwincode_client_port::messages::ClientToServerMessage;
use winwincode_client_port::messages::CommandContext;
use winwincode_control_plane::ClientRegistryService;
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

const BOOTSTRAP_PROOF: &str = "client-exchange-test-bootstrap";
/// Placeholder client node id a fresh device sends before the server assigns
/// the canonical `cnd_` identity.
const FRESH_NODE: &str = "device-local-pending";
/// Canonical 26 character Crockford suffix (`cix_` + 26).
const INSTANCE: &str = "cix_A1A1A1A1A1A1A1A1A1A1A1A1A1";
const RESTARTED_INSTANCE: &str = "cix_B2B2B2B2B2B2B2B2B2B2B2B2B2";
const SCHEMA_VERSION: &str = "winwincode/v1";

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
            "unused in client exchange tests",
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
            "unused in client exchange tests",
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
        UserAccountService::open(test_directory("client-exchange-auth")).expect("account service"),
    );
    Arc::new(
        SqliteAuthSessionManager::open(
            test_directory("client-exchange-auth"),
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

async fn post_exchange(
    address: std::net::SocketAddr,
    body: &str,
    credential: Option<&str>,
) -> (String, Option<serde_json::Value>) {
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

fn frame(
    node: &str,
    instance: &str,
    sequence: u64,
    message: ClientToServerMessage,
) -> serde_json::Value {
    serde_json::to_value(ClientToServerEnvelope {
        schema_version: SCHEMA_VERSION.to_owned(),
        message_id: format!("msg_{sequence:020}"),
        client_node_id: node.to_owned(),
        client_instance_id: instance.to_owned(),
        sequence,
        occurred_at: "2026-01-02T12:00:00.000Z".to_owned(),
        message,
    })
    .expect("frame value")
}

fn enroll_frame(sequence: u64) -> serde_json::Value {
    frame(
        FRESH_NODE,
        INSTANCE,
        sequence,
        ClientToServerMessage::Enroll(Box::new(ClientEnrollPayload {
            command: CommandContext {
                expected_revision: 0,
                idempotency_key: "idem_enroll_1".to_owned(),
            },
            display_name: "Cheng's MacBook".to_owned(),
            platform: ClientPlatformTarget::Aarch64AppleDarwin,
            architecture: ClientArchitecture::Aarch64,
            client_version: "0.1.0-alpha.1".to_owned(),
        })),
    )
}

fn capacity(max: u32, running: u32) -> ClientCapacityReport {
    ClientCapacityReport {
        max_concurrent_worker_sessions: max,
        running_worker_sessions: running,
        reserved_worker_sessions: 0,
        draining_worker_sessions: 0,
    }
}

fn hello_frame(node: &str, instance: &str, sequence: u64) -> serde_json::Value {
    frame(
        node,
        instance,
        sequence,
        ClientToServerMessage::Hello(ClientHelloPayload {
            client_version: "0.1.0-alpha.1".to_owned(),
            capacity: capacity(4, 0),
            accepting_connections: true,
            lock_state: ClientLockState::Unlocked,
            presence_state: PresenceState::Online,
        }),
    )
}

fn heartbeat_frame(node: &str, instance: &str, sequence: u64, running: u32) -> serde_json::Value {
    frame(
        node,
        instance,
        sequence,
        ClientToServerMessage::Heartbeat(ClientHeartbeatPayload {
            capacity: capacity(4, running),
            accepting_connections: true,
            lock_state: ClientLockState::Unlocked,
            presence_state: PresenceState::Online,
            occupancy_lease_id: None,
        }),
    )
}

fn exchange_request(frames: &[serde_json::Value], ack_sequence: u64) -> String {
    serde_json::json!({
        "schemaVersion": SCHEMA_VERSION,
        "frames": frames,
        "ackSequence": ack_sequence,
    })
    .to_string()
}

fn node_snapshot(data_directory: &Path, node_id: &str) -> Option<ClientNodeRecord> {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut registry = ClientRegistryService::new(&mut storage);
    registry.snapshot(node_id).expect("node snapshot")
}

fn cursors(data_directory: &Path, node_id: &str) -> ClientExchangeCursors {
    let mut storage = SqliteStorage::open(data_directory).expect("storage");
    let mut registry = ClientRegistryService::new(&mut storage);
    registry
        .exchange_cursors(node_id)
        .expect("cursors")
        .expect("cursor row")
}

/// Enrolls a fresh node and returns the address, the assigned node id, and
/// the issued credential material.
async fn enroll_device(address: std::net::SocketAddr) -> (String, String) {
    let (status, body) =
        post_exchange(address, &exchange_request(&[enroll_frame(1)], 0), None).await;
    assert!(status.starts_with("HTTP/1.1 200 OK"), "{status}");
    let body = body.expect("enrollment response body");
    let enrollment = body.get("enrollment").expect("enrollment issuance").clone();
    let node_id = enrollment
        .get("clientNodeId")
        .and_then(serde_json::Value::as_str)
        .expect("assigned clientNodeId")
        .to_owned();
    let credential = enrollment
        .get("deviceCredential")
        .and_then(serde_json::Value::as_str)
        .expect("issued credential")
        .to_owned();
    (node_id, credential)
}

#[tokio::test]
async fn enroll_creates_a_pending_node_and_the_authenticated_hello_continues() {
    let data_directory = test_directory("client-exchange-enroll");
    let running = start_with_client_exchange(&data_directory).await;
    let address = running.local_address();

    let (status, body) =
        post_exchange(address, &exchange_request(&[enroll_frame(1)], 0), None).await;
    assert!(status.starts_with("HTTP/1.1 200 OK"), "{status}");
    let body = body.expect("enrollment response body");
    assert_eq!(body.get("ackSequence"), Some(&serde_json::json!(1)));
    assert!(body.get("replayFromSequence").is_none());
    let enrollment = body.get("enrollment").expect("enrollment issuance");
    let node_id = enrollment
        .get("clientNodeId")
        .and_then(serde_json::Value::as_str)
        .expect("assigned clientNodeId");
    assert!(
        node_id.starts_with("cnd_") && node_id.len() == 30,
        "{node_id}"
    );
    let public_client_id = enrollment
        .get("publicClientId")
        .and_then(serde_json::Value::as_str)
        .expect("public client id");
    assert_eq!(public_client_id.len(), 10);
    assert!(public_client_id.bytes().all(|byte| byte.is_ascii_digit()));
    let credential = enrollment
        .get("deviceCredential")
        .and_then(serde_json::Value::as_str)
        .expect("credential material");
    assert_eq!(credential.len(), 64);
    let digest = enrollment
        .get("deviceCredentialDigest")
        .and_then(serde_json::Value::as_str)
        .expect("credential digest");
    let mut secret_bytes = [0_u8; 32];
    for (index, slot) in secret_bytes.iter_mut().enumerate() {
        let high = (credential.as_bytes()[index * 2] as char)
            .to_digit(16)
            .expect("high nibble");
        let low = (credential.as_bytes()[index * 2 + 1] as char)
            .to_digit(16)
            .expect("low nibble");
        *slot = u8::try_from(high << 4 | low).expect("byte");
    }
    assert_eq!(
        digest,
        &format!("sha256:{:x}", Sha256::digest(secret_bytes)),
        "the digest persists exactly the sha256 of the issued material"
    );
    assert_eq!(
        enrollment.get("downlinkFromSequence"),
        Some(&serde_json::json!(1))
    );
    assert!(enrollment.get("heartbeatIntervalMs").is_some());
    assert_eq!(
        body.get("frames")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len),
        Some(1)
    );
    assert_eq!(
        body["frames"][0].get("kind"),
        Some(&serde_json::json!("client.enrollment_accepted"))
    );

    let record = node_snapshot(&data_directory, node_id).expect("enrolled node");
    assert_eq!(
        record.presence_state,
        ClientPresenceState::PendingEnrollment
    );
    assert_eq!(record.device_credential_digest.as_deref(), Some(digest));
    assert_eq!(record.current_instance_id.as_deref(), Some(INSTANCE));

    // The authenticated hello takes presence online on the same stream.
    let (status, body) = post_exchange(
        address,
        &exchange_request(&[hello_frame(node_id, INSTANCE, 2)], 1),
        Some(credential),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200 OK"), "{status}");
    let body = body.expect("hello response body");
    assert_eq!(body.get("ackSequence"), Some(&serde_json::json!(2)));
    assert!(body.get("enrollment").is_none());
    assert_eq!(
        body.get("frames")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len),
        Some(0),
        "the skeleton downlink delivers an empty batch"
    );

    let record = node_snapshot(&data_directory, node_id).expect("hello node");
    assert_eq!(record.presence_state, ClientPresenceState::Online);
    assert_eq!(record.current_instance_id.as_deref(), Some(INSTANCE));
    assert_eq!(
        cursors(&data_directory, node_id).client_to_server_ack_sequence,
        2
    );

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
}

#[tokio::test]
async fn missing_malformed_wrong_and_unknown_credentials_are_one_uniform_rejection() {
    let data_directory = test_directory("client-exchange-auth");
    let running = start_with_client_exchange(&data_directory).await;
    let address = running.local_address();
    let (node_id, credential) = enroll_device(address).await;

    let wrong_hex: String = "ab".repeat(32);
    let attempts = vec![
        // A non-enroll exchange without any credential.
        (
            exchange_request(&[heartbeat_frame(&node_id, INSTANCE, 2, 0)], 1),
            None,
        ),
        // A malformed credential.
        (
            exchange_request(&[heartbeat_frame(&node_id, INSTANCE, 2, 0)], 1),
            Some("not-hex"),
        ),
        // A well-formed but wrong credential.
        (
            exchange_request(&[heartbeat_frame(&node_id, INSTANCE, 2, 0)], 1),
            Some(wrong_hex.as_str()),
        ),
        // A plausible credential against a node that does not exist.
        (
            exchange_request(
                &[heartbeat_frame(
                    "cnd_CCCCCCCCCCCCCCCCCCCCCCCCCC",
                    INSTANCE,
                    1,
                    0,
                )],
                0,
            ),
            Some(credential.as_str()),
        ),
    ];
    let mut responses = Vec::new();
    for (body, credential) in attempts {
        let (status, parsed) = post_exchange(address, &body, credential).await;
        assert!(status.starts_with("HTTP/1.1 401 Unauthorized"), "{status}");
        assert!(parsed.is_none(), "rejections carry no detail: {status}");
        responses.push(status);
    }
    assert!(
        responses.windows(2).all(|pair| pair[0] == pair[1]),
        "every credential failure is byte-identical and discloses nothing"
    );

    // The real credential still authenticates: the failures changed nothing.
    let (status, body) = post_exchange(
        address,
        &exchange_request(&[hello_frame(&node_id, INSTANCE, 2)], 1),
        Some(&credential),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200 OK"), "{status}");
    assert_eq!(
        body.expect("hello body").get("ackSequence"),
        Some(&serde_json::json!(2))
    );

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
}

#[tokio::test]
async fn a_gap_answers_replay_from_sequence_and_keeps_the_cursor() {
    let data_directory = test_directory("client-exchange-gap");
    let running = start_with_client_exchange(&data_directory).await;
    let address = running.local_address();
    let (node_id, credential) = enroll_device(address).await;

    let (status, _) = post_exchange(
        address,
        &exchange_request(&[hello_frame(&node_id, INSTANCE, 2)], 1),
        Some(&credential),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200 OK"), "{status}");
    assert_eq!(
        cursors(&data_directory, &node_id).client_to_server_ack_sequence,
        2
    );

    // Sequence 4 arrives while the cursor sits at 2: gap with a replay hint.
    let (status, body) = post_exchange(
        address,
        &exchange_request(&[heartbeat_frame(&node_id, INSTANCE, 4, 1)], 1),
        Some(&credential),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200 OK"), "{status}");
    let body = body.expect("gap response body");
    assert_eq!(body.get("ackSequence"), Some(&serde_json::json!(2)));
    assert_eq!(body.get("replayFromSequence"), Some(&serde_json::json!(3)));
    assert_eq!(
        cursors(&data_directory, &node_id).client_to_server_ack_sequence,
        2
    );
    let record = node_snapshot(&data_directory, &node_id).expect("node");
    assert_eq!(
        record.last_heartbeat_at, None,
        "a gapped frame never executes its effect"
    );

    // The replay closes the gap and both frames settle.
    let (status, body) = post_exchange(
        address,
        &exchange_request(
            &[
                heartbeat_frame(&node_id, INSTANCE, 3, 1),
                heartbeat_frame(&node_id, INSTANCE, 4, 1),
            ],
            1,
        ),
        Some(&credential),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200 OK"), "{status}");
    let body = body.expect("replay response body");
    assert_eq!(body.get("ackSequence"), Some(&serde_json::json!(4)));
    assert!(body.get("replayFromSequence").is_none());
    assert_eq!(
        cursors(&data_directory, &node_id).client_to_server_ack_sequence,
        4
    );
    let record = node_snapshot(&data_directory, &node_id).expect("node");
    assert!(record.last_heartbeat_at.is_some());

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
}

#[tokio::test]
async fn duplicate_frames_confirm_without_reexecution() {
    let data_directory = test_directory("client-exchange-duplicate");
    let running = start_with_client_exchange(&data_directory).await;
    let address = running.local_address();
    let (node_id, credential) = enroll_device(address).await;

    let (status, _) = post_exchange(
        address,
        &exchange_request(&[hello_frame(&node_id, INSTANCE, 2)], 1),
        Some(&credential),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200 OK"), "{status}");
    let first = node_snapshot(&data_directory, &node_id).expect("node");
    assert_eq!(first.presence_state, ClientPresenceState::Online);

    // The exact same frame replays: it is confirmed, the cursor stays, and
    // the presence projection does not execute a second time.
    let (status, body) = post_exchange(
        address,
        &exchange_request(&[hello_frame(&node_id, INSTANCE, 2)], 1),
        Some(&credential),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200 OK"), "{status}");
    let body = body.expect("duplicate response body");
    assert_eq!(body.get("ackSequence"), Some(&serde_json::json!(2)));
    assert!(body.get("replayFromSequence").is_none());
    let second = node_snapshot(&data_directory, &node_id).expect("node");
    assert_eq!(second.revision, first.revision, "no second effect");
    assert_eq!(second.presence_state, ClientPresenceState::Online);
    assert_eq!(
        cursors(&data_directory, &node_id).client_to_server_ack_sequence,
        2
    );

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
}

#[tokio::test]
async fn heartbeat_updates_the_capacity_and_heartbeat_projection() {
    let data_directory = test_directory("client-exchange-heartbeat");
    let running = start_with_client_exchange(&data_directory).await;
    let address = running.local_address();
    let (node_id, credential) = enroll_device(address).await;

    let (status, _) = post_exchange(
        address,
        &exchange_request(&[hello_frame(&node_id, INSTANCE, 2)], 1),
        Some(&credential),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200 OK"), "{status}");

    let (status, body) = post_exchange(
        address,
        &exchange_request(&[heartbeat_frame(&node_id, INSTANCE, 3, 2)], 1),
        Some(&credential),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200 OK"), "{status}");
    assert_eq!(
        body.expect("heartbeat body").get("ackSequence"),
        Some(&serde_json::json!(3))
    );
    let record = node_snapshot(&data_directory, &node_id).expect("node");
    assert!(
        record.last_heartbeat_at.is_some(),
        "heartbeat instant recorded"
    );
    assert_eq!(record.reported_running_worker_sessions, 2);
    assert_eq!(record.presence_state, ClientPresenceState::Online);

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
}

#[tokio::test]
async fn a_pending_enrollment_heartbeat_is_refused() {
    let data_directory = test_directory("client-exchange-pending-heartbeat");
    let running = start_with_client_exchange(&data_directory).await;
    let address = running.local_address();
    let (node_id, credential) = enroll_device(address).await;

    // The frame settles on the stream, but the heartbeat projection refuses
    // the `pending_enrollment` node: no heartbeat instant, no presence move.
    let (status, body) = post_exchange(
        address,
        &exchange_request(&[heartbeat_frame(&node_id, INSTANCE, 2, 3)], 1),
        Some(&credential),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200 OK"), "{status}");
    assert_eq!(
        body.expect("body").get("ackSequence"),
        Some(&serde_json::json!(2))
    );
    let record = node_snapshot(&data_directory, &node_id).expect("node");
    assert_eq!(
        record.presence_state,
        ClientPresenceState::PendingEnrollment
    );
    assert_eq!(record.last_heartbeat_at, None);
    assert_eq!(record.reported_running_worker_sessions, 0);

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
}

#[tokio::test]
async fn a_second_enroll_after_enrollment_is_refused() {
    let data_directory = test_directory("client-exchange-re-enroll");
    let running = start_with_client_exchange(&data_directory).await;
    let address = running.local_address();
    let (node_id, credential) = enroll_device(address).await;
    let before = node_snapshot(&data_directory, &node_id).expect("node");

    // Unauthenticated re-enroll of an enrolled node: one uniform rejection,
    // no second credential, no state change. The replay targets the assigned
    // node id; a placeholder id would only ever create a fresh node.
    let (status, parsed) = post_exchange(
        address,
        &exchange_request(
            &[frame(
                &node_id,
                INSTANCE,
                1,
                ClientToServerMessage::Enroll(Box::new(ClientEnrollPayload {
                    command: CommandContext {
                        expected_revision: 0,
                        idempotency_key: "idem_enroll_retry".to_owned(),
                    },
                    display_name: "Cheng's MacBook".to_owned(),
                    platform: ClientPlatformTarget::Aarch64AppleDarwin,
                    architecture: ClientArchitecture::Aarch64,
                    client_version: "0.1.0-alpha.1".to_owned(),
                })),
            )],
            0,
        ),
        None,
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 401 Unauthorized"), "{status}");
    assert!(parsed.is_none());
    let after = node_snapshot(&data_directory, &node_id).expect("node");
    assert_eq!(after.revision, before.revision);
    assert_eq!(
        after.device_credential_digest, before.device_credential_digest,
        "no credential re-issue"
    );

    // The original credential still authenticates, and an enroll frame
    // inside an authenticated batch is refused at the conflict outcome.
    let (status, _) = post_exchange(
        address,
        &exchange_request(&[hello_frame(&node_id, INSTANCE, 2)], 1),
        Some(&credential),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200 OK"), "{status}");

    let (status, body) = post_exchange(
        address,
        &exchange_request(
            &[frame(
                &node_id,
                INSTANCE,
                3,
                ClientToServerMessage::Enroll(Box::new(ClientEnrollPayload {
                    command: CommandContext {
                        expected_revision: 0,
                        idempotency_key: "idem_enroll_authenticated".to_owned(),
                    },
                    display_name: "Cheng's MacBook".to_owned(),
                    platform: ClientPlatformTarget::Aarch64AppleDarwin,
                    architecture: ClientArchitecture::Aarch64,
                    client_version: "0.1.0-alpha.1".to_owned(),
                })),
            )],
            1,
        ),
        Some(&credential),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200 OK"), "{status}");
    let body = body.expect("refused enroll response");
    assert_eq!(body.get("ackSequence"), Some(&serde_json::json!(2)));
    assert!(body.get("enrollment").is_none(), "no credential re-issue");
    assert_eq!(
        cursors(&data_directory, &node_id).client_to_server_ack_sequence,
        2
    );
    let refused = node_snapshot(&data_directory, &node_id).expect("node");
    assert_eq!(refused.revision, before.revision + 1, "only hello advanced");
    assert_eq!(refused.presence_state, ClientPresenceState::Online);

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
}

#[tokio::test]
async fn a_restarted_instance_supersedes_the_old_one_via_hello() {
    let data_directory = test_directory("client-exchange-instance");
    let running = start_with_client_exchange(&data_directory).await;
    let address = running.local_address();
    let (node_id, credential) = enroll_device(address).await;

    let (status, _) = post_exchange(
        address,
        &exchange_request(&[hello_frame(&node_id, INSTANCE, 2)], 1),
        Some(&credential),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200 OK"), "{status}");

    // The restarted device announces a new instance: the old one is
    // superseded and the stream continues under the new instance.
    let (status, body) = post_exchange(
        address,
        &exchange_request(&[hello_frame(&node_id, RESTARTED_INSTANCE, 3)], 1),
        Some(&credential),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200 OK"), "{status}");
    assert_eq!(
        body.expect("takeover body").get("ackSequence"),
        Some(&serde_json::json!(3))
    );
    let record = node_snapshot(&data_directory, &node_id).expect("node");
    assert_eq!(
        record.current_instance_id.as_deref(),
        Some(RESTARTED_INSTANCE)
    );
    assert_eq!(record.presence_state, ClientPresenceState::Online);

    // A frame still claiming the superseded instance is refused and the
    // cursor does not move.
    let (status, body) = post_exchange(
        address,
        &exchange_request(&[heartbeat_frame(&node_id, INSTANCE, 4, 0)], 1),
        Some(&credential),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200 OK"), "{status}");
    assert_eq!(
        body.expect("superseded body").get("ackSequence"),
        Some(&serde_json::json!(3))
    );
    assert_eq!(
        cursors(&data_directory, &node_id).client_to_server_ack_sequence,
        3
    );

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
}

#[tokio::test]
async fn malformed_frames_and_unknown_routes_fail_closed() {
    let data_directory = test_directory("client-exchange-malformed");
    let running = start_with_client_exchange(&data_directory).await;
    let address = running.local_address();

    // Version mismatch rejects the whole batch.
    let bad_version = serde_json::json!({
        "schemaVersion": "winwincode/v0",
        "frames": [],
        "ackSequence": 0
    })
    .to_string();
    let (status, parsed) = post_exchange(address, &bad_version, None).await;
    assert!(status.starts_with("HTTP/1.1 400 Bad Request"), "{status}");
    assert!(parsed.is_none());

    // A frame larger than the codec bound rejects the batch.
    let oversized = serde_json::to_value(ClientToServerEnvelope {
        schema_version: SCHEMA_VERSION.to_owned(),
        message_id: "msg_oversized".to_owned(),
        client_node_id: FRESH_NODE.to_owned(),
        client_instance_id: INSTANCE.to_owned(),
        sequence: 1,
        occurred_at: "2026-01-02T12:00:00.000Z".to_owned(),
        message: ClientToServerMessage::Enroll(Box::new(ClientEnrollPayload {
            command: CommandContext {
                expected_revision: 0,
                idempotency_key: "idem_big".to_owned(),
            },
            display_name: "x".repeat(300_000),
            platform: ClientPlatformTarget::Aarch64AppleDarwin,
            architecture: ClientArchitecture::Aarch64,
            client_version: "0.1.0-alpha.1".to_owned(),
        })),
    })
    .expect("oversized frame value");
    let (status, parsed) = post_exchange(address, &exchange_request(&[oversized], 0), None).await;
    assert!(status.starts_with("HTTP/1.1 400 Bad Request"), "{status}");
    assert!(parsed.is_none());

    // Without the exchange application attached the route stays closed.
    let sessions = auth_sessions();
    let authenticator: Arc<dyn RequestAuthenticator> = sessions.clone();
    let bare = start_server_with_remote_worker(
        server_config(&test_directory("client-exchange-bare")),
        sessions,
        authenticator,
        Arc::new(NoopApi),
        None,
        None,
        None,
    )
    .await
    .expect("bare server");
    let (status, _) = post_exchange(
        bare.local_address(),
        &exchange_request(&[enroll_frame(1)], 0),
        None,
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 404 Not Found"), "{status}");
    bare.shutdown().await.expect("shutdown bare");

    running.shutdown().await.expect("shutdown");
    let _ = std::fs::remove_dir_all(&data_directory);
}
