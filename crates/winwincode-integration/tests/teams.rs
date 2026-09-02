// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use serde_json::{Value, json};
use winwincode_audit::AuditScope;
use winwincode_domain::{
    CredentialReferenceId, OrganizationId, ProjectId, RepositoryId, WorkspaceId,
};
use winwincode_integration::{
    ConnectorProtocol, ConnectorRegistration, EnterpriseIntegrationId, InboundStatus,
    IntegrationAuditKind, IntegrationErrorKind, IntegrationFramework, IntegrationLeaseId,
    IntegrationOperationKey, IntegrationStorage, MICROSOFT_TEAMS_CONNECTOR_PROTOCOL,
    OutboundAttemptResult, OutboundOperationState, OutboundRequest, RetryPolicy, TeamsChannelId,
    TeamsConnectorConfig, TeamsCredentialError, TeamsCredentialPort, TeamsEnterpriseConnector,
    TeamsGraphAccessToken, TeamsGraphCallError, TeamsGraphClientState, TeamsGraphHttpTransport,
    TeamsGraphMessageReceipt, TeamsGraphOutboundMessage, TeamsGraphTlsRoots, TeamsGraphTokenClaims,
    TeamsGraphTokenValidationError, TeamsGraphTokenValidatorPort, TeamsGraphTransportPort,
    TeamsGraphValidationChallenge, TeamsGraphWebhookRequestFactory, TeamsGraphWebhookVerifier,
    TeamsTeamId, TeamsTenantId,
};

const TENANT_ID: &str = "11111111-2222-3333-4444-555555555555";
const FOREIGN_TENANT_ID: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
const TEAM_ID: &str = "team-7";
const CHANNEL_ID: &str = "19:channel-fixture@thread.tacv2";
const CLIENT_STATE: &str = "teams-client-state-fixture-secret";
const ACCESS_TOKEN: &str = "teams-graph-access-token-fixture-secret";
const VALIDATION_TOKEN: &str = "signed-graph-notification-jwt-fixture";
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-teams-connector-{name}-{}-{sequence}",
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

fn config() -> TeamsConnectorConfig {
    TeamsConnectorConfig::try_new(
        integration_id(),
        credential_reference_id(),
        TeamsTenantId::try_new(TENANT_ID).expect("tenant id"),
        TeamsTeamId::try_new(TEAM_ID).expect("team id"),
        TeamsChannelId::try_new(CHANNEL_ID).expect("channel id"),
    )
    .expect("Teams config")
}

fn register(framework: &mut IntegrationFramework, config: &TeamsConnectorConfig) {
    framework
        .register_connector(
            &ConnectorRegistration::try_new(
                config.integration_id().clone(),
                tenant_scope(),
                ConnectorProtocol::try_new(MICROSOFT_TEAMS_CONNECTOR_PROTOCOL)
                    .expect("Teams protocol"),
                config.credential_reference_id().clone(),
                10,
            )
            .expect("Teams registration"),
        )
        .expect("register Teams connector");
}

#[derive(Clone, Default)]
struct CredentialFixture;

impl TeamsCredentialPort for CredentialFixture {
    fn resolve_webhook_client_state(
        &mut self,
        reference: &CredentialReferenceId,
    ) -> Result<TeamsGraphClientState, TeamsCredentialError> {
        assert_eq!(reference, &credential_reference_id());
        Ok(TeamsGraphClientState::try_new(CLIENT_STATE).expect("client state"))
    }

    fn resolve_access_token(
        &mut self,
        reference: &CredentialReferenceId,
        tenant_id: &TeamsTenantId,
    ) -> Result<TeamsGraphAccessToken, TeamsCredentialError> {
        assert_eq!(reference, &credential_reference_id());
        assert_eq!(tenant_id.as_str(), TENANT_ID);
        Ok(TeamsGraphAccessToken::try_new(ACCESS_TOKEN).expect("access token"))
    }
}

struct TokenValidatorFixture {
    tenant_id: TeamsTenantId,
}

impl TeamsGraphTokenValidatorPort for TokenValidatorFixture {
    fn validate_notification_token(
        &mut self,
        token: &str,
    ) -> Result<TeamsGraphTokenClaims, TeamsGraphTokenValidationError> {
        if token != VALIDATION_TOKEN {
            return Err(TeamsGraphTokenValidationError::Rejected);
        }
        TeamsGraphTokenClaims::try_new(self.tenant_id.clone(), "graph-notification-app")
            .map_err(|_| TeamsGraphTokenValidationError::Rejected)
    }
}

#[derive(Clone)]
struct SandboxTransport {
    state: Arc<Mutex<SandboxState>>,
}

#[derive(Default)]
struct SandboxState {
    outcomes: VecDeque<SandboxOutcome>,
    messages: HashMap<String, String>,
    calls: Vec<(String, Vec<u8>)>,
}

enum SandboxOutcome {
    RateLimited(u64),
    LostResponseAfterWrite,
}

impl TeamsGraphTransportPort for SandboxTransport {
    fn deliver_message(
        &mut self,
        access_token: &TeamsGraphAccessToken,
        message: &TeamsGraphOutboundMessage,
    ) -> Result<TeamsGraphMessageReceipt, TeamsGraphCallError> {
        assert_eq!(access_token.expose_to_transport(), ACCESS_TOKEN.as_bytes());
        assert_eq!(message.tenant_id().as_str(), TENANT_ID);
        assert_eq!(message.team_id().as_str(), TEAM_ID);
        assert_eq!(message.channel_id().as_str(), CHANNEL_ID);
        let key = message.operation_key().0.clone();
        let mut state = self.state.lock().expect("sandbox state");
        state
            .calls
            .push((key.clone(), message.canonical_body().to_vec()));
        if let Some(message_id) = state.messages.get(&key) {
            return TeamsGraphMessageReceipt::try_new(message_id.clone(), false).map_err(|_| {
                TeamsGraphCallError::try_new(
                    winwincode_integration::TeamsGraphCallErrorKind::Permanent,
                )
                .expect("permanent error")
            });
        }
        match state.outcomes.pop_front().expect("sandbox outcome") {
            SandboxOutcome::RateLimited(delay) => {
                Err(TeamsGraphCallError::rate_limited(delay).expect("429 error"))
            }
            SandboxOutcome::LostResponseAfterWrite => {
                state
                    .messages
                    .insert(key, "graph-message-after-lost-response".to_owned());
                Err(TeamsGraphCallError::try_new(
                    winwincode_integration::TeamsGraphCallErrorKind::Retryable,
                )
                .expect("retryable error"))
            }
        }
    }
}

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

struct TlsTeamsFixture {
    endpoint: String,
    certificate_der: Vec<u8>,
    requests: mpsc::Receiver<Vec<u8>>,
    request_count: usize,
    server: thread::JoinHandle<()>,
}

impl TlsTeamsFixture {
    fn start(replies: Vec<HttpReply>) -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".to_owned()])
                .expect("generate Teams TLS certificate");
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], key)
            .expect("Teams TLS server config");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind Teams TLS fixture");
        let address = listener.local_addr().expect("Teams TLS address");
        let request_count = replies.len();
        let (sender, requests) = mpsc::channel();
        let server = thread::spawn(move || {
            for reply in replies {
                let (socket, _) = listener.accept().expect("accept Teams TLS request");
                let connection =
                    ServerConnection::new(Arc::new(config.clone())).expect("TLS connection");
                let mut stream = StreamOwned::new(connection, socket);
                sender
                    .send(read_http_request(&mut stream))
                    .expect("record Teams request");
                write_http_reply(&mut stream, &reply);
            }
        });
        Self {
            endpoint: format!("https://localhost:{}", address.port()),
            certificate_der: cert.der().to_vec(),
            requests,
            request_count,
            server,
        }
    }

    fn finish(self) -> Vec<Vec<u8>> {
        self.server.join().expect("join Teams TLS fixture");
        (0..self.request_count)
            .map(|_| self.requests.recv().expect("captured Teams request"))
            .collect()
    }
}

fn read_http_request(stream: &mut StreamOwned<ServerConnection, TcpStream>) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4_096];
    loop {
        let count = stream.read(&mut buffer).expect("read Teams request");
        assert_ne!(count, 0, "Teams request closed before body");
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

fn write_http_reply(stream: &mut StreamOwned<ServerConnection, TcpStream>, reply: &HttpReply) {
    let reason = match reply.status {
        200 => "OK",
        201 => "Created",
        429 => "Too Many Requests",
        _ => "Unprocessable Entity",
    };
    write!(
        stream,
        "HTTP/1.1 {} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.body.len()
    )
    .expect("write Teams response headers");
    for (name, value) in &reply.headers {
        write!(stream, "{name}: {value}\r\n").expect("write Teams response header");
    }
    stream.write_all(b"\r\n").expect("end Teams headers");
    stream
        .write_all(&reply.body)
        .expect("write Teams response body");
    stream.flush().expect("flush Teams response");
}

fn http_request_body(request: &[u8]) -> &[u8] {
    let header_end = find_bytes(request, b"\r\n\r\n").expect("HTTP header terminator");
    &request[header_end + 4..]
}

fn notification_payload(
    tenant_id: &str,
    team_id: &str,
    channel_id: &str,
    action: &str,
    sequence: u64,
    expires_at_millis: u64,
) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "@odata.context": "https://graph.microsoft.com/v1.0/$metadata#Collection()",
        "validationTokens": [VALIDATION_TOKEN],
        "value": [{
            "changeType": "updated",
            "clientState": CLIENT_STATE,
            "resource": format!(
                "teams/{team_id}/channels/{channel_id}/messages/message-7"
            ),
            "resourceData": {
                "@odata.id": format!(
                    "teams/{team_id}/channels/{channel_id}/messages/message-7"
                ),
                "@odata.type": "#Microsoft.Graph.chatMessage",
                "action": action,
                "channelId": channel_id,
                "expiresAtMillis": expires_at_millis,
                "fromUserId": "entra-user-7",
                "id": "message-7",
                "interactionId": "interaction-7",
                "teamId": team_id,
            },
            "sequenceNumber": sequence,
            "subscriptionId": "subscription-7",
            "subscriptionExpirationDateTime": "2026-08-29T00:00:00Z",
            "tenantId": tenant_id,
        }]
    }))
    .expect("notification JSON")
}

fn outbound_request(
    operation_identity: &str,
    operation_name: &str,
    retry_policy: RetryPolicy,
    enqueued_at: u64,
) -> OutboundRequest {
    OutboundRequest::try_new(
        integration_id(),
        tenant_scope(),
        IntegrationOperationKey::derive(operation_identity).expect("operation key"),
        operation_name,
        serde_json::to_vec(&json!({
            "body": "Secret approval body",
            "channelId": CHANNEL_ID,
            "expiresAtMillis": 10_000,
            "interactionId": "approval-7",
            "teamId": TEAM_ID,
            "tenantId": TENANT_ID,
            "title": "Secret approval title",
        }))
        .expect("outbound payload"),
        retry_policy,
        enqueued_at,
    )
    .expect("outbound request")
}

fn lease(tail: char) -> IntegrationLeaseId {
    IntegrationLeaseId::try_new(id("igl", tail)).expect("lease id")
}

#[test]
fn graph_validation_challenge_is_exact_plain_text() {
    let response = TeamsGraphValidationChallenge::try_from_query_value("challenge%2Bvalue%2F7")
        .expect("validation challenge")
        .response();
    assert_eq!(response.content_type(), "text/plain");
    assert_eq!(response.body(), b"challenge+value/7");
    assert_eq!(
        TeamsGraphValidationChallenge::try_from_query_value("bad%0Atoken")
            .expect_err("control rejection")
            .kind(),
        IntegrationErrorKind::Invalid
    );
    assert_eq!(
        TeamsGraphValidationChallenge::try_from_query_value("bad%2")
            .expect_err("encoding rejection")
            .kind(),
        IntegrationErrorKind::Invalid
    );
}

#[test]
fn tls_graph_sandbox_delivers_scoped_adaptive_card_with_stable_marker() {
    let fixture = TlsTeamsFixture::start(vec![
        HttpReply::json(200, &json!({"value": []})),
        HttpReply::json(201, &json!({"id": "graph-message-http-7"})),
    ]);
    let directory = temporary_directory("tls-graph");
    fs::create_dir_all(&directory).expect("create directory");
    let config = config();
    let transport = TeamsGraphHttpTransport::try_new(
        fixture.endpoint.clone(),
        TeamsGraphTlsRoots::Specific(vec![fixture.certificate_der.clone()]),
    )
    .expect("Graph HTTP transport");
    let mut connector = TeamsEnterpriseConnector::new(config.clone(), CredentialFixture, transport);
    let mut framework = IntegrationFramework::new(
        IntegrationStorage::open(&directory).expect("integration storage"),
    );
    register(&mut framework, &config);
    let request = outbound_request(
        "tls-approval-operation",
        "teams.approval.notify",
        RetryPolicy::try_new(3, 2, 20).expect("retry policy"),
        100,
    );
    framework.enqueue_outbound(&request).expect("enqueue");
    let result = framework
        .deliver_next(
            &tenant_scope(),
            config.integration_id(),
            100,
            lease('G'),
            110,
            &mut connector,
        )
        .expect("Graph delivery")
        .expect("due operation");
    let OutboundAttemptResult::Delivered(receipt) = result else {
        panic!("expected delivered Graph operation");
    };
    assert_eq!(receipt.remote_write_performed(), Some(true));
    let requests = fixture.finish();
    assert_eq!(requests.len(), 2);
    let lookup = String::from_utf8_lossy(&requests[0]);
    assert!(lookup.starts_with(
        "GET /v1.0/teams/team-7/channels/19%3Achannel-fixture%40thread.tacv2/messages?$top=50 "
    ));
    let create = String::from_utf8_lossy(&requests[1]).to_ascii_lowercase();
    assert!(create.starts_with(
        "post /v1.0/teams/team-7/channels/19%3achannel-fixture%40thread.tacv2/messages "
    ));
    assert!(create.contains(&format!(
        "authorization: bearer {}",
        ACCESS_TOKEN.to_ascii_lowercase()
    )));
    let request_id = create
        .lines()
        .find_map(|line| line.strip_prefix("client-request-id: "))
        .expect("client request id");
    assert_eq!(request_id.len(), 36);
    assert_eq!(request_id.bytes().filter(|byte| *byte == b'-').count(), 4);
    let body: Value = serde_json::from_slice(http_request_body(&requests[1])).expect("Graph body");
    assert_eq!(
        body["attachments"][0]["contentType"],
        "application/vnd.microsoft.card.adaptive"
    );
    assert!(
        body["body"]["content"]
            .as_str()
            .is_some_and(|value| value.contains(request.operation_key().digest().0.as_str()))
    );
    assert!(
        body["body"]["content"]
            .as_str()
            .is_some_and(|value| value.contains("<attachment id="))
    );
    assert!(
        body["attachments"][0]["content"]
            .as_str()
            .is_some_and(|value| value.contains("approval.approve"))
    );
    drop(framework);
    fs::remove_dir_all(directory).expect("remove directory");
}

#[test]
fn production_graph_transport_rejects_insecure_endpoints_and_preserves_429_floor() {
    assert!(TeamsGraphHttpTransport::try_new(
        "http://graph.microsoft.test",
        TeamsGraphTlsRoots::WebPki,
    )
    .is_err());
    assert!(
        TeamsGraphHttpTransport::try_new(
            "https://credential@graph.microsoft.test",
            TeamsGraphTlsRoots::WebPki,
        )
        .is_err()
    );
    assert!(
        TeamsGraphHttpTransport::try_new(
            "https://graph.microsoft.test?foreign=1",
            TeamsGraphTlsRoots::WebPki,
        )
        .is_err()
    );
    assert!(
        TeamsGraphHttpTransport::try_new(
            "https://graph.microsoft.test",
            TeamsGraphTlsRoots::Specific(Vec::new()),
        )
        .is_err()
    );

    let fixture = TlsTeamsFixture::start(vec![HttpReply {
        status: 429,
        headers: vec![("Retry-After".to_owned(), "3".to_owned())],
        body: b"provider-overload".to_vec(),
    }]);
    let directory = temporary_directory("tls-rate-limit");
    fs::create_dir_all(&directory).expect("create directory");
    let config = config();
    let transport = TeamsGraphHttpTransport::try_new(
        fixture.endpoint.clone(),
        TeamsGraphTlsRoots::Specific(vec![fixture.certificate_der.clone()]),
    )
    .expect("Graph HTTP transport");
    let mut connector = TeamsEnterpriseConnector::new(config.clone(), CredentialFixture, transport);
    let mut framework = IntegrationFramework::new(
        IntegrationStorage::open(&directory).expect("integration storage"),
    );
    register(&mut framework, &config);
    framework
        .enqueue_outbound(&outbound_request(
            "tls-rate-limit-operation",
            "teams.attention.notify",
            RetryPolicy::try_new(3, 2, 20).expect("retry policy"),
            100,
        ))
        .expect("enqueue");
    let attempt = framework
        .deliver_next(
            &tenant_scope(),
            config.integration_id(),
            100,
            lease('H'),
            110,
            &mut connector,
        )
        .expect("Graph 429")
        .expect("due operation");
    let OutboundAttemptResult::RetryScheduled(operation) = attempt else {
        panic!("expected rate-limit retry");
    };
    assert_eq!(operation.eligible_at_millis(), 3_100);
    assert_eq!(fixture.finish().len(), 1);
    drop(framework);
    fs::remove_dir_all(directory).expect("remove directory");
}

#[test]
fn graph_webhook_authenticates_identity_scope_and_deterministic_expiry() {
    let directory = temporary_directory("webhook");
    fs::create_dir_all(&directory).expect("create directory");
    let config = config();
    let factory = TeamsGraphWebhookRequestFactory::new(config.clone());
    let mut framework = IntegrationFramework::new(
        IntegrationStorage::open(&directory).expect("integration storage"),
    );
    register(&mut framework, &config);
    let mut verifier = TeamsGraphWebhookVerifier::new(
        config.clone(),
        CredentialFixture,
        TokenValidatorFixture {
            tenant_id: TeamsTenantId::try_new(TENANT_ID).expect("tenant"),
        },
    );
    let sandbox = SandboxTransport {
        state: Arc::new(Mutex::new(SandboxState::default())),
    };
    let mut connector = TeamsEnterpriseConnector::new(config.clone(), CredentialFixture, sandbox);

    let payload = notification_payload(TENANT_ID, TEAM_ID, CHANNEL_ID, "approval.approve", 7, 90);
    let request = factory
        .build(tenant_scope(), payload.clone(), 100)
        .expect("Teams webhook request");
    let first = framework
        .receive_webhook(&request, &mut verifier, &mut connector)
        .expect("accepted Teams webhook");
    assert_eq!(first.status(), InboundStatus::Accepted);
    assert!(!first.idempotent_replay());
    let replay = framework
        .receive_webhook(&request, &mut verifier, &mut connector)
        .expect("exact Teams webhook replay");
    assert!(replay.idempotent_replay());

    let dispatches = framework
        .storage()
        .inbound_dispatches(&tenant_scope(), config.integration_id(), 0, 10)
        .expect("dispatches");
    assert_eq!(dispatches.len(), 1);
    assert_eq!(dispatches[0].command_name(), "teams.interaction.handle");
    let command: Value =
        serde_json::from_slice(dispatches[0].command_payload()).expect("command JSON");
    assert_eq!(command["tenantId"], TENANT_ID);
    assert_eq!(command["teamId"], TEAM_ID);
    assert_eq!(command["channelId"], CHANNEL_ID);
    assert_eq!(command["userId"], "entra-user-7");
    assert_eq!(command["disposition"], "expired");

    let foreign = notification_payload(
        FOREIGN_TENANT_ID,
        TEAM_ID,
        CHANNEL_ID,
        "approval.approve",
        8,
        200,
    );
    assert_eq!(
        factory
            .build(tenant_scope(), foreign, 101)
            .expect_err("foreign tenant")
            .kind(),
        IntegrationErrorKind::Invalid
    );

    let changed = factory
        .build(
            tenant_scope(),
            notification_payload(TENANT_ID, TEAM_ID, CHANNEL_ID, "approval.reject", 7, 90),
            100,
        )
        .expect("changed replay request");
    assert_eq!(
        framework
            .receive_webhook(&changed, &mut verifier, &mut connector)
            .expect_err("changed replay")
            .kind(),
        IntegrationErrorKind::Conflict
    );

    let audit_json = serde_json::to_string(
        &framework
            .storage()
            .audit_facts(&tenant_scope(), config.integration_id(), 0, 20)
            .expect("audit facts"),
    )
    .expect("audit JSON");
    for secret in [
        CLIENT_STATE,
        VALIDATION_TOKEN,
        "entra-user-7",
        "interaction-7",
    ] {
        assert!(!audit_json.contains(secret));
    }
    let database_path = framework.storage().database_path().to_owned();
    drop(framework);
    let database = fs::read(database_path).expect("database bytes");
    assert!(find_bytes(&database, CLIENT_STATE.as_bytes()).is_none());
    assert!(find_bytes(&database, VALIDATION_TOKEN.as_bytes()).is_none());
    fs::remove_dir_all(directory).expect("remove directory");
}

#[test]
fn invalid_graph_proofs_and_foreign_channels_create_zero_dispatches() {
    let directory = temporary_directory("invalid-webhook");
    fs::create_dir_all(&directory).expect("create directory");
    let config = config();
    let factory = TeamsGraphWebhookRequestFactory::new(config.clone());
    let mut framework = IntegrationFramework::new(
        IntegrationStorage::open(&directory).expect("integration storage"),
    );
    register(&mut framework, &config);
    let valid_payload = notification_payload(
        TENANT_ID,
        TEAM_ID,
        CHANNEL_ID,
        "attention.acknowledge",
        9,
        500,
    );
    let mut invalid_client_state: Value =
        serde_json::from_slice(&valid_payload).expect("notification JSON");
    invalid_client_state["value"][0]["clientState"] = json!("wrong-client-state");
    let invalid_client_state = factory
        .build(
            tenant_scope(),
            serde_json::to_vec(&invalid_client_state).expect("invalid client-state JSON"),
            100,
        )
        .expect("bounded request");
    assert_signature_rejected(&mut framework, &config, &invalid_client_state, TENANT_ID);

    let mut invalid_jwt: Value = serde_json::from_slice(&valid_payload).expect("notification JSON");
    invalid_jwt["validationTokens"][0] = json!("forged-notification-jwt");
    let invalid_jwt = factory
        .build(
            tenant_scope(),
            serde_json::to_vec(&invalid_jwt).expect("invalid JWT JSON"),
            101,
        )
        .expect("bounded request");
    assert_signature_rejected(&mut framework, &config, &invalid_jwt, TENANT_ID);

    let valid_request = factory
        .build(tenant_scope(), valid_payload, 102)
        .expect("valid request");
    assert_signature_rejected(&mut framework, &config, &valid_request, FOREIGN_TENANT_ID);
    assert_eq!(
        factory
            .build(
                tenant_scope(),
                notification_payload(
                    TENANT_ID,
                    "foreign-team",
                    "19:foreign-channel@thread.tacv2",
                    "attention.acknowledge",
                    10,
                    500,
                ),
                103,
            )
            .expect_err("foreign Teams resource")
            .kind(),
        IntegrationErrorKind::Invalid
    );
    assert!(
        framework
            .storage()
            .inbound_dispatches(&tenant_scope(), config.integration_id(), 0, 10)
            .expect("dispatches")
            .is_empty()
    );
    drop(framework);
    fs::remove_dir_all(directory).expect("remove directory");
}

fn assert_signature_rejected(
    framework: &mut IntegrationFramework,
    config: &TeamsConnectorConfig,
    request: &winwincode_integration::InboundWebhookRequest,
    token_tenant: &str,
) {
    let mut verifier = TeamsGraphWebhookVerifier::new(
        config.clone(),
        CredentialFixture,
        TokenValidatorFixture {
            tenant_id: TeamsTenantId::try_new(token_tenant).expect("token tenant"),
        },
    );
    let mut connector = TeamsEnterpriseConnector::new(
        config.clone(),
        CredentialFixture,
        SandboxTransport {
            state: Arc::new(Mutex::new(SandboxState::default())),
        },
    );
    assert_eq!(
        framework
            .receive_webhook(request, &mut verifier, &mut connector)
            .expect_err("signature rejection")
            .kind(),
        IntegrationErrorKind::SignatureRejected
    );
}

#[test]
fn outbound_is_idempotent_honors_429_and_dead_letters_after_restart() {
    let directory = temporary_directory("outbound");
    fs::create_dir_all(&directory).expect("create directory");
    let config = config();
    let sandbox_state = Arc::new(Mutex::new(SandboxState {
        outcomes: VecDeque::from([
            SandboxOutcome::RateLimited(7),
            SandboxOutcome::LostResponseAfterWrite,
            SandboxOutcome::RateLimited(5),
            SandboxOutcome::RateLimited(5),
        ]),
        ..SandboxState::default()
    }));
    let mut framework = IntegrationFramework::new(
        IntegrationStorage::open(&directory).expect("integration storage"),
    );
    register(&mut framework, &config);
    let request = outbound_request(
        "approval-operation-7",
        "teams.approval.notify",
        RetryPolicy::try_new(4, 2, 20).expect("retry policy"),
        100,
    );
    assert!(
        !framework
            .enqueue_outbound(&request)
            .expect("enqueue")
            .idempotent_replay()
    );
    assert!(
        framework
            .enqueue_outbound(&request)
            .expect("exact enqueue replay")
            .idempotent_replay()
    );
    let mut connector = TeamsEnterpriseConnector::new(
        config.clone(),
        CredentialFixture,
        SandboxTransport {
            state: Arc::clone(&sandbox_state),
        },
    );
    assert_rate_limit_retry(&mut framework, &config, &mut connector);
    let mut framework = restart_and_reconcile(framework, &config, &mut connector);
    assert_dead_letter_and_secret_audit(&mut framework, &config, &request, &sandbox_state);
    drop(framework);
    fs::remove_dir_all(directory).expect("remove directory");
}

fn assert_rate_limit_retry(
    framework: &mut IntegrationFramework,
    config: &TeamsConnectorConfig,
    connector: &mut TeamsEnterpriseConnector<CredentialFixture, SandboxTransport>,
) {
    let first = framework
        .deliver_next(
            &tenant_scope(),
            config.integration_id(),
            100,
            lease('A'),
            110,
            connector,
        )
        .expect("rate-limited attempt")
        .expect("due attempt");
    let OutboundAttemptResult::RetryScheduled(first) = first else {
        panic!("expected retry");
    };
    assert_eq!(first.eligible_at_millis(), 107);
}

fn restart_and_reconcile(
    framework: IntegrationFramework,
    config: &TeamsConnectorConfig,
    connector: &mut TeamsEnterpriseConnector<CredentialFixture, SandboxTransport>,
) -> IntegrationFramework {
    let database_path = framework.storage().database_path().to_owned();
    drop(framework);
    let mut framework = IntegrationFramework::new(
        IntegrationStorage::open(database_path.parent().expect("data directory"))
            .expect("restart integration storage"),
    );
    assert!(
        framework
            .deliver_next(
                &tenant_scope(),
                config.integration_id(),
                106,
                lease('B'),
                116,
                connector,
            )
            .expect("before retry")
            .is_none()
    );
    let lost_response = framework
        .deliver_next(
            &tenant_scope(),
            config.integration_id(),
            107,
            lease('C'),
            117,
            connector,
        )
        .expect("lost response attempt")
        .expect("due attempt");
    assert!(matches!(
        lost_response,
        OutboundAttemptResult::RetryScheduled(_)
    ));
    let reconciled = framework
        .deliver_next(
            &tenant_scope(),
            config.integration_id(),
            111,
            lease('D'),
            121,
            connector,
        )
        .expect("idempotent Graph reconciliation")
        .expect("due attempt");
    let OutboundAttemptResult::Delivered(delivered) = reconciled else {
        panic!("expected delivery");
    };
    assert_eq!(delivered.remote_write_performed(), Some(false));
    assert_eq!(
        delivered.operation().state(),
        OutboundOperationState::Delivered
    );
    framework
}

fn assert_dead_letter_and_secret_audit(
    framework: &mut IntegrationFramework,
    config: &TeamsConnectorConfig,
    request: &OutboundRequest,
    sandbox_state: &Arc<Mutex<SandboxState>>,
) {
    let dead_request = outbound_request(
        "attention-operation-8",
        "teams.attention.notify",
        RetryPolicy::try_new(2, 2, 20).expect("dead-letter policy"),
        120,
    );
    framework
        .enqueue_outbound(&dead_request)
        .expect("enqueue dead-letter operation");
    let retry = framework
        .deliver_next(
            &tenant_scope(),
            config.integration_id(),
            120,
            lease('E'),
            130,
            &mut TeamsEnterpriseConnector::new(
                config.clone(),
                CredentialFixture,
                SandboxTransport {
                    state: Arc::clone(sandbox_state),
                },
            ),
        )
        .expect("first dead-letter attempt")
        .expect("due attempt");
    assert!(matches!(retry, OutboundAttemptResult::RetryScheduled(_)));
    let dead = framework
        .deliver_next(
            &tenant_scope(),
            config.integration_id(),
            125,
            lease('F'),
            135,
            &mut TeamsEnterpriseConnector::new(
                config.clone(),
                CredentialFixture,
                SandboxTransport {
                    state: Arc::clone(sandbox_state),
                },
            ),
        )
        .expect("terminal dead-letter attempt")
        .expect("due attempt");
    let OutboundAttemptResult::DeadLettered(dead) = dead else {
        panic!("expected dead letter");
    };
    assert_eq!(dead.operation().state(), OutboundOperationState::DeadLetter);
    assert_changed_key_rejected(framework, request);
    assert_secret_safe_audit(framework, config, sandbox_state);
}

fn assert_changed_key_rejected(framework: &mut IntegrationFramework, request: &OutboundRequest) {
    let changed = OutboundRequest::try_new(
        integration_id(),
        tenant_scope(),
        request.operation_key().clone(),
        "teams.approval.notify",
        br#"{"body":"changed"}"#.to_vec(),
        RetryPolicy::try_new(4, 2, 20).expect("retry policy"),
        100,
    )
    .expect("changed request");
    assert_eq!(
        framework
            .enqueue_outbound(&changed)
            .expect_err("changed key reuse")
            .kind(),
        IntegrationErrorKind::Conflict
    );
}

fn assert_secret_safe_audit(
    framework: &IntegrationFramework,
    config: &TeamsConnectorConfig,
    sandbox_state: &Arc<Mutex<SandboxState>>,
) {
    let audit = framework
        .storage()
        .audit_facts(&tenant_scope(), config.integration_id(), 0, 30)
        .expect("audit facts");
    assert!(
        audit
            .iter()
            .any(|fact| fact.kind() == IntegrationAuditKind::OutboundDeadLettered)
    );
    let audit_json = serde_json::to_string(&audit).expect("audit JSON");
    for secret in [
        ACCESS_TOKEN,
        "Secret approval title",
        "Secret approval body",
    ] {
        assert!(!audit_json.contains(secret));
    }
    let database = fs::read(framework.storage().database_path()).expect("database bytes");
    assert!(find_bytes(&database, ACCESS_TOKEN.as_bytes()).is_none());
    assert_eq!(
        sandbox_state.lock().expect("sandbox state").messages.len(),
        1
    );
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
