// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
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
    ConnectorPort, ConnectorProtocol, ConnectorRegistration, EnterpriseIntegrationId,
    InboundStatus, IntegrationErrorKind, IntegrationFramework, IntegrationLeaseId,
    IntegrationOperationKey, IntegrationStorage, OutboundAttemptResult, OutboundOperationState,
    OutboundRequest, RetryPolicy, SLACK_CONNECTOR_PROTOCOL, SlackAppId, SlackBotId,
    SlackBotPermissions, SlackBotToken, SlackChannelId, SlackClock, SlackConnectorConfig,
    SlackCredentialError, SlackCredentialPort, SlackEnterpriseConnector, SlackInstallationIdentity,
    SlackRateLimitGate, SlackSigningSecret, SlackTlsRoots, SlackWebhookHeaders,
    SlackWebhookRequestFactory, SlackWebhookVerifier, SlackWorkspaceId,
};

const WORKSPACE_ID: &str = "T12345678";
const APP_ID: &str = "A12345678";
const BOT_ID: &str = "B12345678";
const CHANNEL_ID: &str = "C12345678";
const FOREIGN_CHANNEL_ID: &str = "C87654321";
const USER_ID: &str = "U12345678";
const SIGNING_SECRET: &str = "slack-signing-secret-fixture";
const BOT_TOKEN: &str = "xoxb-slack-bot-token-fixture";
const NOW_SECONDS: u64 = 1_700_000_000;
const NOW_MILLIS: u64 = NOW_SECONDS * 1_000;
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-slack-connector-{name}-{}-{sequence}",
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

fn config(api_base_url: &str, tls_roots: SlackTlsRoots) -> SlackConnectorConfig {
    SlackConnectorConfig::try_new(
        integration_id(),
        credential_reference_id(),
        installation(),
        SlackChannelId::try_new(CHANNEL_ID).expect("channel id"),
        api_base_url,
        "https://control-plane.example.test/base",
        tls_roots,
    )
    .expect("Slack config")
}

fn installation() -> SlackInstallationIdentity {
    SlackInstallationIdentity::new(
        SlackWorkspaceId::try_new(WORKSPACE_ID).expect("workspace id"),
        SlackAppId::try_new(APP_ID).expect("app id"),
        SlackBotId::try_new(BOT_ID).expect("bot id"),
    )
}

fn register(framework: &mut IntegrationFramework, config: &SlackConnectorConfig) {
    framework
        .register_connector(
            &ConnectorRegistration::try_new(
                config.integration_id().clone(),
                tenant_scope(),
                ConnectorProtocol::try_new(SLACK_CONNECTOR_PROTOCOL).expect("Slack protocol"),
                config.credential_reference_id().clone(),
                10,
            )
            .expect("Slack registration"),
        )
        .expect("register Slack connector");
}

fn slack_connector<Credentials>(
    config: SlackConnectorConfig,
    credentials: Credentials,
    directory: &PathBuf,
) -> SlackEnterpriseConnector<Credentials, ClockFixture> {
    SlackEnterpriseConnector::try_new(
        config,
        credentials,
        SlackRateLimitGate::open(directory).expect("Slack rate-limit gate"),
        ClockFixture(NOW_MILLIS),
    )
    .expect("Slack connector")
}

#[derive(Clone, Copy)]
struct CredentialFixture {
    revoked: bool,
}

impl CredentialFixture {
    const ACTIVE: Self = Self { revoked: false };
    const REVOKED: Self = Self { revoked: true };
}

impl SlackCredentialPort for CredentialFixture {
    fn resolve_signing_secret(
        &mut self,
        reference: &CredentialReferenceId,
    ) -> Result<SlackSigningSecret, SlackCredentialError> {
        assert_eq!(reference, &credential_reference_id());
        if self.revoked {
            return Err(SlackCredentialError::revoked());
        }
        Ok(SlackSigningSecret::try_new(SIGNING_SECRET).expect("signing secret"))
    }

    fn resolve_bot_token(
        &mut self,
        reference: &CredentialReferenceId,
        expected_installation: &SlackInstallationIdentity,
    ) -> Result<SlackBotToken, SlackCredentialError> {
        assert_eq!(reference, &credential_reference_id());
        assert_eq!(expected_installation, &installation());
        if self.revoked {
            return Err(SlackCredentialError::revoked());
        }
        SlackBotToken::try_new(
            BOT_TOKEN,
            installation(),
            SlackChannelId::try_new(CHANNEL_ID).expect("channel"),
            SlackBotPermissions::new(true, true),
        )
        .map_err(|_| SlackCredentialError::revoked())
    }
}

struct WrongScopeCredentialFixture;

impl SlackCredentialPort for WrongScopeCredentialFixture {
    fn resolve_signing_secret(
        &mut self,
        _reference: &CredentialReferenceId,
    ) -> Result<SlackSigningSecret, SlackCredentialError> {
        Ok(SlackSigningSecret::try_new(SIGNING_SECRET).expect("signing secret"))
    }

    fn resolve_bot_token(
        &mut self,
        _reference: &CredentialReferenceId,
        _installation: &SlackInstallationIdentity,
    ) -> Result<SlackBotToken, SlackCredentialError> {
        SlackBotToken::try_new(
            BOT_TOKEN,
            installation(),
            SlackChannelId::try_new(FOREIGN_CHANNEL_ID).expect("foreign channel"),
            SlackBotPermissions::new(true, true),
        )
        .map_err(|_| SlackCredentialError::revoked())
    }
}

struct PanicBotCredentialFixture;

impl SlackCredentialPort for PanicBotCredentialFixture {
    fn resolve_signing_secret(
        &mut self,
        _reference: &CredentialReferenceId,
    ) -> Result<SlackSigningSecret, SlackCredentialError> {
        panic!("Block Kit rejection must precede credential resolution")
    }

    fn resolve_bot_token(
        &mut self,
        _reference: &CredentialReferenceId,
        _installation: &SlackInstallationIdentity,
    ) -> Result<SlackBotToken, SlackCredentialError> {
        panic!("Block Kit rejection must precede credential resolution")
    }
}

#[derive(Clone, Copy)]
struct ClockFixture(u64);

impl SlackClock for ClockFixture {
    fn now_millis(&self) -> u64 {
        self.0
    }
}

fn percent_encode(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(output, "%{byte:02X}").expect("write encoded byte");
        }
    }
    output
}

struct InteractionFacts<'a> {
    channel: &'a str,
    action: &'a str,
    action_timestamp: &'a str,
    expected_revision: u64,
    expires_at_millis: u64,
}

fn interaction_form(facts: &InteractionFacts<'_>) -> Vec<u8> {
    let value = json!({
        "action": facts.action,
        "expiresAtMillis": facts.expires_at_millis,
        "expectedRevision": facts.expected_revision,
        "interactionId": "interaction-7",
    });
    let interaction = json!({
        "actions": [{
            "action_id": facts.action,
            "action_ts": facts.action_timestamp,
            "value": serde_json::to_string(&value).expect("action value"),
        }],
        "api_app_id": APP_ID,
        "channel": {"id": facts.channel},
        "container": {"channel_id": facts.channel, "message_ts": "1700000000.000001"},
        "team": {"id": WORKSPACE_ID},
        "type": "block_actions",
        "user": {"id": USER_ID},
    });
    format!(
        "payload={}",
        percent_encode(&serde_json::to_string(&interaction).expect("interaction JSON"))
    )
    .into_bytes()
}

fn open_interaction_form(action_timestamp: &str, value: &str) -> Vec<u8> {
    let interaction = json!({
        "actions": [{
            "action_id": "control-plane.open",
            "action_ts": action_timestamp,
            "value": value,
        }],
        "api_app_id": APP_ID,
        "channel": {"id": CHANNEL_ID},
        "container": {"channel_id": CHANNEL_ID, "message_ts": "1700000000.000001"},
        "team": {"id": WORKSPACE_ID},
        "type": "block_actions",
        "user": {"id": USER_ID},
    });
    format!(
        "payload={}",
        percent_encode(&serde_json::to_string(&interaction).expect("open interaction JSON"))
    )
    .into_bytes()
}

fn signed_headers(body: &[u8], timestamp_seconds: u64) -> SlackWebhookHeaders {
    let mut mac = Hmac::<Sha256>::new_from_slice(SIGNING_SECRET.as_bytes()).expect("HMAC key");
    mac.update(b"v0:");
    mac.update(timestamp_seconds.to_string().as_bytes());
    mac.update(b":");
    mac.update(body);
    SlackWebhookHeaders::try_new(
        timestamp_seconds,
        format!("v0={:x}", mac.finalize().into_bytes()),
    )
    .expect("Slack headers")
}

fn outbound_request(
    operation_identity: &str,
    operation_name: &str,
    retry_policy: RetryPolicy,
    enqueued_at: u64,
) -> OutboundRequest {
    outbound_request_with_text(
        operation_identity,
        operation_name,
        retry_policy,
        enqueued_at,
        "Approval required",
        "Review the proposed change.",
    )
}

fn outbound_request_with_text(
    operation_identity: &str,
    operation_name: &str,
    retry_policy: RetryPolicy,
    enqueued_at: u64,
    title: &str,
    body: &str,
) -> OutboundRequest {
    OutboundRequest::try_new(
        integration_id(),
        tenant_scope(),
        IntegrationOperationKey::derive(operation_identity).expect("operation key"),
        operation_name,
        serde_json::to_vec(&json!({
            "body": body,
            "channelId": CHANNEL_ID,
            "expiresAtMillis": NOW_MILLIS + 60_000,
            "expectedRevision": 7,
            "interactionId": "interaction-7",
            "title": title,
            "workspaceId": WORKSPACE_ID,
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

    fn rate_limited(seconds: u64) -> Self {
        Self {
            status: 429,
            headers: vec![("Retry-After".to_owned(), seconds.to_string())],
            body: br#"{"ok":false,"error":"ratelimited"}"#.to_vec(),
        }
    }
}

struct TlsSlackFixture {
    endpoint: String,
    certificate_der: Vec<u8>,
    requests: mpsc::Receiver<Vec<u8>>,
    request_count: usize,
    server: thread::JoinHandle<()>,
}

impl TlsSlackFixture {
    fn start(replies: Vec<HttpReply>) -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".to_owned()])
                .expect("generate Slack TLS certificate");
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], key)
            .expect("Slack TLS server config");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind Slack TLS fixture");
        let address = listener.local_addr().expect("Slack TLS address");
        let request_count = replies.len();
        let (sender, requests) = mpsc::channel();
        let server = thread::spawn(move || {
            for reply in replies {
                let (socket, _) = listener.accept().expect("accept Slack TLS request");
                let connection =
                    ServerConnection::new(Arc::new(config.clone())).expect("TLS connection");
                let mut stream = StreamOwned::new(connection, socket);
                sender
                    .send(read_http_request(&mut stream))
                    .expect("record Slack request");
                write_http_reply(&mut stream, &reply);
            }
        });
        Self {
            endpoint: format!("https://localhost:{}/", address.port()),
            certificate_der: cert.der().to_vec(),
            requests,
            request_count,
            server,
        }
    }

    fn finish(self) -> Vec<Vec<u8>> {
        self.server.join().expect("join Slack TLS fixture");
        (0..self.request_count)
            .map(|_| self.requests.recv().expect("captured Slack request"))
            .collect()
    }
}

fn read_http_request(stream: &mut StreamOwned<ServerConnection, TcpStream>) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4_096];
    loop {
        let count = stream.read(&mut buffer).expect("read Slack request");
        assert_ne!(count, 0, "Slack request closed before body");
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
        429 => "Too Many Requests",
        _ => "Unprocessable Entity",
    };
    write!(
        stream,
        "HTTP/1.1 {} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.body.len()
    )
    .expect("write Slack response headers");
    for (name, value) in &reply.headers {
        write!(stream, "{name}: {value}\r\n").expect("write Slack response header");
    }
    stream.write_all(b"\r\n").expect("end Slack headers");
    stream
        .write_all(&reply.body)
        .expect("write Slack response body");
    stream.flush().expect("flush Slack response");
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

#[test]
fn url_button_is_idempotently_acknowledged_empty_within_slack_budget() {
    let config = config("https://slack.example.test/api/", SlackTlsRoots::WebPki);
    let factory = SlackWebhookRequestFactory::new(config);
    let body = open_interaction_form("1700000000.000006", "interaction-7");
    let headers = signed_headers(&body, NOW_SECONDS);
    assert_eq!(
        factory
            .accept(tenant_scope(), &headers, body.clone(), 0)
            .expect_err("zero acknowledgment time")
            .kind(),
        IntegrationErrorKind::Invalid
    );
    let first = factory
        .accept(tenant_scope(), &headers, body.clone(), NOW_MILLIS)
        .expect("open callback");
    let replay = factory
        .accept(tenant_scope(), &headers, body, NOW_MILLIS)
        .expect("open callback replay");

    for acknowledgement in [first.acknowledgement(), replay.acknowledgement()] {
        assert_eq!(acknowledgement.status_code(), 200);
        assert!(acknowledgement.body().is_empty());
        assert_eq!(acknowledgement.send_by_millis(), NOW_MILLIS + 3_000);
    }
    assert!(first.decision_request().is_none());
    assert!(replay.decision_request().is_none());
    let header_debug = format!("{headers:?}");
    let ingress_debug = format!("{first:?}");
    assert!(header_debug.contains("[REDACTED]"));
    assert!(!header_debug.contains("v0="));
    assert!(!header_debug.contains(SIGNING_SECRET));
    assert!(!ingress_debug.contains("payload="));
    assert!(!ingress_debug.contains("v0="));
}

#[test]
fn signed_callbacks_are_revision_bound_expiring_replay_safe_and_secret_safe() {
    let directory = temporary_directory("callback");
    fs::create_dir_all(&directory).expect("create directory");
    let config = config("https://slack.example.test/api/", SlackTlsRoots::WebPki);
    let factory = SlackWebhookRequestFactory::new(config.clone());
    let mut framework = IntegrationFramework::new(
        IntegrationStorage::open(&directory).expect("integration storage"),
    );
    register(&mut framework, &config);
    let mut verifier = SlackWebhookVerifier::new(
        config.clone(),
        CredentialFixture::ACTIVE,
        ClockFixture(NOW_MILLIS),
    );
    let mut connector = slack_connector(config, CredentialFixture::ACTIVE, &directory);
    let facts = InteractionFacts {
        channel: CHANNEL_ID,
        action: "approval.approve",
        action_timestamp: "1700000000.000007",
        expected_revision: 7,
        expires_at_millis: NOW_MILLIS + 1,
    };
    let body = interaction_form(&facts);
    let headers = signed_headers(&body, NOW_SECONDS);
    let request = factory
        .accept(tenant_scope(), &headers, body, NOW_MILLIS)
        .expect("Slack callback")
        .into_decision_request()
        .expect("decision callback");
    let first = framework
        .receive_webhook(&request, &mut verifier, &mut connector)
        .expect("accepted callback");
    assert_eq!(first.status(), InboundStatus::Accepted);
    assert!(!first.idempotent_replay());
    drop(framework);
    let mut framework = IntegrationFramework::new(
        IntegrationStorage::open(&directory).expect("restart integration storage"),
    );
    assert!(
        framework
            .receive_webhook(&request, &mut verifier, &mut connector)
            .expect("callback replay")
            .idempotent_replay()
    );
    assert_callback_dispatch(&framework, "approval.approve", "active", 7);
    assert_changed_callback_conflicts(&factory, &mut framework, &mut verifier, &mut connector);
    assert_expired_callback(&factory, &mut framework, &mut verifier, &mut connector);
    assert_secret_safe(&framework);
    drop(framework);
    fs::remove_dir_all(directory).expect("remove directory");
}

fn assert_callback_dispatch(
    framework: &IntegrationFramework,
    expected_action: &str,
    expected_disposition: &str,
    expected_revision: u64,
) {
    let dispatches = framework
        .storage()
        .inbound_dispatches(&tenant_scope(), &integration_id(), 0, 10)
        .expect("dispatches");
    let dispatch = dispatches.last().expect("Slack dispatch");
    assert_eq!(dispatch.command_name(), "slack.interaction.handle");
    let command: Value = serde_json::from_slice(dispatch.command_payload()).expect("command JSON");
    assert_eq!(command["workspaceId"], WORKSPACE_ID);
    assert_eq!(command["channelId"], CHANNEL_ID);
    assert_eq!(command["userId"], USER_ID);
    assert_eq!(command["action"], expected_action);
    assert_eq!(command["disposition"], expected_disposition);
    assert_eq!(command["expectedRevision"], expected_revision);
}

fn assert_changed_callback_conflicts(
    factory: &SlackWebhookRequestFactory,
    framework: &mut IntegrationFramework,
    verifier: &mut SlackWebhookVerifier<CredentialFixture, ClockFixture>,
    connector: &mut SlackEnterpriseConnector<CredentialFixture, ClockFixture>,
) {
    let changed_body = interaction_form(&InteractionFacts {
        channel: CHANNEL_ID,
        action: "approval.reject",
        action_timestamp: "1700000000.000007",
        expected_revision: 7,
        expires_at_millis: NOW_MILLIS + 1,
    });
    let changed = factory
        .accept(
            tenant_scope(),
            &signed_headers(&changed_body, NOW_SECONDS),
            changed_body,
            NOW_MILLIS,
        )
        .expect("changed callback")
        .into_decision_request()
        .expect("changed decision callback");
    assert_eq!(
        framework
            .receive_webhook(&changed, verifier, connector)
            .expect_err("changed replay")
            .kind(),
        IntegrationErrorKind::Conflict
    );
}

fn assert_expired_callback(
    factory: &SlackWebhookRequestFactory,
    framework: &mut IntegrationFramework,
    verifier: &mut SlackWebhookVerifier<CredentialFixture, ClockFixture>,
    connector: &mut SlackEnterpriseConnector<CredentialFixture, ClockFixture>,
) {
    let expired_body = interaction_form(&InteractionFacts {
        channel: CHANNEL_ID,
        action: "attention.acknowledge",
        action_timestamp: "1700000000.000008",
        expected_revision: 8,
        expires_at_millis: NOW_MILLIS - 1,
    });
    let expired = factory
        .accept(
            tenant_scope(),
            &signed_headers(&expired_body, NOW_SECONDS),
            expired_body,
            NOW_MILLIS,
        )
        .expect("expired callback")
        .into_decision_request()
        .expect("expired decision callback");
    framework
        .receive_webhook(&expired, verifier, connector)
        .expect("expired callback is durably classified");
    assert_callback_dispatch(framework, "attention.acknowledge", "expired", 8);
}

fn assert_secret_safe(framework: &IntegrationFramework) {
    let audit = framework
        .storage()
        .audit_facts(&tenant_scope(), &integration_id(), 0, 20)
        .expect("audit facts");
    let audit_json = serde_json::to_string(&audit).expect("audit JSON");
    assert!(!audit_json.contains(SIGNING_SECRET));
    assert!(!audit_json.contains(BOT_TOKEN));
    let database = fs::read(framework.storage().database_path()).expect("database bytes");
    assert!(find_bytes(&database, SIGNING_SECRET.as_bytes()).is_none());
    assert!(find_bytes(&database, BOT_TOKEN.as_bytes()).is_none());
    let directory = framework
        .storage()
        .database_path()
        .parent()
        .expect("integration directory");
    for entry in fs::read_dir(directory).expect("integration directory entries") {
        let path = entry.expect("integration directory entry").path();
        if path.is_file() {
            let bytes = fs::read(path).expect("integration durable file");
            assert!(find_bytes(&bytes, SIGNING_SECRET.as_bytes()).is_none());
            assert!(find_bytes(&bytes, BOT_TOKEN.as_bytes()).is_none());
        }
    }
}

#[test]
fn signature_window_revocation_and_foreign_scope_fail_before_dispatch() {
    let directory = temporary_directory("signature");
    fs::create_dir_all(&directory).expect("create directory");
    let config = config("https://slack.example.test/api/", SlackTlsRoots::WebPki);
    let factory = SlackWebhookRequestFactory::new(config.clone());
    let mut framework = IntegrationFramework::new(
        IntegrationStorage::open(&directory).expect("integration storage"),
    );
    register(&mut framework, &config);
    let body = interaction_form(&InteractionFacts {
        channel: CHANNEL_ID,
        action: "approval.approve",
        action_timestamp: "1700000000.000009",
        expected_revision: 9,
        expires_at_millis: NOW_MILLIS + 1,
    });
    let request = factory
        .accept(
            tenant_scope(),
            &signed_headers(&body, NOW_SECONDS - 301),
            body,
            NOW_MILLIS,
        )
        .expect("stale signed request")
        .into_decision_request()
        .expect("stale decision request");
    let mut connector = slack_connector(config.clone(), CredentialFixture::ACTIVE, &directory);
    assert_signature_rejection(&mut framework, &config, &request, &mut connector);
    assert_foreign_scope_rejection(&factory);
    assert_body_mutation_rejected(&factory, &mut framework, &config, &mut connector);
    assert_revoked_secret_closes_authority(&mut framework, &config, &mut connector);
    drop(framework);
    fs::remove_dir_all(directory).expect("remove directory");
}

fn assert_signature_rejection(
    framework: &mut IntegrationFramework,
    config: &SlackConnectorConfig,
    request: &winwincode_integration::InboundWebhookRequest,
    connector: &mut SlackEnterpriseConnector<CredentialFixture, ClockFixture>,
) {
    let mut verifier = SlackWebhookVerifier::new(
        config.clone(),
        CredentialFixture::ACTIVE,
        ClockFixture(NOW_MILLIS),
    );
    assert_eq!(
        framework
            .receive_webhook(request, &mut verifier, connector)
            .expect_err("stale Slack signature")
            .kind(),
        IntegrationErrorKind::SignatureRejected
    );
    assert!(
        framework
            .storage()
            .inbound_dispatches(&tenant_scope(), &integration_id(), 0, 10)
            .expect("dispatches")
            .is_empty()
    );
}

fn assert_foreign_scope_rejection(factory: &SlackWebhookRequestFactory) {
    let body = interaction_form(&InteractionFacts {
        channel: FOREIGN_CHANNEL_ID,
        action: "approval.approve",
        action_timestamp: "1700000000.000010",
        expected_revision: 10,
        expires_at_millis: NOW_MILLIS + 1,
    });
    assert_eq!(
        factory
            .accept(
                tenant_scope(),
                &signed_headers(&body, NOW_SECONDS),
                body,
                NOW_MILLIS,
            )
            .expect_err("foreign Slack channel")
            .kind(),
        IntegrationErrorKind::Invalid
    );
}

fn assert_body_mutation_rejected(
    factory: &SlackWebhookRequestFactory,
    framework: &mut IntegrationFramework,
    config: &SlackConnectorConfig,
    connector: &mut SlackEnterpriseConnector<CredentialFixture, ClockFixture>,
) {
    let signed_body = interaction_form(&InteractionFacts {
        channel: CHANNEL_ID,
        action: "approval.approve",
        action_timestamp: "1700000000.000012",
        expected_revision: 12,
        expires_at_millis: NOW_MILLIS + 1,
    });
    let changed_body = interaction_form(&InteractionFacts {
        channel: CHANNEL_ID,
        action: "approval.reject",
        action_timestamp: "1700000000.000012",
        expected_revision: 12,
        expires_at_millis: NOW_MILLIS + 1,
    });
    let forged = factory
        .accept(
            tenant_scope(),
            &signed_headers(&signed_body, NOW_SECONDS),
            changed_body,
            NOW_MILLIS,
        )
        .expect("bounded forged request")
        .into_decision_request()
        .expect("forged decision request");
    let mut verifier = SlackWebhookVerifier::new(
        config.clone(),
        CredentialFixture::ACTIVE,
        ClockFixture(NOW_MILLIS),
    );
    assert_eq!(
        framework
            .receive_webhook(&forged, &mut verifier, connector)
            .expect_err("body mutation")
            .kind(),
        IntegrationErrorKind::SignatureRejected
    );
}

fn assert_revoked_secret_closes_authority(
    framework: &mut IntegrationFramework,
    config: &SlackConnectorConfig,
    connector: &mut SlackEnterpriseConnector<CredentialFixture, ClockFixture>,
) {
    let body = interaction_form(&InteractionFacts {
        channel: CHANNEL_ID,
        action: "approval.reject",
        action_timestamp: "1700000000.000011",
        expected_revision: 11,
        expires_at_millis: NOW_MILLIS + 1,
    });
    let request = SlackWebhookRequestFactory::new(config.clone())
        .accept(
            tenant_scope(),
            &signed_headers(&body, NOW_SECONDS),
            body,
            NOW_MILLIS,
        )
        .expect("revoked request")
        .into_decision_request()
        .expect("revoked decision request");
    let mut verifier = SlackWebhookVerifier::new(
        config.clone(),
        CredentialFixture::REVOKED,
        ClockFixture(NOW_MILLIS),
    );
    assert_eq!(
        framework
            .receive_webhook(&request, &mut verifier, connector)
            .expect_err("revoked signing secret")
            .kind(),
        IntegrationErrorKind::CredentialRevoked
    );
}

#[test]
fn tls_slack_sandbox_posts_closed_block_kit_and_control_plane_deep_link() {
    let fixture = TlsSlackFixture::start(vec![
        HttpReply::json(
            200,
            &json!({"messages": [], "ok": true, "response_metadata": {"next_cursor": ""}}),
        ),
        HttpReply::json(
            200,
            &json!({"channel": CHANNEL_ID, "ok": true, "ts": "1712345678.123456"}),
        ),
    ]);
    let directory = temporary_directory("tls-post");
    fs::create_dir_all(&directory).expect("create directory");
    let config = config(
        &fixture.endpoint,
        SlackTlsRoots::Specific(vec![fixture.certificate_der.clone()]),
    );
    let mut framework = IntegrationFramework::new(
        IntegrationStorage::open(&directory).expect("integration storage"),
    );
    register(&mut framework, &config);
    let request = outbound_request(
        "slack-approval-operation",
        "slack.approval.notify",
        RetryPolicy::try_new(3, 2, 20).expect("retry policy"),
        100,
    );
    framework.enqueue_outbound(&request).expect("enqueue");
    let mut connector = slack_connector(config, CredentialFixture::ACTIVE, &directory);
    let result = framework
        .deliver_next(
            &tenant_scope(),
            &integration_id(),
            100,
            lease('A'),
            110,
            &mut connector,
        )
        .expect("Slack delivery")
        .expect("due operation");
    let OutboundAttemptResult::Delivered(receipt) = result else {
        panic!("expected delivered Slack operation");
    };
    assert_eq!(receipt.remote_write_performed(), Some(true));
    let requests = fixture.finish();
    assert_slack_http_requests(&requests, &request);
    assert_secret_safe(&framework);
    drop(framework);
    fs::remove_dir_all(directory).expect("remove directory");
}

fn assert_slack_http_requests(requests: &[Vec<u8>], request: &OutboundRequest) {
    assert_eq!(requests.len(), 2);
    let lookup = String::from_utf8_lossy(&requests[0]);
    assert!(lookup.starts_with(
        "GET /conversations.history?channel=C12345678&include_all_metadata=true&limit=100 "
    ));
    let create_headers = String::from_utf8_lossy(&requests[1]);
    assert!(create_headers.starts_with("POST /chat.postMessage "));
    assert!(create_headers.to_ascii_lowercase().contains(&format!(
        "authorization: bearer {}",
        BOT_TOKEN.to_ascii_lowercase()
    )));
    let body: Value =
        serde_json::from_slice(http_request_body(&requests[1])).expect("Slack message body");
    assert_eq!(body["channel"], CHANNEL_ID);
    assert_eq!(body["metadata"]["event_type"], "winwincode_notification");
    assert_eq!(
        body["metadata"]["event_payload"]["operation_key"],
        request.operation_key().digest().0
    );
    assert_eq!(body["metadata"]["event_payload"]["team_id"], WORKSPACE_ID);
    assert_eq!(body["metadata"]["event_payload"]["app_id"], APP_ID);
    assert_eq!(body["metadata"]["event_payload"]["bot_id"], BOT_ID);
    let client_message_id = body["client_msg_id"].as_str().expect("client message id");
    assert_eq!(client_message_id.len(), 36);
    let body_json = serde_json::to_string(&body).expect("message JSON");
    for expected in [
        "approval.approve",
        "approval.reject",
        "https://control-plane.example.test/base/interactions/interaction-7",
    ] {
        assert!(body_json.contains(expected));
    }
    assert!(!body_json.contains(SIGNING_SECRET));
    assert!(!body_json.contains(BOT_TOKEN));
}

#[test]
fn block_kit_uses_official_character_and_message_block_limits_before_network() {
    assert_exact_block_kit_limits_are_delivered();
    assert_oversized_block_kit_is_rejected_before_credentials();
}

fn assert_exact_block_kit_limits_are_delivered() {
    let fixture = TlsSlackFixture::start(vec![
        HttpReply::json(
            200,
            &json!({"messages": [], "ok": true, "response_metadata": {"next_cursor": ""}}),
        ),
        HttpReply::json(
            200,
            &json!({"channel": CHANNEL_ID, "ok": true, "ts": "1712345678.123499"}),
        ),
    ]);
    let directory = temporary_directory("block-limits-exact");
    fs::create_dir_all(&directory).expect("create directory");
    let config = config(
        &fixture.endpoint,
        SlackTlsRoots::Specific(vec![fixture.certificate_der.clone()]),
    );
    let mut framework = IntegrationFramework::new(
        IntegrationStorage::open(&directory).expect("integration storage"),
    );
    register(&mut framework, &config);
    framework
        .enqueue_outbound(&outbound_request_with_text(
            "slack-exact-block-limits",
            "slack.approval.notify",
            RetryPolicy::try_new(1, 2, 20).expect("retry policy"),
            100,
            &"界".repeat(150),
            &"节".repeat(3_000),
        ))
        .expect("enqueue exact limits");
    let mut connector = slack_connector(config, CredentialFixture::ACTIVE, &directory);
    assert!(matches!(
        framework
            .deliver_next(
                &tenant_scope(),
                &integration_id(),
                100,
                lease('G'),
                110,
                &mut connector,
            )
            .expect("exact limits")
            .expect("due exact limits"),
        OutboundAttemptResult::Delivered(_)
    ));
    let requests = fixture.finish();
    let message: Value =
        serde_json::from_slice(http_request_body(&requests[1])).expect("Slack message JSON");
    let blocks = message["blocks"].as_array().expect("message blocks");
    assert_eq!(blocks.len(), 3);
    assert!(blocks.len() <= 50);
    assert_eq!(
        blocks[0]["text"]["text"]
            .as_str()
            .expect("header text")
            .chars()
            .count(),
        150
    );
    assert_eq!(
        blocks[1]["text"]["text"]
            .as_str()
            .expect("section text")
            .chars()
            .count(),
        3_000
    );
    drop(framework);
    fs::remove_dir_all(directory).expect("remove directory");
}

fn assert_oversized_block_kit_is_rejected_before_credentials() {
    let directory = temporary_directory("block-limits-rejected");
    fs::create_dir_all(&directory).expect("create directory");
    let config = config("https://localhost:9/", SlackTlsRoots::WebPki);
    let mut framework = IntegrationFramework::new(
        IntegrationStorage::open(&directory).expect("integration storage"),
    );
    register(&mut framework, &config);
    for (identity, title, body) in [
        (
            "slack-header-too-long",
            "H".repeat(151),
            "valid section".to_owned(),
        ),
        (
            "slack-section-too-long",
            "valid header".to_owned(),
            "S".repeat(3_001),
        ),
    ] {
        framework
            .enqueue_outbound(&outbound_request_with_text(
                identity,
                "slack.approval.notify",
                RetryPolicy::try_new(1, 2, 20).expect("retry policy"),
                100,
                &title,
                &body,
            ))
            .expect("enqueue oversized Block Kit");
    }
    let mut connector = slack_connector(config, PanicBotCredentialFixture, &directory);
    for (lease_id, now) in [(lease('H'), 100), (lease('J'), 101)] {
        assert!(matches!(
            framework
                .deliver_next(
                    &tenant_scope(),
                    &integration_id(),
                    now,
                    lease_id,
                    now + 10,
                    &mut connector,
                )
                .expect("oversized rejection")
                .expect("due oversized request"),
            OutboundAttemptResult::DeadLettered(_)
        ));
    }
    drop(framework);
    fs::remove_dir_all(directory).expect("remove directory");
}

#[test]
fn lost_local_receipt_is_reconciled_by_metadata_without_a_second_post() {
    let operation = outbound_request(
        "slack-recovery-operation",
        "slack.attention.notify",
        RetryPolicy::try_new(3, 2, 20).expect("retry policy"),
        100,
    );
    let marker = operation.operation_key().digest().0.clone();
    let fixture = TlsSlackFixture::start(vec![
        HttpReply::json(
            200,
            &json!({"messages": [], "ok": true, "response_metadata": {"next_cursor": ""}}),
        ),
        HttpReply::json(
            200,
            &json!({"channel": CHANNEL_ID, "ok": true, "ts": "1712345678.123457"}),
        ),
        HttpReply::json(
            200,
            &json!({
                "messages": [{
                    "metadata": {"event_payload": {
                        "app_id": APP_ID,
                        "bot_id": BOT_ID,
                        "operation_key": marker,
                        "team_id": WORKSPACE_ID
                    }},
                    "ts": "1712345678.123457"
                }],
                "ok": true,
                "response_metadata": {"next_cursor": ""}
            }),
        ),
    ]);
    let directory = temporary_directory("lost-receipt");
    fs::create_dir_all(&directory).expect("create directory");
    let config = config(
        &fixture.endpoint,
        SlackTlsRoots::Specific(vec![fixture.certificate_der.clone()]),
    );
    let mut storage = IntegrationStorage::open(&directory).expect("integration storage");
    storage
        .register(
            &ConnectorRegistration::try_new(
                integration_id(),
                tenant_scope(),
                ConnectorProtocol::try_new(SLACK_CONNECTOR_PROTOCOL).expect("protocol"),
                credential_reference_id(),
                10,
            )
            .expect("registration"),
        )
        .expect("register");
    storage.enqueue_outbound(&operation).expect("enqueue");
    let abandoned = storage
        .claim_due(&tenant_scope(), &integration_id(), 100, lease('B'), 105)
        .expect("claim")
        .expect("due operation");
    let mut connector = slack_connector(config.clone(), CredentialFixture::ACTIVE, &directory);
    assert!(connector.deliver_outbound(&abandoned).is_ok());
    drop(storage);
    let mut restarted = IntegrationStorage::open(&directory).expect("restart storage");
    let recovered = restarted
        .claim_due(&tenant_scope(), &integration_id(), 105, lease('C'), 115)
        .expect("recovery claim")
        .expect("recovered operation");
    let receipt = connector
        .deliver_outbound(&recovered)
        .expect("metadata reconciliation");
    assert!(!receipt.remote_write_performed());
    restarted
        .record_success(&tenant_scope(), &recovered, &receipt, 106)
        .expect("record recovered success");
    let requests = fixture.finish();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.starts_with(b"POST /chat.postMessage "))
            .count(),
        1
    );
    drop(restarted);
    fs::remove_dir_all(directory).expect("remove directory");
}

#[test]
fn slack_429_floor_retries_then_dead_letters_without_remote_write() {
    let fixture = TlsSlackFixture::start(vec![HttpReply::rate_limited(3)]);
    let directory = temporary_directory("rate-limit");
    fs::create_dir_all(&directory).expect("create directory");
    let config = config(
        &fixture.endpoint,
        SlackTlsRoots::Specific(vec![fixture.certificate_der.clone()]),
    );
    let mut framework = IntegrationFramework::new(
        IntegrationStorage::open(&directory).expect("integration storage"),
    );
    register(&mut framework, &config);
    let request = outbound_request(
        "slack-rate-limit-operation",
        "slack.attention.notify",
        RetryPolicy::try_new(2, 2, 20).expect("retry policy"),
        100,
    );
    framework.enqueue_outbound(&request).expect("enqueue");
    let mut connector = slack_connector(config, CredentialFixture::ACTIVE, &directory);
    let first = framework
        .deliver_next(
            &tenant_scope(),
            &integration_id(),
            100,
            lease('D'),
            110,
            &mut connector,
        )
        .expect("first 429")
        .expect("due operation");
    let OutboundAttemptResult::RetryScheduled(retry) = first else {
        panic!("expected Slack retry");
    };
    assert_eq!(retry.eligible_at_millis(), 3_100);
    let second = framework
        .deliver_next(
            &tenant_scope(),
            &integration_id(),
            3_100,
            lease('E'),
            3_110,
            &mut connector,
        )
        .expect("second 429")
        .expect("due retry");
    assert!(matches!(second, OutboundAttemptResult::DeadLettered(_)));
    assert_eq!(
        framework
            .outbound_operation(&tenant_scope(), &integration_id(), request.operation_key())
            .expect("outbound operation")
            .state(),
        OutboundOperationState::DeadLetter
    );
    assert_eq!(fixture.finish().len(), 1);
    drop(framework);
    fs::remove_dir_all(directory).expect("remove directory");
}

#[test]
fn configuration_and_permission_scope_reject_before_network() {
    assert!(
        SlackConnectorConfig::try_new(
            integration_id(),
            credential_reference_id(),
            installation(),
            SlackChannelId::try_new(CHANNEL_ID).expect("channel"),
            "http://slack.example.test/api",
            "https://control-plane.example.test",
            SlackTlsRoots::WebPki,
        )
        .is_err()
    );
    assert!(
        SlackConnectorConfig::try_new(
            integration_id(),
            credential_reference_id(),
            installation(),
            SlackChannelId::try_new(CHANNEL_ID).expect("channel"),
            "https://credential@slack.example.test/api",
            "https://control-plane.example.test",
            SlackTlsRoots::WebPki,
        )
        .is_err()
    );
    assert!(
        SlackConnectorConfig::try_new(
            integration_id(),
            credential_reference_id(),
            installation(),
            SlackChannelId::try_new(CHANNEL_ID).expect("channel"),
            "https://slack.example.test/api",
            "https://control-plane.example.test?foreign=1",
            SlackTlsRoots::Specific(Vec::new()),
        )
        .is_err()
    );
    let wrong_token = SlackBotToken::try_new(
        BOT_TOKEN,
        installation(),
        SlackChannelId::try_new(FOREIGN_CHANNEL_ID).expect("foreign channel"),
        SlackBotPermissions::new(true, true),
    )
    .expect("wrong-scope token");
    assert!(format!("{wrong_token:?}").contains("[REDACTED]"));
    assert!(!format!("{wrong_token:?}").contains(BOT_TOKEN));
    assert_wrong_scope_token_dead_letters_before_network();
}

fn assert_wrong_scope_token_dead_letters_before_network() {
    let directory = temporary_directory("wrong-token-scope");
    fs::create_dir_all(&directory).expect("create directory");
    let config = config("https://localhost:9/", SlackTlsRoots::WebPki);
    let mut framework = IntegrationFramework::new(
        IntegrationStorage::open(&directory).expect("integration storage"),
    );
    register(&mut framework, &config);
    framework
        .enqueue_outbound(&outbound_request(
            "slack-wrong-token-scope",
            "slack.attention.notify",
            RetryPolicy::try_new(2, 2, 20).expect("retry policy"),
            100,
        ))
        .expect("enqueue");
    let mut connector = slack_connector(config, WrongScopeCredentialFixture, &directory);
    assert!(matches!(
        framework
            .deliver_next(
                &tenant_scope(),
                &integration_id(),
                100,
                lease('F'),
                110,
                &mut connector,
            )
            .expect("scope rejection")
            .expect("due operation"),
        OutboundAttemptResult::DeadLettered(_)
    ));
    drop(framework);
    fs::remove_dir_all(directory).expect("remove directory");
}
