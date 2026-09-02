// SPDX-License-Identifier: Apache-2.0

use std::fmt::Write as _;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use hmac::{Hmac, Mac};
use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use serde_json::{Value, json};
use sha2::Sha256;
use winwincode_audit::AuditScope;
use winwincode_domain::{
    CredentialReferenceId, OrganizationId, ProjectId, RepositoryId, WorkspaceId,
};
use winwincode_integration::{
    ConnectorCallError, ConnectorCallErrorKind, ConnectorPort, ConnectorProtocol,
    ConnectorRegistration, ConnectorState, EnterpriseIntegrationId, InboundStatus,
    IntegrationErrorKind, IntegrationFramework, IntegrationLeaseId, IntegrationOperationKey,
    IntegrationStorage, LINEAR_CONNECTOR_PROTOCOL, LinearClock, LinearConnectorConfig,
    LinearConnectorScope, LinearCredentialError, LinearCredentialPort, LinearEnterpriseConnector,
    LinearEventAction, LinearEventKind, LinearEventMapperPort, LinearInboundEvent,
    LinearOAuthScope, LinearOAuthToken, LinearProjectId, LinearTeamId, LinearTlsRoots,
    LinearWebhookHeaders, LinearWebhookRequestFactory, LinearWebhookSecret, LinearWebhookVerifier,
    LinearWorkspaceId, NormalizedInboundEvent, OutboundAttemptResult, OutboundOperationState,
    OutboundRequest, RetryPolicy,
};

const WEBHOOK_SECRET: &[u8] = b"linear-webhook-secret-fixture";
const OAUTH_TOKEN: &str = "linear-oauth-access-token-fixture";
const WORKSPACE_UUID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const TEAM_UUID: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const PROJECT_UUID: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
const ISSUE_UUID: &str = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
const COMMENT_UUID: &str = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
const STATE_UUID: &str = "ffffffff-ffff-4fff-8fff-ffffffffffff";
const WEBHOOK_UUID: &str = "11111111-1111-4111-8111-111111111111";
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-linear-connector-{name}-{}-{sequence}",
        std::process::id()
    ))
}

fn id(prefix: &str, tail: char) -> String {
    format!("{prefix}_{}", tail.to_string().repeat(26))
}

fn integration_id() -> EnterpriseIntegrationId {
    EnterpriseIntegrationId(id("int", '1'))
}

fn credential_reference_id() -> CredentialReferenceId {
    CredentialReferenceId(id("crd", '2'))
}

fn tenant_scope() -> AuditScope {
    AuditScope::repository(
        OrganizationId(id("org", '3')),
        WorkspaceId(id("wsp", '4')),
        ProjectId(id("prj", '5')),
        RepositoryId(id("rep", '6')),
    )
    .expect("tenant scope")
}

fn linear_scope() -> LinearConnectorScope {
    LinearConnectorScope::new(
        LinearWorkspaceId::try_new(WORKSPACE_UUID).expect("workspace"),
        LinearTeamId::try_new(TEAM_UUID).expect("team"),
        Some(LinearProjectId::try_new(PROJECT_UUID).expect("project")),
    )
}

fn base_config(endpoint: String, roots: LinearTlsRoots) -> LinearConnectorConfig {
    LinearConnectorConfig::try_new(
        integration_id(),
        credential_reference_id(),
        linear_scope(),
        endpoint,
        roots,
    )
    .expect("Linear connector config")
}

fn register(framework: &mut IntegrationFramework, config: &LinearConnectorConfig) {
    framework
        .register_connector(
            &ConnectorRegistration::try_new(
                config.integration_id().clone(),
                tenant_scope(),
                ConnectorProtocol::try_new(LINEAR_CONNECTOR_PROTOCOL).expect("protocol"),
                config.credential_reference_id().clone(),
                10,
            )
            .expect("registration"),
        )
        .expect("register Linear connector");
}

#[derive(Clone, Copy)]
struct FixedClock(u64);

impl LinearClock for FixedClock {
    fn now_millis(&self) -> u64 {
        self.0
    }
}

#[derive(Clone)]
struct CredentialFixture {
    scopes: Vec<LinearOAuthScope>,
    webhook_lookups: Arc<AtomicU64>,
    token_lookups: Arc<AtomicU64>,
}

impl CredentialFixture {
    fn full() -> Self {
        Self {
            scopes: vec![LinearOAuthScope::Read, LinearOAuthScope::Write],
            webhook_lookups: Arc::new(AtomicU64::new(0)),
            token_lookups: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl LinearCredentialPort for CredentialFixture {
    fn resolve_webhook_secret(
        &mut self,
        reference: &CredentialReferenceId,
    ) -> Result<LinearWebhookSecret, LinearCredentialError> {
        assert_eq!(reference, &credential_reference_id());
        self.webhook_lookups.fetch_add(1, Ordering::Relaxed);
        Ok(LinearWebhookSecret::try_new(WEBHOOK_SECRET).expect("webhook secret"))
    }

    fn resolve_oauth_token(
        &mut self,
        reference: &CredentialReferenceId,
        workspace_id: &LinearWorkspaceId,
    ) -> Result<LinearOAuthToken, LinearCredentialError> {
        assert_eq!(reference, &credential_reference_id());
        assert_eq!(workspace_id.as_str(), WORKSPACE_UUID);
        self.token_lookups.fetch_add(1, Ordering::Relaxed);
        Ok(LinearOAuthToken::try_new(
            OAUTH_TOKEN,
            LinearWorkspaceId::try_new(WORKSPACE_UUID).expect("workspace"),
            self.scopes.iter().copied(),
            100_000,
        )
        .expect("OAuth token"))
    }
}

#[derive(Clone, Default)]
struct MapperFixture(Arc<Mutex<Vec<(LinearEventKind, LinearEventAction, String)>>>);

impl LinearEventMapperPort for MapperFixture {
    fn map_event(
        &mut self,
        _authority: &winwincode_integration::ConnectorAuthority,
        event: &LinearInboundEvent<'_>,
    ) -> Result<NormalizedInboundEvent, ConnectorCallError> {
        self.0.lock().expect("mapper calls").push((
            event.kind(),
            event.action(),
            event.resource_id().to_owned(),
        ));
        NormalizedInboundEvent::try_new(
            "delivery.update",
            br#"{"command":"delivery.update"}"#.to_vec(),
        )
        .map_err(|_| {
            ConnectorCallError::try_new(ConnectorCallErrorKind::Permanent, "LINEAR_MAPPING_INVALID")
                .expect("mapping error")
        })
    }
}

fn webhook_payload(event_type: &str, action: &str, resource_id: &str, timestamp: u64) -> Vec<u8> {
    let mut data = json!({
        "id": resource_id,
        "projectId": PROJECT_UUID,
        "teamId": TEAM_UUID,
    });
    if event_type == "Comment" {
        data["issueId"] = json!(ISSUE_UUID);
    }
    serde_json::to_vec(&json!({
        "action": action,
        "data": data,
        "organizationId": WORKSPACE_UUID,
        "type": event_type,
        "webhookTimestamp": timestamp,
    }))
    .expect("Linear webhook JSON")
}

fn webhook_signature(payload: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(WEBHOOK_SECRET).expect("HMAC key");
    mac.update(payload);
    let mut signature = String::new();
    for byte in mac.finalize().into_bytes() {
        write!(signature, "{byte:02x}").expect("write signature");
    }
    signature.into_bytes()
}

fn build_webhook(
    factory: &LinearWebhookRequestFactory,
    event_type: &str,
    payload: Vec<u8>,
    timestamp: u64,
) -> winwincode_integration::InboundWebhookRequest {
    let signature = webhook_signature(&payload);
    factory
        .build(
            tenant_scope(),
            LinearWebhookHeaders::try_new(WEBHOOK_UUID, event_type, signature, timestamp)
                .expect("headers"),
            payload,
            timestamp + 1,
        )
        .expect("request")
}

#[test]
fn signed_webhooks_are_closed_scoped_replay_safe_and_secret_free() {
    let root = temporary_directory("webhooks");
    let config = base_config(
        "https://api.linear.app/graphql".to_owned(),
        LinearTlsRoots::WebPki,
    );
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    register(&mut framework, &config);
    let factory = LinearWebhookRequestFactory::new(config.clone());
    let payload = webhook_payload("Issue", "update", ISSUE_UUID, 1_000);
    let request = build_webhook(&factory, "Issue", payload, 1_000);
    let credentials = CredentialFixture::full();
    let webhook_lookups = Arc::clone(&credentials.webhook_lookups);
    let calls = MapperFixture::default();
    let captured = Arc::clone(&calls.0);
    let mut verifier =
        LinearWebhookVerifier::new(config.clone(), credentials.clone(), FixedClock(1_000));
    let mut connector =
        LinearEnterpriseConnector::try_new(config.clone(), credentials, calls, FixedClock(1_000))
            .expect("connector");

    let first = framework
        .receive_webhook(&request, &mut verifier, &mut connector)
        .expect("accepted webhook");
    assert_eq!(first.status(), InboundStatus::Accepted);
    assert!(!first.idempotent_replay());
    let replay = framework
        .receive_webhook(&request, &mut verifier, &mut connector)
        .expect("exact replay");
    assert!(replay.idempotent_replay());
    assert_eq!(captured.lock().expect("calls").len(), 2);
    assert_eq!(webhook_lookups.load(Ordering::Relaxed), 2);
    let audits = framework
        .storage()
        .audit_facts(&tenant_scope(), config.integration_id(), 0, 20)
        .expect("audit facts");
    assert_eq!(audits.len(), 2);
    let database = fs::read(framework.storage().database_path()).expect("database bytes");
    assert!(find_bytes(&database, WEBHOOK_SECRET).is_none());
    assert!(find_bytes(&database, OAUTH_TOKEN.as_bytes()).is_none());
    drop(framework);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn webhook_timestamp_header_scope_and_revocation_fail_closed() {
    let root = temporary_directory("webhook-rejections");
    let config = base_config(
        "https://api.linear.app/graphql".to_owned(),
        LinearTlsRoots::WebPki,
    );
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    register(&mut framework, &config);
    let factory = LinearWebhookRequestFactory::new(config.clone());
    let payload = webhook_payload("Comment", "create", COMMENT_UUID, 1_000);
    assert!(
        factory
            .build(
                tenant_scope(),
                LinearWebhookHeaders::try_new(
                    WEBHOOK_UUID,
                    "Comment",
                    webhook_signature(&payload),
                    999,
                )
                .expect("headers"),
                payload.clone(),
                1_001,
            )
            .is_err()
    );
    let mut foreign: Value = serde_json::from_slice(&payload).expect("payload");
    foreign["organizationId"] = json!("99999999-9999-4999-8999-999999999999");
    let foreign = serde_json::to_vec(&foreign).expect("foreign payload");
    assert_eq!(
        factory
            .build(
                tenant_scope(),
                LinearWebhookHeaders::try_new(
                    WEBHOOK_UUID,
                    "Comment",
                    webhook_signature(&foreign),
                    1_000,
                )
                .expect("headers"),
                foreign,
                1_001,
            )
            .expect_err("foreign scope")
            .kind(),
        IntegrationErrorKind::Invalid
    );
    let request = build_webhook(&factory, "Comment", payload, 1_000);
    let credentials = CredentialFixture::full();
    let mut stale =
        LinearWebhookVerifier::new(config.clone(), credentials.clone(), FixedClock(61_001));
    let mut connector = LinearEnterpriseConnector::try_new(
        config.clone(),
        credentials.clone(),
        MapperFixture::default(),
        FixedClock(61_001),
    )
    .expect("connector");
    assert_eq!(
        framework
            .receive_webhook(&request, &mut stale, &mut connector)
            .expect_err("stale webhook")
            .kind(),
        IntegrationErrorKind::SignatureRejected
    );

    let revoked_payload = serde_json::to_vec(&json!({
        "action": "revoked",
        "oauthClientId": "linear-oauth-client-fixture",
        "organizationId": WORKSPACE_UUID,
        "type": "OAuthApp",
        "webhookTimestamp": 2_000,
    }))
    .expect("revocation JSON");
    let revoked = build_webhook(&factory, "OAuthApp", revoked_payload, 2_000);
    let mut verifier = LinearWebhookVerifier::new(config.clone(), credentials, FixedClock(2_000));
    assert_eq!(
        framework
            .receive_webhook(&revoked, &mut verifier, &mut connector)
            .expect_err("revocation webhook")
            .kind(),
        IntegrationErrorKind::CredentialRevoked
    );
    assert_eq!(
        framework
            .storage()
            .authority(&tenant_scope(), config.integration_id())
            .expect("revoked authority")
            .state(),
        ConnectorState::CredentialRevoked
    );
    drop(framework);
    fs::remove_dir_all(root).expect("cleanup");
}

#[derive(Clone)]
struct HttpReply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpReply {
    fn json(status: u16, body: &Value) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body: serde_json::to_vec(body).expect("reply JSON"),
        }
    }
}

struct TlsLinearFixture {
    endpoint: String,
    certificate_der: Vec<u8>,
    requests: mpsc::Receiver<Vec<u8>>,
    request_count: usize,
    server: thread::JoinHandle<()>,
}

impl TlsLinearFixture {
    fn start(replies: Vec<HttpReply>) -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".to_owned()])
                .expect("generate Linear TLS certificate");
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], key)
            .expect("Linear TLS server config");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind Linear TLS fixture");
        let address = listener.local_addr().expect("Linear TLS address");
        let request_count = replies.len();
        let (sender, requests) = mpsc::channel();
        let server = thread::spawn(move || {
            for reply in replies {
                let (socket, _) = listener.accept().expect("accept Linear TLS request");
                let connection =
                    ServerConnection::new(Arc::new(config.clone())).expect("TLS connection");
                let mut stream = StreamOwned::new(connection, socket);
                sender
                    .send(read_http_request(&mut stream))
                    .expect("record Linear request");
                write_http_reply(&mut stream, &reply);
            }
        });
        Self {
            endpoint: format!("https://localhost:{}/graphql", address.port()),
            certificate_der: cert.der().to_vec(),
            requests,
            request_count,
            server,
        }
    }

    fn finish(self) -> Vec<Vec<u8>> {
        self.server.join().expect("join Linear TLS fixture");
        (0..self.request_count)
            .map(|_| self.requests.recv().expect("captured Linear request"))
            .collect()
    }
}

fn read_http_request(stream: &mut StreamOwned<ServerConnection, TcpStream>) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = stream.read(&mut buffer).expect("read Linear request");
        assert_ne!(count, 0, "Linear request closed before body");
        request.extend_from_slice(&buffer[..count]);
        let Some(header_end) = find_bytes(&request, b"\r\n\r\n") else {
            continue;
        };
        let length = http_content_length(&request[..header_end]);
        if request.len() >= header_end + 4 + length {
            return request;
        }
    }
}

fn http_content_length(headers: &[u8]) -> usize {
    String::from_utf8_lossy(headers)
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0)
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn http_request_body(request: &[u8]) -> &[u8] {
    let header_end = find_bytes(request, b"\r\n\r\n").expect("HTTP header terminator");
    &request[header_end + 4..]
}

fn write_http_reply(stream: &mut StreamOwned<ServerConnection, TcpStream>, reply: &HttpReply) {
    let reason = match reply.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        429 => "Too Many Requests",
        _ => "Service Unavailable",
    };
    write!(
        stream,
        "HTTP/1.1 {} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.body.len()
    )
    .expect("write Linear response headers");
    for (name, value) in &reply.headers {
        write!(stream, "{name}: {value}\r\n").expect("write Linear response header");
    }
    stream.write_all(b"\r\n").expect("end Linear headers");
    stream
        .write_all(&reply.body)
        .expect("write Linear response body");
    stream.flush().expect("flush Linear response");
}

fn outbound_request(
    config: &LinearConnectorConfig,
    operation_key: &str,
    operation_name: &str,
    payload: &Value,
    retry_policy: RetryPolicy,
    enqueued_at: u64,
) -> OutboundRequest {
    OutboundRequest::try_new(
        config.integration_id().clone(),
        tenant_scope(),
        IntegrationOperationKey::derive(operation_key).expect("operation key"),
        operation_name,
        serde_json::to_vec(payload).expect("operation JSON"),
        retry_policy,
        enqueued_at,
    )
    .expect("outbound request")
}

fn integration_lease(tail: char) -> IntegrationLeaseId {
    IntegrationLeaseId::try_new(id("igl", tail)).expect("lease id")
}

fn connector_for_fixture(
    fixture: &TlsLinearFixture,
    clock: u64,
) -> LinearEnterpriseConnector<CredentialFixture, MapperFixture, FixedClock> {
    let config = base_config(
        fixture.endpoint.clone(),
        LinearTlsRoots::Specific(vec![fixture.certificate_der.clone()]),
    );
    LinearEnterpriseConnector::try_new(
        config,
        CredentialFixture::full(),
        MapperFixture::default(),
        FixedClock(clock),
    )
    .expect("connector")
}

fn issue_page(nodes: &Value, has_next_page: bool, end_cursor: Option<&str>) -> HttpReply {
    HttpReply::json(
        200,
        &json!({"data": {"team": {"issues": {
            "nodes": nodes,
            "pageInfo": {"hasNextPage": has_next_page, "endCursor": end_cursor}
        }}}}),
    )
}

#[test]
fn issue_creation_paginates_and_uses_one_stable_idempotency_marker() {
    let fixture = TlsLinearFixture::start(vec![
        issue_page(&json!([]), true, Some("next-page")),
        issue_page(&json!([]), false, None),
        HttpReply::json(
            200,
            &json!({"data": {"issueCreate": {"success": true, "issue": {"id": ISSUE_UUID}}}}),
        ),
    ]);
    let root = temporary_directory("issue-create");
    let config = base_config(
        fixture.endpoint.clone(),
        LinearTlsRoots::Specific(vec![fixture.certificate_der.clone()]),
    );
    let request = outbound_request(
        &config,
        "linear-create-issue",
        "linear.issue.create.v1",
        &json!({"description": "A durable issue", "state_id": STATE_UUID, "title": "WinWinCode task"}),
        RetryPolicy::try_new(3, 10, 40).expect("retry policy"),
        1_000,
    );
    let mut connector = connector_for_fixture(&fixture, 1_000);
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    register(&mut framework, &config);
    framework.enqueue_outbound(&request).expect("enqueue");
    let attempt = framework
        .deliver_next(
            &tenant_scope(),
            config.integration_id(),
            1_000,
            integration_lease('7'),
            1_005,
            &mut connector,
        )
        .expect("delivery")
        .expect("due operation");
    let OutboundAttemptResult::Delivered(receipt) = attempt else {
        panic!("expected delivered issue");
    };
    assert_eq!(
        receipt.operation().state(),
        OutboundOperationState::Delivered
    );
    assert_eq!(receipt.remote_write_performed(), Some(true));
    assert!(
        framework
            .enqueue_outbound(&request)
            .expect("replay")
            .idempotent_replay()
    );

    let requests = fixture.finish();
    assert_eq!(requests.len(), 3);
    let page_two: Value =
        serde_json::from_slice(http_request_body(&requests[1])).expect("page two");
    assert_eq!(page_two["variables"]["after"], "next-page");
    let mutation: Value =
        serde_json::from_slice(http_request_body(&requests[2])).expect("mutation");
    let body = mutation["variables"]["input"]["description"]
        .as_str()
        .expect("description");
    assert!(body.contains(&request.operation_key().digest().0));
    assert_eq!(mutation["variables"]["input"]["teamId"], TEAM_UUID);
    assert_eq!(mutation["variables"]["input"]["projectId"], PROJECT_UUID);
    let headers = String::from_utf8_lossy(&requests[2]).to_ascii_lowercase();
    assert!(headers.contains(&format!("authorization: bearer {OAUTH_TOKEN}")));
    let database = fs::read(framework.storage().database_path()).expect("database bytes");
    assert!(find_bytes(&database, OAUTH_TOKEN.as_bytes()).is_none());
    drop(framework);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn restart_after_remote_comment_write_finds_marker_without_duplicate_mutation() {
    let operation_key =
        IntegrationOperationKey::derive("linear-comment-recovery").expect("operation key");
    let marker = format!(
        "<!-- winwincode-integration:{} -->",
        operation_key.digest().0
    );
    let issue = |comments: Value| {
        json!({
            "comments": {"nodes": comments, "pageInfo": {"hasNextPage": false, "endCursor": null}},
            "id": ISSUE_UUID,
            "project": {"id": PROJECT_UUID},
            "team": {"id": TEAM_UUID}
        })
    };
    let fixture = TlsLinearFixture::start(vec![
        HttpReply::json(200, &json!({"data": {"issue": issue(json!([]))}})),
        HttpReply::json(
            200,
            &json!({"data": {"commentCreate": {"success": true, "comment": {"id": COMMENT_UUID}}}}),
        ),
        HttpReply::json(
            200,
            &json!({"data": {"issue": issue(json!([{"body": marker, "id": COMMENT_UUID}]))}}),
        ),
    ]);
    let root = temporary_directory("comment-recovery");
    let config = base_config(
        fixture.endpoint.clone(),
        LinearTlsRoots::Specific(vec![fixture.certificate_der.clone()]),
    );
    let request = outbound_request(
        &config,
        "linear-comment-recovery",
        "linear.comment.create.v1",
        &json!({"body": "Durable comment", "issue_id": ISSUE_UUID, "project_id": PROJECT_UUID, "team_id": TEAM_UUID}),
        RetryPolicy::try_new(3, 10, 40).expect("retry policy"),
        1_000,
    );
    assert_eq!(request.operation_key(), &operation_key);
    let mut connector = connector_for_fixture(&fixture, 1_000);
    let mut first = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    register(&mut first, &config);
    first.enqueue_outbound(&request).expect("enqueue");
    let abandoned = first
        .storage_mut()
        .claim_due(
            &tenant_scope(),
            config.integration_id(),
            1_000,
            integration_lease('8'),
            1_005,
        )
        .expect("claim")
        .expect("due claim");
    assert!(
        connector
            .deliver_outbound(&abandoned)
            .expect("remote write")
            .remote_write_performed()
    );
    drop(first);

    let mut restarted =
        IntegrationFramework::new(IntegrationStorage::open(&root).expect("restart store"));
    let recovered = restarted
        .storage_mut()
        .claim_due(
            &tenant_scope(),
            config.integration_id(),
            1_005,
            integration_lease('9'),
            1_010,
        )
        .expect("recovery claim")
        .expect("due recovery");
    let found = connector
        .deliver_outbound(&recovered)
        .expect("remote recovery lookup");
    assert!(!found.remote_write_performed());
    restarted
        .storage_mut()
        .record_success(&tenant_scope(), &recovered, &found, 1_006)
        .expect("record recovery");
    let requests = fixture.finish();
    let mutation_count = requests
        .iter()
        .filter(|request| {
            let body: Value = serde_json::from_slice(http_request_body(request)).expect("request");
            body["query"]
                .as_str()
                .is_some_and(|query| query.contains("mutation WinWinCodeCommentCreate"))
        })
        .count();
    assert_eq!(mutation_count, 1);
    drop(restarted);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn graphql_rate_limit_uses_provider_reset_then_permanent_failure_dead_letters() {
    let fixture = TlsLinearFixture::start(vec![
        HttpReply {
            status: 400,
            headers: vec![("X-RateLimit-Requests-Reset".to_owned(), "31000".to_owned())],
            body: serde_json::to_vec(&json!({
                "errors": [{"extensions": {"code": "RATELIMITED"}, "message": "limited"}]
            }))
            .expect("rate-limit body"),
        },
        HttpReply::json(
            200,
            &json!({"errors": [{"extensions": {"code": "BAD_USER_INPUT"}, "message": "rejected"}]}),
        ),
    ]);
    let root = temporary_directory("rate-limit");
    let config = base_config(
        fixture.endpoint.clone(),
        LinearTlsRoots::Specific(vec![fixture.certificate_der.clone()]),
    );
    let request = outbound_request(
        &config,
        "linear-rate-limited",
        "linear.issue.create.v1",
        &json!({"description": "queued", "state_id": null, "title": "Rate limited"}),
        RetryPolicy::try_new(3, 10, 40).expect("retry policy"),
        1_000,
    );
    let mut connector = connector_for_fixture(&fixture, 1_000);
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    register(&mut framework, &config);
    framework.enqueue_outbound(&request).expect("enqueue");
    let retry = framework
        .deliver_next(
            &tenant_scope(),
            config.integration_id(),
            1_000,
            integration_lease('A'),
            1_005,
            &mut connector,
        )
        .expect("rate-limit attempt")
        .expect("due operation");
    let OutboundAttemptResult::RetryScheduled(operation) = retry else {
        panic!("expected retry schedule");
    };
    assert_eq!(operation.eligible_at_millis(), 31_000);
    let terminal = framework
        .deliver_next(
            &tenant_scope(),
            config.integration_id(),
            31_000,
            integration_lease('B'),
            31_005,
            &mut connector,
        )
        .expect("permanent attempt")
        .expect("due retry");
    assert!(matches!(terminal, OutboundAttemptResult::DeadLettered(_)));
    assert_eq!(
        framework
            .outbound_operation(
                &tenant_scope(),
                config.integration_id(),
                request.operation_key(),
            )
            .expect("operation")
            .state(),
        OutboundOperationState::DeadLetter
    );
    assert_eq!(fixture.finish().len(), 2);
    drop(framework);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn oauth_unauthorized_response_revokes_connector_and_dead_letters() {
    let fixture = TlsLinearFixture::start(vec![HttpReply::json(
        401,
        &json!({"errors": [{"extensions": {"code": "UNAUTHENTICATED"}}]}),
    )]);
    let root = temporary_directory("oauth-revoked");
    let config = base_config(
        fixture.endpoint.clone(),
        LinearTlsRoots::Specific(vec![fixture.certificate_der.clone()]),
    );
    let request = outbound_request(
        &config,
        "linear-revoked-token",
        "linear.issue.create.v1",
        &json!({"description": "must stop", "state_id": null, "title": "Revoked"}),
        RetryPolicy::try_new(3, 10, 40).expect("retry policy"),
        1_000,
    );
    let mut connector = connector_for_fixture(&fixture, 1_000);
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    register(&mut framework, &config);
    framework.enqueue_outbound(&request).expect("enqueue");
    let result = framework
        .deliver_next(
            &tenant_scope(),
            config.integration_id(),
            1_000,
            integration_lease('C'),
            1_005,
            &mut connector,
        )
        .expect("revoked attempt")
        .expect("due operation");
    assert!(matches!(result, OutboundAttemptResult::DeadLettered(_)));
    assert_eq!(
        framework
            .storage()
            .authority(&tenant_scope(), config.integration_id())
            .expect("authority")
            .state(),
        ConnectorState::CredentialRevoked
    );
    assert_eq!(fixture.finish().len(), 1);
    drop(framework);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn issue_status_is_closed_scoped_and_skips_an_already_applied_state() {
    let fixture = TlsLinearFixture::start(vec![HttpReply::json(
        200,
        &json!({"data": {"issue": {
            "id": ISSUE_UUID,
            "project": {"id": PROJECT_UUID},
            "state": {"id": STATE_UUID},
            "team": {"id": TEAM_UUID}
        }}}),
    )]);
    let root = temporary_directory("status");
    let config = base_config(
        fixture.endpoint.clone(),
        LinearTlsRoots::Specific(vec![fixture.certificate_der.clone()]),
    );
    let request = outbound_request(
        &config,
        "linear-status",
        "linear.issue.status.set.v1",
        &json!({"issue_id": ISSUE_UUID, "project_id": PROJECT_UUID, "state_id": STATE_UUID, "team_id": TEAM_UUID}),
        RetryPolicy::try_new(2, 10, 20).expect("retry policy"),
        1_000,
    );
    let mut connector = connector_for_fixture(&fixture, 1_000);
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    register(&mut framework, &config);
    framework.enqueue_outbound(&request).expect("enqueue");
    let result = framework
        .deliver_next(
            &tenant_scope(),
            config.integration_id(),
            1_000,
            integration_lease('D'),
            1_005,
            &mut connector,
        )
        .expect("status attempt")
        .expect("due status");
    let OutboundAttemptResult::Delivered(receipt) = result else {
        panic!("expected delivered status");
    };
    assert_eq!(receipt.remote_write_performed(), Some(false));
    let requests = fixture.finish();
    assert_eq!(requests.len(), 1);
    let request_body: Value =
        serde_json::from_slice(http_request_body(&requests[0])).expect("query");
    assert!(
        request_body["query"]
            .as_str()
            .is_some_and(|query| query.contains("query WinWinCodeIssue"))
    );
    drop(framework);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn invalid_operation_scope_dead_letters_without_oauth_or_network() {
    let root = temporary_directory("foreign-operation");
    let config = base_config(
        "https://localhost:9/graphql".to_owned(),
        LinearTlsRoots::WebPki,
    );
    let credentials = CredentialFixture::full();
    let token_lookups = Arc::clone(&credentials.token_lookups);
    let mut connector = LinearEnterpriseConnector::try_new(
        config.clone(),
        credentials,
        MapperFixture::default(),
        FixedClock(1_000),
    )
    .expect("connector");
    let request = outbound_request(
        &config,
        "linear-foreign-operation",
        "linear.comment.create.v1",
        &json!({
            "body": "foreign",
            "issue_id": ISSUE_UUID,
            "project_id": PROJECT_UUID,
            "team_id": "99999999-9999-4999-8999-999999999999"
        }),
        RetryPolicy::try_new(2, 10, 20).expect("retry policy"),
        1_000,
    );
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    register(&mut framework, &config);
    framework.enqueue_outbound(&request).expect("enqueue");
    let result = framework
        .deliver_next(
            &tenant_scope(),
            config.integration_id(),
            1_000,
            integration_lease('E'),
            1_005,
            &mut connector,
        )
        .expect("scope attempt")
        .expect("due operation");
    assert!(matches!(result, OutboundAttemptResult::DeadLettered(_)));
    assert_eq!(token_lookups.load(Ordering::Relaxed), 0);
    drop(framework);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn config_and_closed_webhook_types_reject_unsafe_inputs() {
    for endpoint in [
        "http://api.linear.app/graphql",
        "https://token@api.linear.app/graphql",
        "https://api.linear.app/graphql?token=secret",
        "https://api.linear.app/not-graphql",
        "https:///graphql",
    ] {
        assert_eq!(
            LinearConnectorConfig::try_new(
                integration_id(),
                credential_reference_id(),
                linear_scope(),
                endpoint,
                LinearTlsRoots::WebPki,
            )
            .expect_err("invalid endpoint")
            .kind(),
            IntegrationErrorKind::Invalid
        );
    }
    assert!(LinearWebhookHeaders::try_new(WEBHOOK_UUID, "User", "0".repeat(64), 1_000,).is_err());
    assert!(LinearWebhookHeaders::try_new(WEBHOOK_UUID, "Issue", "G".repeat(64), 1_000,).is_err());
}
