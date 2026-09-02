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
    CredentialReferenceId, GitHubRepositorySlug, OrganizationId, ProjectId, RepositoryId,
    WorkspaceId,
};
use winwincode_integration::{
    ConnectorCallError, ConnectorCallErrorKind, ConnectorPort, ConnectorProtocol,
    ConnectorRegistration, EnterpriseIntegrationId, GITHUB_CONNECTOR_PROTOCOL, GitHubAppId,
    GitHubClock, GitHubConnectorConfig, GitHubCredentialError, GitHubCredentialPort,
    GitHubEnterpriseConnector, GitHubEventMapperPort, GitHubInboundEvent, GitHubInstallationId,
    GitHubInstallationPermissions, GitHubInstallationToken, GitHubPermission, GitHubTlsRoots,
    GitHubWebhookHeaders, GitHubWebhookRequestFactory, GitHubWebhookSecret, GitHubWebhookVerifier,
    InboundStatus, IntegrationErrorKind, IntegrationFramework, IntegrationLeaseId,
    IntegrationOperationKey, IntegrationStorage, NormalizedInboundEvent, OutboundAttemptResult,
    OutboundOperationState, OutboundRequest, RetryPolicy,
};
use winwincode_publication::{
    CredentialResolutionError, GitHubCredential, GitHubCredentialResolver,
};

const WEBHOOK_SECRET: &[u8] = b"github-webhook-secret-fixture";
const INSTALLATION_TOKEN: &str = "github-installation-token-fixture";
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn temporary_directory(name: &str) -> PathBuf {
    let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-github-connector-{name}-{}-{sequence}",
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

fn base_config(endpoint: String, roots: GitHubTlsRoots) -> GitHubConnectorConfig {
    GitHubConnectorConfig::try_new(
        integration_id(),
        credential_reference_id(),
        GitHubAppId::try_new(7).expect("app id"),
        GitHubInstallationId::try_new(8).expect("installation id"),
        GitHubRepositorySlug("acme/widget".to_owned()),
        endpoint,
        roots,
    )
    .expect("GitHub connector config")
}

fn register(framework: &mut IntegrationFramework, config: &GitHubConnectorConfig) {
    framework
        .register_connector(
            &ConnectorRegistration::try_new(
                config.integration_id().clone(),
                tenant_scope(),
                ConnectorProtocol::try_new(GITHUB_CONNECTOR_PROTOCOL).expect("protocol"),
                config.credential_reference_id().clone(),
                10,
            )
            .expect("registration"),
        )
        .expect("register GitHub connector");
}

#[derive(Clone, Copy)]
struct FixedClock(u64);

impl GitHubClock for FixedClock {
    fn now_millis(&self) -> u64 {
        self.0
    }
}

#[derive(Clone)]
struct CredentialFixture {
    permissions: GitHubInstallationPermissions,
    webhook_lookups: Arc<AtomicU64>,
    token_lookups: Arc<AtomicU64>,
}

impl CredentialFixture {
    fn full() -> Self {
        Self {
            permissions: GitHubInstallationPermissions::new(
                GitHubPermission::Write,
                GitHubPermission::Write,
                GitHubPermission::Write,
                GitHubPermission::Read,
            ),
            webhook_lookups: Arc::new(AtomicU64::new(0)),
            token_lookups: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl GitHubCredentialPort for CredentialFixture {
    fn resolve_webhook_secret(
        &mut self,
        reference: &CredentialReferenceId,
    ) -> Result<GitHubWebhookSecret, GitHubCredentialError> {
        assert_eq!(reference, &credential_reference_id());
        self.webhook_lookups.fetch_add(1, Ordering::Relaxed);
        Ok(GitHubWebhookSecret::try_new(WEBHOOK_SECRET).expect("webhook secret"))
    }

    fn resolve_installation_token(
        &mut self,
        reference: &CredentialReferenceId,
        app_id: GitHubAppId,
        installation_id: GitHubInstallationId,
    ) -> Result<GitHubInstallationToken, GitHubCredentialError> {
        assert_eq!(reference, &credential_reference_id());
        self.token_lookups.fetch_add(1, Ordering::Relaxed);
        Ok(GitHubInstallationToken::try_new(
            INSTALLATION_TOKEN,
            app_id,
            installation_id,
            GitHubRepositorySlug("acme/widget".to_owned()),
            self.permissions,
            100_000,
        )
        .expect("installation token"))
    }
}

#[derive(Clone, Default)]
struct MapperFixture(Arc<Mutex<Vec<(String, String)>>>);

impl GitHubEventMapperPort for MapperFixture {
    fn map_event(
        &mut self,
        _authority: &winwincode_integration::ConnectorAuthority,
        event: &GitHubInboundEvent<'_>,
    ) -> Result<NormalizedInboundEvent, ConnectorCallError> {
        self.0
            .lock()
            .expect("mapper calls")
            .push((event.event_type().to_owned(), event.action().to_owned()));
        NormalizedInboundEvent::try_new(
            "delivery.create",
            br#"{"command":"delivery.create"}"#.to_vec(),
        )
        .map_err(|_| {
            ConnectorCallError::try_new(ConnectorCallErrorKind::Permanent, "GITHUB_MAPPING_INVALID")
                .expect("mapping error")
        })
    }
}

fn webhook_payload(issue_id: u64, updated_at: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "action": "edited",
        "installation": {"id": 8},
        "issue": {"id": issue_id, "updated_at": updated_at},
        "repository": {"full_name": "acme/widget"}
    }))
    .expect("GitHub webhook JSON")
}

fn webhook_signature(payload: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(WEBHOOK_SECRET).expect("HMAC key");
    mac.update(payload);
    let mut signature = String::from("sha256=");
    for byte in mac.finalize().into_bytes() {
        write!(signature, "{byte:02x}").expect("write HMAC digest");
    }
    signature.into_bytes()
}

fn github_webhook(
    factory: &GitHubWebhookRequestFactory,
    delivery_id: &str,
    event_type: &str,
    payload: Vec<u8>,
    received_at: u64,
) -> winwincode_integration::InboundWebhookRequest {
    let signature = webhook_signature(&payload);
    github_webhook_with_signature(
        factory,
        delivery_id,
        event_type,
        payload,
        received_at,
        &signature,
    )
}

fn github_webhook_with_signature(
    factory: &GitHubWebhookRequestFactory,
    delivery_id: &str,
    event_type: &str,
    payload: Vec<u8>,
    received_at: u64,
    signature: &[u8],
) -> winwincode_integration::InboundWebhookRequest {
    factory
        .build(
            tenant_scope(),
            GitHubWebhookHeaders::try_new(delivery_id, event_type, signature).expect("headers"),
            payload,
            received_at,
        )
        .expect("GitHub webhook")
}

#[test]
fn signed_webhooks_are_tenant_scoped_resource_ordered_and_secret_safe() {
    let root = temporary_directory("webhooks");
    let config = base_config("https://api.github.com".to_owned(), GitHubTlsRoots::WebPki);
    let factory = GitHubWebhookRequestFactory::new(config.clone());
    let credentials = CredentialFixture::full();
    let webhook_lookups = Arc::clone(&credentials.webhook_lookups);
    let mut verifier = GitHubWebhookVerifier::new(config.clone(), credentials.clone());
    let mapper = MapperFixture::default();
    let mapper_calls = Arc::clone(&mapper.0);
    let mut connector =
        GitHubEnterpriseConnector::try_new(config.clone(), credentials, mapper, FixedClock(100))
            .expect("connector");
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    register(&mut framework, &config);

    let first_payload = webhook_payload(101, "2026-08-28T12:00:00Z");
    let first = github_webhook(&factory, "delivery-a", "issues", first_payload.clone(), 20);
    let accepted = framework
        .receive_webhook(&first, &mut verifier, &mut connector)
        .expect("accepted webhook");
    assert_eq!(accepted.status(), InboundStatus::Accepted);
    assert!(!accepted.idempotent_replay());
    assert!(
        framework
            .receive_webhook(&first, &mut verifier, &mut connector)
            .expect("exact replay")
            .idempotent_replay()
    );

    let second_payload = webhook_payload(102, "2025-08-28T12:00:00Z");
    let second = github_webhook(&factory, "delivery-b", "issues", second_payload, 21);
    assert_eq!(
        framework
            .receive_webhook(&second, &mut verifier, &mut connector)
            .expect("independent resource ordering")
            .status(),
        InboundStatus::Accepted
    );

    let stale_payload = webhook_payload(101, "2024-08-28T12:00:00Z");
    let stale = github_webhook(&factory, "delivery-c", "issues", stale_payload, 22);
    assert_eq!(
        framework
            .receive_webhook(&stale, &mut verifier, &mut connector)
            .expect("stale receipt")
            .status(),
        InboundStatus::IgnoredOutOfOrder
    );
    assert_eq!(
        framework
            .storage()
            .inbound_dispatches(&tenant_scope(), config.integration_id(), 0, 10)
            .expect("dispatches")
            .len(),
        2
    );
    assert_eq!(mapper_calls.lock().expect("mapper calls").len(), 4);
    assert_eq!(webhook_lookups.load(Ordering::Relaxed), 4);

    let bad_signature = github_webhook_with_signature(
        &factory,
        "delivery-d",
        "issues",
        webhook_payload(103, "2026-08-28T12:00:00Z"),
        23,
        b"sha256=0000000000000000000000000000000000000000000000000000000000000000",
    );
    assert_eq!(
        framework
            .receive_webhook(&bad_signature, &mut verifier, &mut connector)
            .expect_err("signature rejection")
            .kind(),
        IntegrationErrorKind::SignatureRejected
    );

    let audit_json = serde_json::to_string(
        &framework
            .storage()
            .audit_facts(&tenant_scope(), config.integration_id(), 0, 20)
            .expect("audit"),
    )
    .expect("audit JSON");
    assert!(!audit_json.contains("github-webhook-secret"));
    assert!(!audit_json.contains("installation-token"));
    let database_path = framework.storage().database_path().to_owned();
    drop(framework);
    let database = fs::read(database_path).expect("database bytes");
    assert!(find_bytes(&database, WEBHOOK_SECRET).is_none());
    assert!(find_bytes(&database, INSTALLATION_TOKEN.as_bytes()).is_none());
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn webhook_factory_has_closed_pull_review_and_check_ordering_streams() {
    let config = base_config("https://api.github.com".to_owned(), GitHubTlsRoots::WebPki);
    let factory = GitHubWebhookRequestFactory::new(config);
    let cases = [
        (
            "pull_request",
            json!({
                "action": "synchronize", "installation": {"id": 8},
                "pull_request": {"id": 201, "updated_at": "2026-08-28T12:00:00Z"},
                "repository": {"full_name": "acme/widget"}
            }),
            "pull_request:201",
        ),
        (
            "pull_request_review",
            json!({
                "action": "submitted", "installation": {"id": 8},
                "repository": {"full_name": "acme/widget"},
                "review": {"id": 202, "submitted_at": "2026-08-28T12:00:01Z"}
            }),
            "review:202",
        ),
        (
            "check_run",
            json!({
                "action": "completed", "check_run": {"id": 203, "status": "completed"},
                "installation": {"id": 8}, "repository": {"full_name": "acme/widget"}
            }),
            "check_run:203",
        ),
    ];
    for (index, (event_type, payload, ordering_key)) in cases.into_iter().enumerate() {
        let request = github_webhook(
            &factory,
            &format!("closed-event-{index}"),
            event_type,
            serde_json::to_vec(&payload).expect("event JSON"),
            30 + u64::try_from(index).expect("small index"),
        );
        assert_eq!(request.event_type(), event_type);
        assert_eq!(request.ordering_key(), ordering_key);
        assert!(request.provider_sequence() > 0);
    }
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
            body: serde_json::to_vec(&body).expect("reply JSON"),
        }
    }
}

struct TlsGitHubFixture {
    endpoint: String,
    certificate_der: Vec<u8>,
    requests: mpsc::Receiver<Vec<u8>>,
    request_count: usize,
    server: thread::JoinHandle<()>,
}

impl TlsGitHubFixture {
    fn start(replies: Vec<HttpReply>) -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["localhost".to_owned()])
                .expect("generate GitHub TLS certificate");
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], key)
            .expect("GitHub TLS server config");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind GitHub TLS fixture");
        let address = listener.local_addr().expect("GitHub TLS address");
        let request_count = replies.len();
        let (sender, requests) = mpsc::channel();
        let server = thread::spawn(move || {
            for reply in replies {
                let (socket, _) = listener.accept().expect("accept GitHub TLS request");
                let connection =
                    ServerConnection::new(Arc::new(config.clone())).expect("TLS connection");
                let mut stream = StreamOwned::new(connection, socket);
                sender
                    .send(read_http_request(&mut stream))
                    .expect("record GitHub request");
                write_http_reply(&mut stream, &reply);
            }
        });
        Self {
            endpoint: format!("https://localhost:{}/api/v3", address.port()),
            certificate_der: cert.der().to_vec(),
            requests,
            request_count,
            server,
        }
    }

    fn finish(self) -> Vec<Vec<u8>> {
        self.server.join().expect("join GitHub TLS fixture");
        (0..self.request_count)
            .map(|_| self.requests.recv().expect("captured GitHub request"))
            .collect()
    }
}

fn read_http_request(stream: &mut StreamOwned<ServerConnection, TcpStream>) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = stream.read(&mut buffer).expect("read GitHub request");
        assert_ne!(count, 0, "GitHub request closed before body");
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
    .expect("write GitHub response headers");
    for (name, value) in &reply.headers {
        write!(stream, "{name}: {value}\r\n").expect("write GitHub response header");
    }
    stream.write_all(b"\r\n").expect("end GitHub headers");
    stream
        .write_all(&reply.body)
        .expect("write GitHub response body");
    stream.flush().expect("flush GitHub response");
}

fn outbound_request(
    config: &GitHubConnectorConfig,
    operation_key: &str,
    operation_name: &str,
    payload: &Value,
    enqueued_at: u64,
) -> OutboundRequest {
    OutboundRequest::try_new(
        config.integration_id().clone(),
        tenant_scope(),
        IntegrationOperationKey::derive(operation_key).expect("operation key"),
        operation_name,
        serde_json::to_vec(&payload).expect("operation JSON"),
        RetryPolicy::try_new(3, 10, 40).expect("retry policy"),
        enqueued_at,
    )
    .expect("outbound request")
}

fn integration_lease(tail: char) -> IntegrationLeaseId {
    IntegrationLeaseId::try_new(id("igl", tail)).expect("lease id")
}

fn assert_check_run_requests(requests: &[Vec<u8>], request: &OutboundRequest) {
    assert_eq!(requests.len(), 2);
    let lookup = String::from_utf8_lossy(&requests[0]);
    let create = String::from_utf8_lossy(&requests[1]);
    assert!(lookup.starts_with("GET /api/v3/repos/acme/widget/commits/"));
    assert!(create.starts_with("POST /api/v3/repos/acme/widget/check-runs HTTP/1.1"));
    let create_lower = create.to_ascii_lowercase();
    assert!(create_lower.contains(&format!(
        "x-github-idempotency-key: {}",
        request.operation_key().digest().0
    )));
    let create_body: Value =
        serde_json::from_slice(http_request_body(&requests[1])).expect("check-run request JSON");
    assert_eq!(
        create_body.get("external_id").and_then(Value::as_str),
        Some(request.operation_key().digest().0.as_str())
    );
    assert!(create_lower.contains(&format!(
        "authorization: bearer {}",
        INSTALLATION_TOKEN.to_ascii_lowercase()
    )));
}

#[test]
fn tls_check_run_delivery_uses_stable_provider_idempotency_and_exact_replay() {
    let fixture = TlsGitHubFixture::start(vec![
        HttpReply::json(200, &json!({"check_runs": []})),
        HttpReply::json(201, &json!({"id": 77, "node_id": "check-node-77"})),
    ]);
    let root = temporary_directory("check-run");
    let config = base_config(
        fixture.endpoint.clone(),
        GitHubTlsRoots::Specific(vec![fixture.certificate_der.clone()]),
    );
    let credentials = CredentialFixture::full();
    let token_lookups = Arc::clone(&credentials.token_lookups);
    let mut connector = GitHubEnterpriseConnector::try_new(
        config.clone(),
        credentials,
        MapperFixture::default(),
        FixedClock(100),
    )
    .expect("connector");
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    register(&mut framework, &config);
    let request = outbound_request(
        &config,
        "check-run-operation",
        "github.check_run.upsert.v1",
        &json!({
            "conclusion": null,
            "details_url": "https://winwincode.local/deliveries/1",
            "head_sha": "0123456789abcdef0123456789abcdef01234567",
            "name": "WinWinCode",
            "status": "in_progress",
            "summary": "Delivery is running",
            "title": "Delivery status"
        }),
        100,
    );
    assert!(
        !framework
            .enqueue_outbound(&request)
            .expect("enqueue")
            .idempotent_replay()
    );
    let result = framework
        .deliver_next(
            &tenant_scope(),
            config.integration_id(),
            100,
            integration_lease('7'),
            105,
            &mut connector,
        )
        .expect("delivery attempt")
        .expect("due operation");
    let OutboundAttemptResult::Delivered(receipt) = result else {
        panic!("expected delivered check run");
    };
    assert_eq!(
        receipt.operation().state(),
        OutboundOperationState::Delivered
    );
    assert_eq!(receipt.remote_write_performed(), Some(true));
    assert_eq!(token_lookups.load(Ordering::Relaxed), 1);
    assert!(
        framework
            .enqueue_outbound(&request)
            .expect("enqueue replay")
            .idempotent_replay()
    );
    assert!(
        framework
            .deliver_next(
                &tenant_scope(),
                config.integration_id(),
                101,
                integration_lease('8'),
                106,
                &mut connector,
            )
            .expect("terminal replay")
            .is_none()
    );

    let requests = fixture.finish();
    assert_check_run_requests(&requests, &request);
    let database = fs::read(framework.storage().database_path()).expect("database bytes");
    assert!(find_bytes(&database, INSTALLATION_TOKEN.as_bytes()).is_none());
    drop(framework);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn lease_recovery_finds_the_remote_issue_comment_and_does_not_post_twice() {
    let operation_key =
        IntegrationOperationKey::derive("recovered-comment").expect("operation key");
    let marker = format!(
        "<!-- winwincode-integration:{} -->",
        operation_key.digest().0
    );
    let fixture = TlsGitHubFixture::start(vec![
        HttpReply::json(200, &json!([])),
        HttpReply::json(201, &json!({"id": 91, "node_id": "comment-node-91"})),
        HttpReply::json(200, &json!([{"body": marker, "id": 91} ])),
    ]);
    let root = temporary_directory("lease-recovery");
    let config = base_config(
        fixture.endpoint.clone(),
        GitHubTlsRoots::Specific(vec![fixture.certificate_der.clone()]),
    );
    let request = outbound_request(
        &config,
        "recovered-comment",
        "github.issue.comment.v1",
        &json!({"body": "one remote comment", "issue_number": 44}),
        100,
    );
    assert_eq!(request.operation_key(), &operation_key);
    let mut connector = GitHubEnterpriseConnector::try_new(
        config.clone(),
        CredentialFixture::full(),
        MapperFixture::default(),
        FixedClock(100),
    )
    .expect("connector");
    let mut first = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    register(&mut first, &config);
    first.enqueue_outbound(&request).expect("enqueue");
    let abandoned = first
        .storage_mut()
        .claim_due(
            &tenant_scope(),
            config.integration_id(),
            100,
            integration_lease('B'),
            105,
        )
        .expect("first claim")
        .expect("due first claim");
    assert!(
        connector
            .deliver_outbound(&abandoned)
            .expect("first remote write")
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
            105,
            integration_lease('C'),
            110,
        )
        .expect("recovered claim")
        .expect("due recovery");
    let found = connector
        .deliver_outbound(&recovered)
        .expect("remote lookup recovery");
    assert!(!found.remote_write_performed());
    restarted
        .storage_mut()
        .record_success(&tenant_scope(), &recovered, &found, 106)
        .expect("record recovered success");
    let requests = fixture.finish();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.starts_with(b"POST "))
            .count(),
        1
    );
    assert_eq!(
        restarted
            .outbound_operation(&tenant_scope(), config.integration_id(), &operation_key)
            .expect("terminal operation")
            .state(),
        OutboundOperationState::Delivered
    );
    drop(restarted);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn rate_limit_schedules_framework_backoff_without_duplicate_remote_write() {
    let rate_limit = TlsGitHubFixture::start(vec![HttpReply {
        status: 429,
        headers: vec![("Retry-After".to_owned(), "30".to_owned())],
        body: Vec::new(),
    }]);
    let root = temporary_directory("rate-limit");
    let config = base_config(
        rate_limit.endpoint.clone(),
        GitHubTlsRoots::Specific(vec![rate_limit.certificate_der.clone()]),
    );
    let credentials = CredentialFixture::full();
    let mut connector = GitHubEnterpriseConnector::try_new(
        config.clone(),
        credentials,
        MapperFixture::default(),
        FixedClock(100),
    )
    .expect("connector");
    let mut framework = IntegrationFramework::new(IntegrationStorage::open(&root).expect("store"));
    register(&mut framework, &config);
    let request = outbound_request(
        &config,
        "rate-limited-comment",
        "github.issue.comment.v1",
        &json!({"body": "queued update", "issue_number": 42}),
        100,
    );
    framework.enqueue_outbound(&request).expect("enqueue");
    let attempt = framework
        .deliver_next(
            &tenant_scope(),
            config.integration_id(),
            100,
            integration_lease('9'),
            105,
            &mut connector,
        )
        .expect("rate-limited attempt")
        .expect("due operation");
    let OutboundAttemptResult::RetryScheduled(operation) = attempt else {
        panic!("expected retry schedule");
    };
    assert_eq!(operation.eligible_at_millis(), 30_100);
    let rate_limit_requests = rate_limit.finish();
    assert_eq!(rate_limit_requests.len(), 1);
    assert!(
        String::from_utf8_lossy(&rate_limit_requests[0]).starts_with(
            "GET /api/v3/repos/acme/widget/issues/42/comments?per_page=100&page=1 HTTP/1.1"
        )
    );
    drop(framework);
    fs::remove_dir_all(root).expect("cleanup rate limit");
}

#[test]
fn missing_permission_dead_letters_before_any_network_call() {
    let denied_root = temporary_directory("permission-denied");
    let denied_config = base_config(
        "https://localhost:9/api/v3".to_owned(),
        GitHubTlsRoots::WebPki,
    );
    let denied_credentials = CredentialFixture {
        permissions: GitHubInstallationPermissions::new(
            GitHubPermission::Read,
            GitHubPermission::None,
            GitHubPermission::None,
            GitHubPermission::None,
        ),
        ..CredentialFixture::full()
    };
    let mut denied_connector = GitHubEnterpriseConnector::try_new(
        denied_config.clone(),
        denied_credentials,
        MapperFixture::default(),
        FixedClock(100),
    )
    .expect("permission connector");
    let mut denied = IntegrationFramework::new(
        IntegrationStorage::open(&denied_root).expect("permission store"),
    );
    register(&mut denied, &denied_config);
    let denied_request = outbound_request(
        &denied_config,
        "permission-denied-comment",
        "github.issue.comment.v1",
        &json!({"body": "must not leave process", "issue_number": 43}),
        100,
    );
    denied
        .enqueue_outbound(&denied_request)
        .expect("enqueue denied operation");
    let denied_attempt = denied
        .deliver_next(
            &tenant_scope(),
            denied_config.integration_id(),
            100,
            integration_lease('A'),
            105,
            &mut denied_connector,
        )
        .expect("permission attempt")
        .expect("due denied operation");
    assert!(matches!(
        denied_attempt,
        OutboundAttemptResult::DeadLettered(_)
    ));
    assert_eq!(
        denied
            .outbound_operation(
                &tenant_scope(),
                denied_config.integration_id(),
                denied_request.operation_key(),
            )
            .expect("denied operation")
            .state(),
        OutboundOperationState::DeadLetter
    );

    drop(denied);
    fs::remove_dir_all(denied_root).expect("cleanup permission denied");
}

#[test]
fn config_rejects_insecure_or_credential_bearing_github_endpoints() {
    for endpoint in [
        "http://github.example/api/v3",
        "https://token@github.example/api/v3",
        "https://github.example/api/v3?token=secret",
    ] {
        assert_eq!(
            GitHubConnectorConfig::try_new(
                integration_id(),
                credential_reference_id(),
                GitHubAppId::try_new(7).expect("app id"),
                GitHubInstallationId::try_new(8).expect("installation id"),
                GitHubRepositorySlug("acme/widget".to_owned()),
                endpoint,
                GitHubTlsRoots::WebPki,
            )
            .expect_err("invalid endpoint")
            .kind(),
            IntegrationErrorKind::Invalid
        );
    }
}

struct NeverPublicationCredential;

impl GitHubCredentialResolver for NeverPublicationCredential {
    fn resolve(
        &mut self,
        _reference: &CredentialReferenceId,
    ) -> Result<GitHubCredential, CredentialResolutionError> {
        panic!("Publication credential resolution is not part of adapter construction")
    }
}

#[test]
fn publication_adapter_reuses_the_same_credential_reference_and_api_boundary() {
    let config = base_config("https://api.github.com".to_owned(), GitHubTlsRoots::WebPki);
    let adapter = config
        .publication_adapter(NeverPublicationCredential)
        .expect("canonical Publication adapter");
    let _resolver = adapter.into_credential_resolver();
}
