// SPDX-License-Identifier: Apache-2.0

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
};

use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::{
    ServerConfig, ServerConnection, StreamOwned,
    pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
};
use serde_json::{Value, json};
use winwincode_audit::AuditScope;
use winwincode_domain::{
    CredentialReferenceId, OrganizationId, ProjectId, RepositoryId, WorkspaceId,
};
use winwincode_integration::{
    ConnectorPort, ConnectorProtocol, ConnectorRegistration, EnterpriseIntegrationId,
    IntegrationLeaseId, IntegrationOperationKey, IntegrationStorage, OutboundClaim,
    OutboundRequest, RetryPolicy, SLACK_CONNECTOR_PROTOCOL, SlackAppId, SlackBotId,
    SlackBotPermissions, SlackBotToken, SlackChannelId, SlackClock, SlackConnectorConfig,
    SlackCredentialError, SlackCredentialPort, SlackEnterpriseConnector, SlackInstallationIdentity,
    SlackRateLimitGate, SlackSigningSecret, SlackTlsRoots, SlackWebApiMethod, SlackWorkspaceId,
};

const WORKSPACE_ID: &str = "T12345678";
const APP_ID: &str = "A12345678";
const BOT_ID: &str = "B12345678";
const CHANNEL_ID: &str = "C12345678";
const BOT_TOKEN: &str = "xoxb-slack-rate-limit-fixture";
const NOW_MILLIS: u64 = 1_700_000_000_000;
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn id(prefix: &str, tail: char) -> String {
    format!("{prefix}_{}", tail.to_string().repeat(26))
}

fn temporary_directory(name: &str) -> PathBuf {
    let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-slack-shared-gate-{name}-{}-{sequence}",
        std::process::id()
    ))
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

fn installation() -> SlackInstallationIdentity {
    SlackInstallationIdentity::new(
        SlackWorkspaceId::try_new(WORKSPACE_ID).expect("workspace id"),
        SlackAppId::try_new(APP_ID).expect("app id"),
        SlackBotId::try_new(BOT_ID).expect("bot id"),
    )
}

fn config(endpoint: &str, certificate_der: Vec<u8>) -> SlackConnectorConfig {
    SlackConnectorConfig::try_new(
        integration_id(),
        credential_reference_id(),
        installation(),
        SlackChannelId::try_new(CHANNEL_ID).expect("channel id"),
        endpoint,
        "https://control-plane.example.test/base",
        SlackTlsRoots::Specific(vec![certificate_der]),
    )
    .expect("Slack config")
}

#[derive(Clone, Copy)]
struct CredentialFixture;

impl SlackCredentialPort for CredentialFixture {
    fn resolve_signing_secret(
        &mut self,
        _reference: &CredentialReferenceId,
    ) -> Result<SlackSigningSecret, SlackCredentialError> {
        Ok(SlackSigningSecret::try_new("signing-secret-fixture").expect("signing secret"))
    }

    fn resolve_bot_token(
        &mut self,
        reference: &CredentialReferenceId,
        expected_installation: &SlackInstallationIdentity,
    ) -> Result<SlackBotToken, SlackCredentialError> {
        assert_eq!(reference, &credential_reference_id());
        assert_eq!(expected_installation, &installation());
        SlackBotToken::try_new(
            BOT_TOKEN,
            installation(),
            SlackChannelId::try_new(CHANNEL_ID).expect("channel"),
            SlackBotPermissions::new(true, true),
        )
        .map_err(|_| SlackCredentialError::revoked())
    }
}

#[derive(Clone, Copy)]
struct ClockFixture;

impl SlackClock for ClockFixture {
    fn now_millis(&self) -> u64 {
        NOW_MILLIS
    }
}

fn connector(
    config: SlackConnectorConfig,
    directory: &PathBuf,
) -> SlackEnterpriseConnector<CredentialFixture, ClockFixture> {
    SlackEnterpriseConnector::try_new(
        config,
        CredentialFixture,
        SlackRateLimitGate::open(directory).expect("Slack rate-limit gate"),
        ClockFixture,
    )
    .expect("Slack connector")
}

fn register(storage: &mut IntegrationStorage) {
    storage
        .register(
            &ConnectorRegistration::try_new(
                integration_id(),
                tenant_scope(),
                ConnectorProtocol::try_new(SLACK_CONNECTOR_PROTOCOL).expect("Slack protocol"),
                credential_reference_id(),
                10,
            )
            .expect("Slack registration"),
        )
        .expect("register Slack connector");
}

fn outbound_request(identity: &str, enqueued_at: u64) -> OutboundRequest {
    OutboundRequest::try_new(
        integration_id(),
        tenant_scope(),
        IntegrationOperationKey::derive(identity).expect("operation key"),
        "slack.attention.notify",
        serde_json::to_vec(&json!({
            "body": "Review the proposed change.",
            "channelId": CHANNEL_ID,
            "expiresAtMillis": NOW_MILLIS + 60_000,
            "expectedRevision": 7,
            "interactionId": format!("interaction-{identity}"),
            "title": "Attention required",
            "workspaceId": WORKSPACE_ID,
        }))
        .expect("Slack outbound payload"),
        RetryPolicy::try_new(3, 2, 20).expect("retry policy"),
        enqueued_at,
    )
    .expect("outbound request")
}

fn claim(
    storage: &mut IntegrationStorage,
    request: &OutboundRequest,
    lease_tail: char,
    now_millis: u64,
) -> OutboundClaim {
    storage
        .enqueue_outbound(request)
        .expect("enqueue Slack call");
    storage
        .claim_due(
            &tenant_scope(),
            &integration_id(),
            now_millis,
            IntegrationLeaseId::try_new(id("igl", lease_tail)).expect("lease id"),
            now_millis + 100,
        )
        .expect("claim Slack call")
        .expect("due Slack call")
}

struct HttpReply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpReply {
    fn json(body: &Value) -> Self {
        Self {
            status: 200,
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
        let server = thread::spawn(move || serve_tls(&listener, &config, replies, &sender));
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

fn serve_tls(
    listener: &TcpListener,
    config: &ServerConfig,
    replies: Vec<HttpReply>,
    sender: &mpsc::Sender<Vec<u8>>,
) {
    for reply in replies {
        let (socket, _) = listener.accept().expect("accept Slack TLS request");
        let connection = ServerConnection::new(Arc::new(config.clone())).expect("TLS connection");
        let mut stream = StreamOwned::new(connection, socket);
        sender
            .send(read_http_request(&mut stream))
            .expect("record Slack request");
        write_http_reply(&mut stream, &reply);
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
    let reason = if reply.status == 429 {
        "Too Many Requests"
    } else {
        "OK"
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

#[test]
fn history_429_is_shared_across_connectors_and_restart_without_scope_crosstalk() {
    let fixture = TlsSlackFixture::start(vec![HttpReply::rate_limited(3)]);
    let directory = temporary_directory("429-restart");
    fs::create_dir_all(&directory).expect("create fixture directory");
    let config = config(&fixture.endpoint, fixture.certificate_der.clone());
    let mut storage = IntegrationStorage::open(&directory).expect("integration storage");
    register(&mut storage);
    let request = outbound_request("shared-rate-limit", NOW_MILLIS);
    let operation = claim(&mut storage, &request, 'A', NOW_MILLIS);

    let mut first = connector(config.clone(), &directory);
    let first_error = first
        .deliver_outbound(&operation)
        .expect_err("Slack returns 429");
    assert_eq!(first_error.code(), "SLACK_RATE_LIMITED");
    assert_eq!(first_error.retry_after_millis(), Some(3_000));
    let concurrent = (0..2)
        .map(|_| {
            let config = config.clone();
            let directory = directory.clone();
            let operation = operation.clone();
            thread::spawn(move || {
                connector(config, &directory)
                    .deliver_outbound(&operation)
                    .expect_err("concurrent connector observes shared floor")
            })
        })
        .collect::<Vec<_>>();
    for task in concurrent {
        let shared_error = task.join().expect("join concurrent Slack connector");
        assert_eq!(shared_error.retry_after_millis(), Some(3_000));
    }
    drop(first);

    let mut restarted = connector(config, &directory);
    let restart_error = restarted
        .deliver_outbound(&operation)
        .expect_err("restarted connector observes durable floor");
    assert_eq!(restart_error.retry_after_millis(), Some(3_000));
    let gate = SlackRateLimitGate::open(&directory).expect("restart rate-limit gate");
    assert_eq!(
        gate.retry_after_millis(
            &installation(),
            SlackWebApiMethod::ConversationsHistory,
            NOW_MILLIS
        )
        .expect("history floor"),
        Some(3_000)
    );
    assert_eq!(
        gate.retry_after_millis(
            &installation(),
            SlackWebApiMethod::ChatPostMessage,
            NOW_MILLIS
        )
        .expect("method isolation"),
        None
    );
    assert_eq!(
        gate.retry_after_millis(
            &foreign_installation(),
            SlackWebApiMethod::ConversationsHistory,
            NOW_MILLIS
        )
        .expect("installation isolation"),
        None
    );
    assert_eq!(fixture.finish().len(), 1);
    drop(storage);
    fs::remove_dir_all(directory).expect("remove fixture directory");
}

fn foreign_installation() -> SlackInstallationIdentity {
    SlackInstallationIdentity::new(
        SlackWorkspaceId::try_new("T87654321").expect("foreign workspace"),
        SlackAppId::try_new(APP_ID).expect("app id"),
        SlackBotId::try_new(BOT_ID).expect("bot id"),
    )
}

fn history_message(
    marker: &str,
    team_id: &str,
    app_id: &str,
    bot_id: &str,
    timestamp: &str,
) -> HttpReply {
    HttpReply::json(&json!({
        "messages": [{
            "metadata": {
                "event_payload": {
                    "app_id": app_id,
                    "bot_id": bot_id,
                    "operation_key": marker,
                    "team_id": team_id,
                },
                "event_type": "winwincode_notification",
            },
            "ts": timestamp,
        }],
        "ok": true,
        "response_metadata": {"next_cursor": ""},
    }))
}

fn post_success(timestamp: &str) -> HttpReply {
    HttpReply::json(&json!({"channel": CHANNEL_ID, "ok": true, "ts": timestamp}))
}

#[test]
fn reconciliation_rejects_foreign_team_app_and_bot_before_accepting_exact_installation() {
    let requests = [
        outbound_request("foreign-team", 100),
        outbound_request("foreign-app", 101),
        outbound_request("foreign-bot", 102),
        outbound_request("exact-installation", 103),
    ];
    let markers = requests
        .each_ref()
        .map(|request| request.operation_key().digest().0.clone());
    let fixture = TlsSlackFixture::start(vec![
        history_message(&markers[0], "T87654321", APP_ID, BOT_ID, "171.000001"),
        post_success("171.000011"),
        history_message(&markers[1], WORKSPACE_ID, "A87654321", BOT_ID, "171.000002"),
        post_success("171.000012"),
        history_message(&markers[2], WORKSPACE_ID, APP_ID, "B87654321", "171.000003"),
        post_success("171.000013"),
        history_message(&markers[3], WORKSPACE_ID, APP_ID, BOT_ID, "171.000004"),
    ]);
    let directory = temporary_directory("metadata-installation");
    fs::create_dir_all(&directory).expect("create fixture directory");
    let config = config(&fixture.endpoint, fixture.certificate_der.clone());
    let mut storage = IntegrationStorage::open(&directory).expect("integration storage");
    register(&mut storage);
    let mut connector = connector(config, &directory);

    for (index, request) in requests.iter().enumerate() {
        let claim = claim(
            &mut storage,
            request,
            char::from(b'A' + u8::try_from(index).expect("fixture index")),
            100 + u64::try_from(index).expect("fixture index"),
        );
        let receipt = connector
            .deliver_outbound(&claim)
            .expect("Slack reconciliation result");
        assert_eq!(receipt.remote_write_performed(), index < 3);
        storage
            .record_success(&tenant_scope(), &claim, &receipt, 200 + index as u64)
            .expect("record Slack success");
    }
    let captured = fixture.finish();
    assert_eq!(
        captured
            .iter()
            .filter(|request| request.starts_with(b"POST /chat.postMessage "))
            .count(),
        3
    );
    drop(storage);
    fs::remove_dir_all(directory).expect("remove fixture directory");
}
